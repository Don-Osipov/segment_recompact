//! Token accounting that matches what the model is actually sent when a session resumes.
//!
//! Measured on real twins (2026-09-23, `claude -p --resume` against Haiku 4.5 and Opus 5.5):
//! - Raw record size (serialized JSON / 4) overstated the real prompt by a median 3.6x across 62
//!   twins. Envelope fields, provenance, `toolUseResult` duplicates and unrendered attachments
//!   (`prompt_snapshot` runs ~200 KB each) never reach the model.
//! - Attachments reach the model through their `rendered` text, and only through it.
//! - Thinking carried across a compaction never costs anything (its prefix changed, so it is
//!   dropped), but thinking produced after a resume is preserved on Opus 4.5+/Fable and billed as
//!   input: in one Fable twin it was 38% of the live context (~0.65 tokens per signature char),
//!   invisible in the transcript text. So live usage cannot calibrate a chars-per-token ratio.
//! - The ratio is the tokenizer's: the same content ran 2.39 chars/token on Opus 5.5, ~2.65 on
//!   Fable 5.1 and 3.06 on Haiku 4.5 (dense content; prose runs higher).
//!
//! Hence: visible chars at the model's measured ratio for sizing a twin (twins carry no
//! thinking), plus a fixed system+tools overhead (`--overhead` overrides it), and the last usage
//! record — ground truth — for how big the live session is now.

use serde_json::Value;

use crate::{rec_type, truthy, IMAGE_EST_CHARS};

/// Chars per token for visible content when the model is unknown: the dense end of what was
/// measured, so thresholds err toward firing early.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 2.5;

/// System prompt + tool definitions when no usage record says otherwise (27.8k-32.5k measured in
/// a bare project; projects with many MCP servers run higher).
pub const DEFAULT_OVERHEAD: usize = 35_000;

/// Measured tokenizer density for visible transcript content, by model family.
pub fn chars_per_token_for(model: &str) -> f64 {
    let m = model.to_ascii_lowercase();
    if m.contains("haiku") {
        3.1
    } else if m.contains("fable") || m.contains("mythos") {
        2.65
    } else if m.contains("opus") || m.contains("sonnet") {
        2.4
    } else {
        DEFAULT_CHARS_PER_TOKEN
    }
}

/// Framing the API adds around each tool call or result (ids, names, block wrappers).
const TOOL_FRAMING_CHARS: usize = 30;

fn block_chars(b: &Value) -> usize {
    match b.get("type").and_then(|v| v.as_str()) {
        Some("text") => b.get("text").and_then(|v| v.as_str()).map_or(0, str::len),
        // Stripped before sending (measured): prior-turn reasoning costs no context.
        Some("thinking") | Some("redacted_thinking") => 0,
        Some("image") => IMAGE_EST_CHARS,
        Some("document") => 3 * IMAGE_EST_CHARS,
        Some("tool_use") => {
            TOOL_FRAMING_CHARS
                + b.get("name").and_then(|v| v.as_str()).map_or(0, str::len)
                + b.get("input")
                    .map_or(0, |i| serde_json::to_string(i).map_or(0, |s| s.len()))
        }
        Some("tool_result") => {
            TOOL_FRAMING_CHARS
                + match b.get("content") {
                    Some(Value::String(s)) => s.len(),
                    Some(Value::Array(inner)) => inner.iter().map(block_chars).sum(),
                    _ => 0,
                }
        }
        _ => serde_json::to_string(b).map_or(0, |s| s.len()),
    }
}

/// Bytes of model-visible text in one record: message content (minus thinking, images at flat
/// visual weight) and the rendered text of attachments. Everything else is envelope.
pub fn visible_chars(r: &Value) -> usize {
    match rec_type(r) {
        "user" | "assistant" => {
            if truthy(r, "isVisibleInTranscriptOnly") {
                return 0;
            }
            match r.pointer("/message/content") {
                Some(Value::String(s)) => s.len(),
                Some(Value::Array(blocks)) => blocks.iter().map(block_chars).sum(),
                _ => 0,
            }
        }
        "attachment" => match r.get("rendered") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.get("content").and_then(|c| c.as_str()))
                .map(str::len)
                .sum(),
            // Transcripts written before Claude Code stored `rendered` still sent these to the
            // model; approximate by payload. Types known never to render cost nothing.
            _ => match r.pointer("/attachment/type").and_then(|v| v.as_str()) {
                Some(t) if NEVER_RENDERED.contains(&t) => 0,
                _ => r
                    .get("attachment")
                    .map_or(0, |a| serde_json::to_string(a).map_or(0, |s| s.len())),
            },
        },
        _ => 0,
    }
}

/// Attachment types that carry bookkeeping only (observed with no `rendered` text in every
/// transcript that has the field).
const NEVER_RENDERED: &[&str] = &[
    "batching_reminder_sent",
    "command_permissions",
    "credential_org",
    "deferred_tools_record",
    "diagnostics",
    "hook_cancelled",
    "hook_non_blocking_error",
    "hook_success",
    "hook_system_message",
    "prompt_snapshot",
    "structured_output",
    "thinking_drop",
    "ultrathink_effort",
];

/// How visible characters convert to real prompt tokens for one session.
#[derive(Clone, Debug, Default)]
pub struct Calib {
    pub tokens_per_char: f64,
    /// System prompt + tool definitions of the environment the twin will be resumed into. Not
    /// measurable from history: one real session's first request carried 150k of eagerly loaded
    /// tool schemas, and grew by another 130k when more loaded mid-session.
    pub overhead: usize,
    /// Usage records seen.
    pub samples: usize,
    /// The live context size per the last usage record — what `/context` shows right now,
    /// preserved thinking included.
    pub live: Option<usize>,
    pub model: Option<String>,
}

impl Calib {
    pub fn for_model(model: Option<&str>) -> Calib {
        Calib {
            tokens_per_char: 1.0 / model.map_or(DEFAULT_CHARS_PER_TOKEN, chars_per_token_for),
            overhead: DEFAULT_OVERHEAD,
            samples: 0,
            live: None,
            model: model.map(str::to_string),
        }
    }

    pub fn tokens(&self, chars: usize) -> usize {
        (chars as f64 * self.tokens_per_char).round() as usize
    }

    /// Conversation tokens for a record set.
    pub fn conv_tokens(&self, records: &[Value]) -> usize {
        self.tokens(records.iter().map(visible_chars).sum())
    }

    /// Context tokens the records would cost as a freshly resumed session: conversation plus
    /// fixed overhead. For a compacted twin (no thinking) this is its size on resume.
    pub fn context_tokens(&self, records: &[Value]) -> usize {
        self.overhead + self.conv_tokens(records)
    }

    /// The size that decides whether to compact: the live measurement when there is one.
    pub fn current_tokens(&self, records: &[Value]) -> usize {
        self.live.unwrap_or_else(|| self.context_tokens(records))
    }

    pub fn describe(&self) -> String {
        let model = self.model.as_deref().unwrap_or("unknown model");
        let live = self
            .live
            .map(|l| format!("; live context {l} per last usage"))
            .unwrap_or_default();
        format!(
            "{:.2} chars/token for {model}, ~{} system+tools{live}",
            1.0 / self.tokens_per_char,
            self.overhead
        )
    }
}

pub(crate) fn prompt_tokens(usage: &Value) -> Option<usize> {
    let get = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let t =
        get("input_tokens") + get("cache_creation_input_tokens") + get("cache_read_input_tokens");
    (t > 0).then_some(t)
}

/// Model ratio from the table; live size from the last usage record.
pub fn calibrate(records: &[Value]) -> Calib {
    let model = records
        .iter()
        .rev()
        .filter(|r| crate::is_real_assistant(r))
        .find_map(|r| r.pointer("/message/model").and_then(|v| v.as_str()))
        .filter(|m| m.starts_with("claude-"));
    let mut c = Calib::for_model(model);
    for r in records {
        if rec_type(r) == "assistant" {
            if let Some(y) = r.pointer("/message/usage").and_then(prompt_tokens) {
                c.samples += 1;
                c.live = Some(y);
            }
        }
    }
    c
}
