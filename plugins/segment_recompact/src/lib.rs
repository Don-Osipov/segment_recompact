//! recompact — deterministic structural surgery for offline, segment-wise compaction of
//! Claude Code session `.jsonl` files. The lossy summarization is NOT done here; it is done by
//! Claude, live, between the two subcommands. This crate only:
//!   extract  — parse a session, select the active path, classify + segment, emit a worksheet
//!   assemble — rebuild a shorter, resume-compatible session from hand-written per-segment summaries
//!   verify   — structural checks on an assembled session (chain, tool pairs, user-turn fidelity)
//!
//! Invariants (see the rollback plan): the original is opened read-only and never written; the
//! output is create-new-only and lands in the same project transcript dir as the original.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

pub const TOOL_RESULT_TRUNC: usize = 1500;

pub const USAGE: &str = "usage:\n  \
recompact extract   <session.jsonl> [--out work/segments.json] [--keep K]\n  \
recompact assemble  <session.jsonl> <summaries.json> [--keep K] [--out <path>]\n  \
recompact assemble  <session.jsonl> --mode mask [--keep K] [--out <path>]\n  \
recompact verify    <assembled.jsonl> [--source <session.jsonl>]\n  \
recompact probe     <session.jsonl>\n  \
recompact rehydrate <compacted.jsonl> [ordinal]\n  \
recompact continue  <session.jsonl | sessionId> [--threshold T] [--keep K]\n                      \
[--summarize-with M [--escalate-with M2] [--escalate-above S]]\n  \
recompact shell     [sessionId] [--threshold T] [--goal G] [--auto]\n                      \
[--summarize-with M ...] (continuous self-compacting session)\n  \
recompact resume    <session.jsonl | sessionId>\n  \
recompact scan      [project-dir] [--estimate]\n\n\
  --keep K       number of most-recent segments kept verbatim (default 1)\n  \
  --mode mask    no-LLM compaction: keep every record, replace old tool-result\n                 \
payloads with placeholders (errors kept verbatim, head+tail)\n  \
  --cache <path> summary cache keyed by segment content hash: unchanged\n                 \
segments reuse their summaries on repeated recompactions\n  \
  --split <tok>  split segments over this many tokens into parts at safe\n                 \
seams, delegation boundaries first (default 20000; 0 disables).\n                 \
Pass the same value to extract and assemble\n  \
  --target <tok> plan per-unit treatments (verbatim/mask/summarize) toward\n                 \
this token budget; salience floors may exceed it, with reasons\n  \
  --plan         with --target: print the plan table and exit without\n                 \
validating or writing anything";

fn usage() -> i32 {
    eprintln!("{USAGE}");
    2
}

// ----------------------------------------------------------------------------- record predicates

pub fn rec_type(r: &Value) -> &str {
    r.get("type").and_then(|v| v.as_str()).unwrap_or("")
}
pub fn rec_uuid(r: &Value) -> Option<&str> {
    r.get("uuid").and_then(|v| v.as_str())
}
pub fn truthy(r: &Value, k: &str) -> bool {
    r.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}
fn content(r: &Value) -> Option<&Value> {
    r.pointer("/message/content")
}

/// Sentinel prefixes for user-channel records authored by the harness or other agents, not typed
/// by the human: teammate messages and background-task notifications. These records carry NO
/// distinguishing metadata (verified empirically: isMeta absent, no source field) — the sentinel
/// prefix is the only signal. Detection is anchored at the very start of the message text and
/// matches the exact harness framing, so a human QUOTING these phrases mid-message still
/// classifies as genuine.
const TEAMMATE_SENTINEL: &str = "Another Claude session sent a message:\n<teammate-message ";
const TASK_NOTIFICATION_SENTINEL: &str = "<task-notification>";

fn first_text(r: &Value) -> Option<&str> {
    match content(r) {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(a)) => a.iter().find_map(|b| {
            if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                b.get("text").and_then(|v| v.as_str())
            } else {
                None
            }
        }),
        _ => None,
    }
}

/// Agent-delivered content arriving on the user channel. In delegation-heavy sessions these are
/// the dominant "user" mass (a single teammate report can be 40KB), yet they are agent-authored
/// distillates, not human prompts — compressible with care, always recoverable via provenance.
/// Returns the kind, or None for anything human-typed or unrecognized (fail-open to genuine).
pub fn delivered_kind(r: &Value) -> Option<&'static str> {
    if rec_type(r) != "user" || truthy(r, "isMeta") || truthy(r, "isCompactSummary") {
        return None;
    }
    // A record whose content was rewritten by masking no longer carries the sentinel; the marker
    // stamped at mask time keeps the classification stable across passes.
    if let Some(k) = r.get("recompactDelivered").and_then(|v| v.as_str()) {
        return Some(match k {
            "task_notification" => "task_notification",
            _ => "teammate_message",
        });
    }
    if r.get("sourceToolAssistantUUID").is_some() {
        return None;
    }
    let t = first_text(r)?;
    if t.starts_with(TEAMMATE_SENTINEL) {
        Some("teammate_message")
    } else if t.starts_with(TASK_NOTIFICATION_SENTINEL) {
        Some("task_notification")
    } else {
        None
    }
}

/// A genuine human-authored user turn: a segment boundary, always kept verbatim.
///
/// Fail-open on retention: any user record that is not a tool result, a meta record, a compaction
/// summary, or sentinel-matched delivered content counts as genuine — including content shapes
/// this tool doesn't know (image-first turns, future block types). Misclassifying a real prompt
/// as agent activity would let a collapse silently drop it; misclassifying activity as a prompt
/// only costs compression.
pub fn is_genuine_user(r: &Value) -> bool {
    if rec_type(r) != "user" {
        return false;
    }
    if truthy(r, "isMeta") || truthy(r, "isCompactSummary") {
        return false;
    }
    if r.get("sourceToolAssistantUUID").is_some() {
        return false; // tool-result record
    }
    if delivered_kind(r).is_some() {
        return false; // agent-delivered content, not a human turn
    }
    match content(r) {
        Some(Value::String(_)) => true,
        Some(Value::Array(a)) => !a
            .iter()
            .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result")),
        _ => false,
    }
}

pub fn user_text(r: &Value) -> String {
    match content(r) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ----------------------------------------------------------------------------- active path

/// Session files are trees, not chains: retries and rewinds leave abandoned branches in the file,
/// and an auto-compaction starts a fresh chain root, leaving everything before the boundary
/// unreachable from the leaf. Resume replays only the leaf's parent chain, so off-path records
/// are invisible to the live session — carrying them into the rebuilt file would resurrect dead
/// branches and pre-compaction history.
///
/// Keeps the records on the leaf→root chain plus records that carry no uuid (they are not part of
/// the chain), preserving file order. Falls back to the full file when no leaf can be determined.
/// Returns (kept records, number of off-path records dropped).
pub fn select_active(records: Vec<Value>) -> (Vec<Value>, usize) {
    let mut by_uuid: HashMap<String, usize> = HashMap::new();
    for (i, r) in records.iter().enumerate() {
        if let Some(u) = rec_uuid(r) {
            by_uuid.insert(u.to_string(), i);
        }
    }
    let leaf: Option<String> = records
        .iter()
        .rev()
        .find(|r| rec_type(r) == "last-prompt")
        .and_then(|r| r.get("leafUuid").and_then(|v| v.as_str()))
        .filter(|u| by_uuid.contains_key(*u))
        .map(String::from)
        .or_else(|| records.iter().rev().find_map(|r| rec_uuid(r).map(String::from)));
    let Some(mut cur) = leaf else {
        return (records, 0);
    };
    let mut on_path: HashSet<usize> = HashSet::new();
    while let Some(&i) = by_uuid.get(&cur) {
        if !on_path.insert(i) {
            break; // cycle guard: malformed files must not hang us
        }
        match records[i].get("parentUuid").and_then(|v| v.as_str()) {
            Some(p) => cur = p.to_string(),
            None => break,
        }
    }
    let total = records.len();
    let kept: Vec<Value> = records
        .into_iter()
        .enumerate()
        .filter(|(i, r)| rec_uuid(r).is_none() || on_path.contains(i))
        .map(|(_, r)| r)
        .collect();
    let dropped = total - kept.len();
    (kept, dropped)
}

// ------------------------------------------------------------------------------------ segmenting

pub struct Segment {
    pub user_idx: usize,
    pub activity: Vec<usize>,
}

/// Returns (head record indices before the first user turn, segments).
pub fn segment(records: &[Value]) -> (Vec<usize>, Vec<Segment>) {
    let mut head = Vec::new();
    let mut segs: Vec<Segment> = Vec::new();
    for (i, r) in records.iter().enumerate() {
        if is_genuine_user(r) {
            segs.push(Segment {
                user_idx: i,
                activity: Vec::new(),
            });
        } else if let Some(last) = segs.last_mut() {
            last.activity.push(i);
        } else {
            head.push(i);
        }
    }
    (head, segs)
}

/// Does this segment carry real agent work (records with a uuid: assistant / tool-result / system)?
pub fn has_agent_activity(records: &[Value], seg: &Segment) -> bool {
    seg.activity.iter().any(|&i| rec_uuid(&records[i]).is_some())
}

pub struct SegPlan {
    pub kept_verbatim: bool,
    pub needs_summary: bool,
}

/// Stable identity of a run of records across recompaction passes. Envelope fields (sessionId,
/// parentUuid, usage) are rewritten by every assemble, so the hash covers only what the
/// conversation actually said: record type + message content, in order. FNV-1a, hand-rolled
/// because std's DefaultHasher is not stable across Rust versions and a cache must be.
fn content_hash_over(records: &[Value], indices: impl Iterator<Item = usize>) -> String {
    fn feed(h: &mut u64, s: &[u8]) {
        for &b in s {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for i in indices {
        let r = &records[i];
        feed(&mut h, rec_type(r).as_bytes());
        feed(&mut h, b"\x1f");
        if let Some(c) = content(r) {
            feed(&mut h, serde_json::to_string(c).unwrap_or_default().as_bytes());
        }
        feed(&mut h, b"\x1e");
    }
    format!("{h:016x}")
}

pub fn segment_content_hash(records: &[Value], seg: &Segment) -> String {
    content_hash_over(
        records,
        std::iter::once(seg.user_idx).chain(seg.activity.iter().copied()),
    )
}

/// Hash of one part of a split segment. The first part carries the user turn's content (matching
/// the whole-segment hash when a segment has a single part, so caches stay compatible).
pub fn part_content_hash(records: &[Value], seg: &Segment, part: &[usize], first: bool) -> String {
    if first {
        content_hash_over(
            records,
            std::iter::once(seg.user_idx).chain(part.iter().copied()),
        )
    } else {
        content_hash_over(records, part.iter().copied())
    }
}

// ------------------------------------------------------------------------------------ splitting

pub const DEFAULT_SPLIT_THRESHOLD: usize = 20_000;
const DELEGATION_TOOLS: &[&str] = &["Task", "Agent", "Workflow", "Skill"];

/// Split an oversized segment's activity into parts at safe seams. A seam is valid only where no
/// tool_use is awaiting its result, so a pair can never straddle parts. Parts close early at
/// delegation seams (a completed Task/Agent/Workflow/Skill result, or a delivered message — each
/// ends a self-contained unit of delegated work) and otherwise once the part exceeds its budget.
/// A segment at or under the threshold stays a single part. threshold 0 disables splitting.
pub fn split_parts(records: &[Value], seg: &Segment, threshold: usize) -> Vec<Vec<usize>> {
    let seg_tokens: usize = seg
        .activity
        .iter()
        .map(|&i| serde_json::to_string(&records[i]).map(|s| s.len()).unwrap_or(0))
        .sum::<usize>()
        / 4;
    if threshold == 0 || seg_tokens <= threshold {
        return vec![seg.activity.clone()];
    }
    let part_budget = (threshold / 2).max(1);
    let min_part = (threshold / 10).max(1);
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for &i in &seg.activity {
        if let Some(blocks) = content(&records[i]).and_then(|c| c.as_array()) {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    if let (Some(id), Some(name)) = (
                        b.get("id").and_then(|v| v.as_str()),
                        b.get("name").and_then(|v| v.as_str()),
                    ) {
                        tool_names.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }
    let mut parts: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_tokens = 0usize;
    let mut pending: HashSet<String> = HashSet::new();
    for &i in &seg.activity {
        let r = &records[i];
        let mut delegation_end = delivered_kind(r).is_some();
        if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
            for b in blocks {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("tool_use") => {
                        if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
                            pending.insert(id.to_string());
                        }
                    }
                    Some("tool_result") => {
                        if let Some(id) = b.get("tool_use_id").and_then(|v| v.as_str()) {
                            pending.remove(id);
                            if tool_names
                                .get(id)
                                .is_some_and(|n| DELEGATION_TOOLS.contains(&n.as_str()))
                            {
                                delegation_end = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        cur.push(i);
        cur_tokens += serde_json::to_string(r).map(|s| s.len()).unwrap_or(0) / 4;
        let seam_ok = pending.is_empty();
        if seam_ok
            && ((delegation_end && cur_tokens >= min_part) || cur_tokens >= part_budget)
        {
            parts.push(std::mem::take(&mut cur));
            cur_tokens = 0;
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    if parts.is_empty() {
        parts.push(Vec::new());
    }
    parts
}

/// One retention decision per segment, shared by extract and assemble so the two passes can never
/// disagree. The last `keep` segments stay verbatim. A segment whose activity carries a compaction
/// summary (isCompactSummary, or this tool's own recompactSynthetic) is pinned verbatim regardless
/// of age: those records are the only surviving carriers of what they replaced, and a hand-written
/// summary of a summary is exactly the recursive loss this tool exists to avoid.
pub fn plan(records: &[Value], segs: &[Segment], keep: usize) -> Vec<SegPlan> {
    segs.iter()
        .enumerate()
        .map(|(s, seg)| {
            let tail = s + keep >= segs.len();
            let pinned = seg.activity.iter().any(|&i| {
                truthy(&records[i], "isCompactSummary")
                    || truthy(&records[i], "recompactSynthetic")
                    || truthy(&records[i], "recompactLedger")
            });
            let kept_verbatim = tail || pinned;
            let needs_summary = has_agent_activity(records, seg) && !kept_verbatim;
            SegPlan {
                kept_verbatim,
                needs_summary,
            }
        })
        .collect()
}

// ------------------------------------------------------------------------------------------- I/O

pub fn load_jsonl(path: &Path) -> Vec<Value> {
    let mut buf = String::new();
    match fs::File::open(path).and_then(|mut f| f.read_to_string(&mut buf)) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", path.display());
            std::process::exit(1);
        }
    }
    let mut out = Vec::new();
    for (n, line) in buf.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => out.push(v),
            Err(e) => {
                eprintln!("error: line {} is not valid JSON: {e}", n + 1);
                std::process::exit(1);
            }
        }
    }
    out
}

pub fn approx_tokens(records: &[Value]) -> usize {
    records
        .iter()
        .map(|r| serde_json::to_string(r).map(|s| s.len()).unwrap_or(0))
        .sum::<usize>()
        / 4
}

pub fn truncate(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}…[+{} chars]", count - n)
    }
}

/// Head+tail truncation: build failures and assertion errors cluster at the END of output, so a
/// pure-head cut loses exactly the load-bearing lines. Ratio is documented, not ad hoc.
pub fn truncate_head_tail(s: &str, n: usize, head_ratio: f32) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    let head_n = (n as f32 * head_ratio) as usize;
    let tail_n = n.saturating_sub(head_n);
    let head: String = s.chars().take(head_n).collect();
    let tail: String = s.chars().skip(count - tail_n).collect();
    format!("{head}\n…[{} chars elided]…\n{tail}", count - n)
}

fn last_prompt_text(records: &[Value]) -> Option<String> {
    // Prefer a trailing `last-prompt` record's stored prompt; else fall back to text.
    records
        .iter()
        .rev()
        .find(|r| rec_type(r) == "last-prompt")
        .and_then(|r| r.get("lastPrompt").and_then(|v| v.as_str()).map(String::from))
}

// --------------------------------------------------------------------------------- arg plumbing

pub fn parse_opts(args: &[String]) -> (Vec<String>, Map<String, Value>) {
    const FLAGS: &[&str] = &["plan", "auto", "estimate"]; // boolean flags take no value
    let mut positional = Vec::new();
    let mut opts = Map::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(key) = a.strip_prefix("--") {
            if FLAGS.contains(&key) {
                opts.insert(key.to_string(), Value::Bool(true));
                i += 1;
            } else {
                let val = args.get(i + 1).cloned().unwrap_or_default();
                opts.insert(key.to_string(), Value::String(val));
                i += 2;
            }
        } else {
            positional.push(a.clone());
            i += 1;
        }
    }
    (positional, opts)
}

fn keep_window(opts: &Map<String, Value>) -> usize {
    opts.get("keep")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

// --------------------------------------------------------------------------------- budget planner

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Treatment {
    Verbatim,
    Mask,
    Summarize,
}

pub struct UnitPlan {
    pub key: String,
    pub seg: usize,
    pub salience: f32,
    pub treatment: Treatment,
    /// tokens under [verbatim, mask, summarize]
    pub cost: [usize; 3],
    /// non-empty when a floor limits demotion ("error": never below mask)
    pub floor: &'static str,
}

pub struct BudgetPlan {
    pub units: Vec<UnitPlan>,
    pub fixed_tokens: usize,
    pub planned_total: usize,
    pub target: usize,
    /// Summarize units with no summary available yet — the operator's work list.
    pub need_summaries: Vec<String>,
}

fn tokens_of(records: &[Value], indices: &[usize]) -> usize {
    indices
        .iter()
        .map(|&i| serde_json::to_string(&records[i]).map(|s| s.len()).unwrap_or(0))
        .sum::<usize>()
        / 4
}

fn mask_tokens_of(records: &[Value], indices: &[usize]) -> usize {
    indices
        .iter()
        .map(|&i| match mask_record(&records[i]) {
            Masked::Unchanged => serde_json::to_string(&records[i]).map(|s| s.len()).unwrap_or(0),
            Masked::Replaced(v) => serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0),
            Masked::Dropped => 0,
        })
        .sum::<usize>()
        / 4
}

fn first_word_signals_correction(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "no", "actually", "wait", "instead", "revert", "don't", "dont", "stop", "undo", "wrong",
    ];
    let first: String = text
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '\'')
        .collect::<String>()
        .to_lowercase();
    MARKERS.contains(&first.as_str())
}

/// Choose a treatment per unit so the output approaches `target` tokens while keeping what
/// matters. The budget is an objective; the floors are constraints; floors win — the output may
/// exceed the target, and the plan says exactly why. Salience is code-derived: error density,
/// future-file overlap (this tool knows the session's future — a live compactor never does), and
/// correction markers in the next human turn.
#[allow(clippy::too_many_arguments)]
pub fn plan_budget(
    records: &[Value],
    segs: &[Segment],
    plans: &[SegPlan],
    seg_parts: &[Vec<Vec<usize>>],
    seg_keys: &[Vec<String>],
    target: usize,
    allow_summarize: bool,
    summary_tokens: impl Fn(&str) -> Option<usize>,
) -> BudgetPlan {
    // Fixed cost: head + every genuine user turn + pinned/tail segments kept whole.
    let mut fixed = 0usize;
    for (s, seg) in segs.iter().enumerate() {
        fixed += tokens_of(records, &[seg.user_idx]);
        if plans[s].kept_verbatim {
            fixed += tokens_of(records, &seg.activity);
        }
    }

    // Per-unit file sets for the future-reference signal, then a reverse-scan suffix union.
    let mut unit_meta: Vec<(usize, usize)> = Vec::new(); // (seg, part)
    for (s, parts) in seg_parts.iter().enumerate() {
        if plans[s].kept_verbatim {
            continue;
        }
        for p in 0..parts.len() {
            unit_meta.push((s, p));
        }
    }
    let file_sets: Vec<HashSet<String>> = unit_meta
        .iter()
        .map(|&(s, p)| {
            segment_index(records, &seg_parts[s][p])["files"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        })
        .collect();
    let mut later: Vec<HashSet<String>> = vec![HashSet::new(); file_sets.len()];
    let mut acc: HashSet<String> = HashSet::new();
    for i in (0..file_sets.len()).rev() {
        later[i] = acc.clone();
        acc.extend(file_sets[i].iter().cloned());
    }

    let mut units: Vec<UnitPlan> = Vec::new();
    for (u, &(s, p)) in unit_meta.iter().enumerate() {
        let part = &seg_parts[s][p];
        let key = seg_keys[s][p].clone();
        let has_error = segment_index(records, part)["error_count"]
            .as_u64()
            .unwrap_or(0)
            > 0;
        let mut salience: f32 = 0.1;
        let mut floor = "";
        if has_error {
            salience += 0.4;
            floor = "error";
        }
        if !file_sets[u].is_empty() {
            let overlap = file_sets[u].intersection(&later[u]).count() as f32
                / file_sets[u].len() as f32;
            salience += 0.3 * overlap;
        }
        if let Some(next) = segs.get(s + 1) {
            if first_word_signals_correction(&user_text(&records[next.user_idx])) {
                salience += 0.3;
            }
        }
        let verbatim = tokens_of(records, part);
        let mask = mask_tokens_of(records, part).min(verbatim);
        let summary = summary_tokens(&key).unwrap_or(400).min(mask);
        units.push(UnitPlan {
            key,
            seg: s,
            salience: salience.min(1.0),
            treatment: Treatment::Verbatim,
            cost: [verbatim, mask, summary],
            floor,
        });
    }

    let mut total = fixed + units.iter().map(|u| u.cost[0]).sum::<usize>();
    loop {
        if total <= target {
            break;
        }
        let mut best: Option<(usize, Treatment, usize, f32)> = None; // (idx, next, savings, score)
        for (i, u) in units.iter().enumerate() {
            let next = match u.treatment {
                // Pure-prose units mask to zero savings; they may skip straight to Summarize,
                // or the ladder would strand them at Verbatim forever.
                Treatment::Verbatim if u.cost[1] < u.cost[0] => Treatment::Mask,
                Treatment::Verbatim if allow_summarize && u.floor != "error" => {
                    Treatment::Summarize
                }
                Treatment::Mask if allow_summarize && u.floor != "error" => Treatment::Summarize,
                _ => continue,
            };
            let cur_cost = u.cost[u.treatment as usize];
            let next_cost = u.cost[next as usize];
            let savings = cur_cost.saturating_sub(next_cost);
            if savings == 0 {
                continue;
            }
            let score = savings as f32 * (1.0 - u.salience);
            if best.map_or(true, |(_, _, _, s)| score > s) {
                best = Some((i, next, savings, score));
            }
        }
        let Some((i, next, savings, _)) = best else {
            break; // floors hold: over target and nothing left to demote
        };
        units[i].treatment = next;
        total -= savings;
    }

    let need_summaries: Vec<String> = units
        .iter()
        .filter(|u| u.treatment == Treatment::Summarize && summary_tokens(&u.key).is_none())
        .map(|u| u.key.clone())
        .collect();
    BudgetPlan {
        units,
        fixed_tokens: fixed,
        planned_total: total,
        target,
        need_summaries,
    }
}

fn print_budget_plan(b: &BudgetPlan) {
    eprintln!(
        "plan: target {} tokens, fixed {} (head + user turns + pinned/tail)",
        b.target, b.fixed_tokens
    );
    eprintln!(
        "  {:<8} {:<9} {:<10} {:>10} {:>10}  floor",
        "unit", "salience", "treatment", "verbatim", "planned"
    );
    for u in &b.units {
        eprintln!(
            "  {:<8} {:<9.2} {:<10} {:>10} {:>10}  {}",
            u.key,
            u.salience,
            format!("{:?}", u.treatment).to_lowercase(),
            u.cost[0],
            u.cost[u.treatment as usize],
            u.floor
        );
    }
    if b.planned_total > b.target {
        eprintln!(
            "plan: total {} tokens, {} OVER target — floors and fixed cost hold; this is the price of retention",
            b.planned_total,
            b.planned_total - b.target
        );
    } else {
        eprintln!(
            "plan: total {} tokens (target {})",
            b.planned_total, b.target
        );
    }
    if !b.need_summaries.is_empty() {
        eprintln!("plan: provide summaries for {:?}", b.need_summaries);
    }
}

// ----------------------------------------------------------------------------------- subcommand: extract

pub fn cmd_extract(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    if pos.is_empty() {
        return usage();
    }
    let src = PathBuf::from(&pos[0]);
    let out = opts
        .get("out")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("work/segments.json"));
    let keep = keep_window(&opts);

    let split_threshold = opts
        .get("split")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SPLIT_THRESHOLD);

    let loaded = load_jsonl(&src);
    let total_in_file = loaded.len();
    let (records, off_path) = select_active(loaded);
    let (head, segs) = segment(&records);
    let plans = plan(&records, &segs, keep);

    let mut needs_keys: Vec<String> = Vec::new();
    let mut seg_json = Vec::new();
    for (s, seg) in segs.iter().enumerate() {
        // Map tool_use ids to names segment-wide so each result can be labeled with the tool
        // that produced it (pairs never straddle parts, but the map is cheap to build once).
        let mut tool_names: HashMap<String, String> = HashMap::new();
        for &i in &seg.activity {
            if let Some(blocks) = content(&records[i]).and_then(|c| c.as_array()) {
                for b in blocks {
                    if b.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        if let (Some(id), Some(name)) = (
                            b.get("id").and_then(|v| v.as_str()),
                            b.get("name").and_then(|v| v.as_str()),
                        ) {
                            tool_names.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }
        }
        let mut seen_results: HashSet<String> = HashSet::new();
        let parts = split_parts(&records, seg, split_threshold);
        let split = parts.len() > 1;

        let mut parts_json: Vec<Value> = Vec::new();
        for (p, part) in parts.iter().enumerate() {
            let mut activity = Vec::new();
            let mut covered: Vec<Value> = Vec::new();
            if p == 0 {
                covered.push(
                    records[seg.user_idx]
                        .get("uuid")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
            }
            for &i in part {
                if let Some(u) = rec_uuid(&records[i]) {
                    covered.push(Value::String(u.to_string()));
                }
                activity.extend(render_record(&records[i], &tool_names, &mut seen_results));
            }
            let key = if split { format!("{s}.{p}") } else { s.to_string() };
            parts_json.push(json!({
                "key": key,
                "covered_uuids": covered,
                "content_hash": part_content_hash(&records, seg, part, p == 0),
                "approx_tokens": approx_tokens(
                    &part.iter().map(|&i| records[i].clone()).collect::<Vec<_>>()
                ),
                "activity": activity,
            }));
        }
        if plans[s].needs_summary {
            for pj in &parts_json {
                needs_keys.push(pj["key"].as_str().unwrap_or_default().to_string());
            }
        }

        let mut seg_obj = json!({
            "index": s,
            "user_text": user_text(&records[seg.user_idx]),
            "has_agent_activity": has_agent_activity(&records, seg),
            "needs_summary": plans[s].needs_summary,
            "kept_verbatim": plans[s].kept_verbatim,
            "content_hash": segment_content_hash(&records, seg),
            "derived_index": segment_index(&records, &seg.activity),
            "approx_tokens": approx_tokens(
                &std::iter::once(seg.user_idx)
                    .chain(seg.activity.iter().copied())
                    .map(|i| records[i].clone())
                    .collect::<Vec<_>>()
            ),
        });
        if split {
            seg_obj["parts"] = Value::Array(parts_json);
        } else if let Some(single) = parts_json.pop() {
            seg_obj["covered_uuids"] = single["covered_uuids"].clone();
            seg_obj["activity"] = single["activity"].clone();
        }
        seg_json.push(seg_obj);
    }

    let session_id = records
        .iter()
        .find_map(|r| r.get("sessionId").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let leaf = records
        .iter()
        .rev()
        .find_map(|r| r.get("leafUuid").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();

    let doc = json!({
        "source": src.canonicalize().unwrap_or(src.clone()).to_string_lossy(),
        "original_session_id": session_id,
        "leaf_uuid": leaf,
        "total_records": records.len(),
        "off_path_dropped": off_path,
        "head_record_count": head.len(),
        "approx_tokens_total": approx_tokens(&records),
        "keep_verbatim_last": keep,
        "split_threshold": split_threshold,
        "segments_needing_summary": needs_keys,
        "segments": seg_json,
    });

    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    if let Err(e) = fs::write(&out, serde_json::to_string_pretty(&doc).unwrap()) {
        eprintln!("error: cannot write {}: {e}", out.display());
        return 1;
    }

    eprintln!(
        "extract: {} records in file, {} on active path ({} off-path dropped) → {} segments ({} head, ~{} tokens). Summaries needed for {:?}. Worksheet: {}",
        total_in_file,
        records.len(),
        off_path,
        segs.len(),
        head.len(),
        approx_tokens(&records),
        needs_keys,
        out.display()
    );
    0
}

/// Render one record into worksheet activity items, with mechanical elision: statuses instead of
/// payloads for empty and duplicate results, head+tail truncation so trailing errors survive,
/// char counts so the summarizer can see how much it is not seeing, and a larger budget for
/// errors (they are load-bearing verbatim).
fn render_record(
    r: &Value,
    tool_names: &HashMap<String, String>,
    seen_results: &mut HashSet<String>,
) -> Vec<Value> {
    let mut activity = Vec::new();
    {
        {
            match rec_type(r) {
                "assistant" => {
                    if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
                        for b in blocks {
                            match b.get("type").and_then(|v| v.as_str()) {
                                Some("text") => activity.push(json!({
                                    "kind": "assistant_text",
                                    "text": b.get("text").and_then(|v| v.as_str()).unwrap_or("")
                                })),
                                Some("thinking") => activity.push(json!({
                                    "kind": "thinking",
                                    "text": b.get("thinking").and_then(|v| v.as_str()).unwrap_or("")
                                })),
                                Some("tool_use") => activity.push(json!({
                                    "kind": "tool_use",
                                    "name": b.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                                    "input": b.get("input").cloned().unwrap_or(Value::Null)
                                })),
                                _ => {}
                            }
                        }
                    }
                }
                "user" => {
                    if truthy(r, "isCompactSummary") {
                        // The compaction summary is the sole carrier of pre-boundary context; the
                        // summarizer must see it even though its segment is pinned verbatim.
                        activity.push(json!({
                            "kind": "compact_summary",
                            "text": truncate(&user_text(r), 4000)
                        }));
                    } else if let Some(kind) = delivered_kind(r) {
                        // Agent-delivered reports are distillates: give the summarizer a generous
                        // window so their key findings can be carried into the summary.
                        let text = first_text(r).unwrap_or("");
                        activity.push(json!({
                            "kind": "delivered_message",
                            "delivered": kind,
                            "chars": text.chars().count(),
                            "text": truncate_head_tail(text, 8000, 0.7)
                        }));
                    } else if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
                        for b in blocks {
                            if b.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                                let text = tool_result_text(b);
                                let is_error =
                                    b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                                let tool = b
                                    .get("tool_use_id")
                                    .and_then(|v| v.as_str())
                                    .and_then(|id| tool_names.get(id).cloned())
                                    .unwrap_or_default();
                                let chars = text.chars().count();
                                let (status, rendered) = if is_error {
                                    ("error", truncate_head_tail(&text, 4000, 0.5))
                                } else if text.trim().is_empty() {
                                    ("empty", "[empty result]".to_string())
                                } else if !seen_results.insert(format!("{tool}\u{0}{text}")) {
                                    (
                                        "duplicate",
                                        "[identical to an earlier result in this segment]"
                                            .to_string(),
                                    )
                                } else {
                                    ("ok", truncate_head_tail(&text, TOOL_RESULT_TRUNC, 0.6))
                                };
                                activity.push(json!({
                                    "kind": "tool_result",
                                    "tool": tool,
                                    "status": status,
                                    "chars": chars,
                                    "result": rendered
                                }));
                            }
                        }
                    }
                }
                "system" => activity.push(json!({
                    "kind": "system",
                    "text": truncate(r.get("content").and_then(|v| v.as_str()).unwrap_or(""), 400)
                })),
                _ => {}
            }
        }
    }
    activity
}

// ------------------------------------------------------------------------------------- masking

/// Error output stays verbatim up to this budget (head+tail): the exact assertion or stack trace
/// is the one thing a resumed session cannot re-derive cheaply.
pub const MASK_ERROR_BUDGET: usize = 2000;
/// tool_use input string fields above this length get head+tail truncated (a 50KB Write payload
/// is on disk already; history only needs its shape).
pub const MASK_INPUT_FIELD_MAX: usize = 2000;
/// Non-error results at or under this length stay verbatim: a placeholder would not be smaller,
/// and short results ("ok", a count, a path) are usually the load-bearing part of the exchange.
pub const MASK_RESULT_MIN: usize = 500;

pub enum Masked {
    Unchanged,
    Replaced(Value),
    /// The record held nothing but dead weight (e.g. a lone empty-thinking signature carrier).
    Dropped,
}

/// Mechanical, non-generative compression of one record: replace stale tool-result payloads with
/// placeholders, truncate oversized tool_use input fields, drop empty thinking blocks (their
/// multi-KB signatures are pure dead weight on old turns, which are never replayed with thinking),
/// and elide the top-level toolUseResult duplicate (UI metadata, never sent to the API). Never
/// rewrites prose, so it cannot hallucinate; it can only omit, and the untouched original session
/// retains everything omitted.
pub fn mask_record(r: &Value) -> Masked {
    let mut m = r.clone();
    let mut changed = false;
    if let Some(blocks) = m.pointer_mut("/message/content").and_then(|c| c.as_array_mut()) {
        let before = blocks.len();
        blocks.retain(|b| {
            !(b.get("type").and_then(|v| v.as_str()) == Some("thinking")
                && b.get("thinking")
                    .and_then(|v| v.as_str())
                    .is_none_or(|t| t.trim().is_empty()))
        });
        if blocks.len() != before {
            changed = true;
        }
        if blocks.is_empty() {
            return Masked::Dropped;
        }
        for b in blocks.iter_mut() {
            match b.get("type").and_then(|v| v.as_str()) {
                Some("tool_result") => {
                    let text = tool_result_text(b);
                    let is_error = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                    let chars = text.chars().count();
                    let replacement = if is_error {
                        if chars <= MASK_ERROR_BUDGET {
                            continue; // errors under budget stay verbatim
                        }
                        truncate_head_tail(&text, MASK_ERROR_BUDGET, 0.5)
                    } else {
                        if chars <= MASK_RESULT_MIN {
                            continue; // short results stay verbatim; a placeholder would not be smaller
                        }
                        format!("[recompact: elided {chars}-char result; the original session file retains it verbatim]")
                    };
                    if let Some(obj) = b.as_object_mut() {
                        obj.insert("content".into(), Value::String(replacement));
                        changed = true;
                    }
                }
                Some("tool_use") => {
                    if let Some(input) = b.get_mut("input").and_then(|i| i.as_object_mut()) {
                        for (_k, v) in input.iter_mut() {
                            if let Some(s) = v.as_str() {
                                if s.chars().count() > MASK_INPUT_FIELD_MAX {
                                    *v = Value::String(truncate_head_tail(s, 500, 0.5));
                                    changed = true;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // Delivered content: task notifications are pure ceremony once processed; oversized teammate
    // reports keep a head+tail window (they are distillates, so the gist matters) and remain
    // fully recoverable from the untouched original.
    if let Some(kind) = delivered_kind(r) {
        let text = first_text(r).unwrap_or("").to_string();
        let chars = text.chars().count();
        let replacement = match kind {
            "task_notification" => Some("[recompact: task notification elided]".to_string()),
            "teammate_message" if chars > 4000 => Some(truncate_head_tail(&text, 4000, 0.6)),
            _ => None,
        };
        if let Some(newtext) = replacement {
            if let Some(msg) = m.pointer_mut("/message").and_then(|v| v.as_object_mut()) {
                msg.insert("content".into(), Value::String(newtext));
                changed = true;
            }
            if let Some(obj) = m.as_object_mut() {
                obj.insert("recompactDelivered".into(), Value::String(kind.to_string()));
            }
        }
    }
    // Claude Code duplicates every tool result in a top-level toolUseResult field (transcript-UI
    // metadata, never part of the API message); for bulky results the duplicate costs as much as
    // the payload itself.
    if let Some(obj) = m.as_object_mut() {
        if let Some(t) = obj.get("toolUseResult") {
            let n = serde_json::to_string(t).map(|s| s.len()).unwrap_or(0);
            if n > MASK_INPUT_FIELD_MAX {
                obj.insert(
                    "toolUseResult".into(),
                    json!({"recompactElided": true, "chars": n}),
                );
                changed = true;
            }
        }
    }
    if changed {
        if let Some(obj) = m.as_object_mut() {
            obj.insert("recompactMasked".into(), Value::Bool(true));
        }
        Masked::Replaced(m)
    } else {
        Masked::Unchanged
    }
}

/// Code-derived facts about a segment: which files were touched and how, what ran, what failed.
/// Deterministic (no model call), so it can be regenerated from raw on every pass without drift,
/// and it cannot forget a file path the way prose summarization measurably does.
pub fn segment_index(records: &[Value], activity: &[usize]) -> Value {
    use std::collections::BTreeMap;
    fn push_role(files: &mut BTreeMap<String, Vec<&'static str>>, path: &str, role: &'static str) {
        let roles = files.entry(path.to_string()).or_default();
        if !roles.contains(&role) {
            roles.push(role);
        }
    }
    let mut files: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    let mut commands: Vec<String> = Vec::new();
    let mut tool_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut error_count = 0usize;
    for &i in activity {
        if let Some(blocks) = content(&records[i]).and_then(|c| c.as_array()) {
            for b in blocks {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("tool_use") => {
                        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        *tool_counts.entry(name.to_string()).or_default() += 1;
                        let input = b.get("input");
                        let path = input
                            .and_then(|i| i.get("file_path").or_else(|| i.get("notebook_path")))
                            .and_then(|v| v.as_str());
                        match (name, path) {
                            ("Read", Some(p)) => push_role(&mut files, p, "read"),
                            ("Edit", Some(p)) | ("NotebookEdit", Some(p)) => {
                                push_role(&mut files, p, "edited")
                            }
                            ("Write", Some(p)) => push_role(&mut files, p, "written"),
                            _ => {}
                        }
                        if name == "Bash" {
                            if let Some(c) =
                                input.and_then(|i| i.get("command")).and_then(|v| v.as_str())
                            {
                                commands.push(truncate(c, 200));
                            }
                        }
                    }
                    Some("tool_result") => {
                        if b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false) {
                            error_count += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    json!({
        "files": files,
        "commands": commands,
        "tool_counts": tool_counts,
        "error_count": error_count,
    })
}

fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------------- subcommand: assemble

pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("os rng");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

pub fn cmd_assemble(args: &[String]) -> i32 {
    match run_assemble(args) {
        Ok(Some((id, _))) => {
            println!("{id}"); // stdout: the new sessionId, for scripting / `claude --resume`
            0
        }
        Ok(None) => 0, // --plan preview
        Err(rc) => rc,
    }
}

/// Core of assemble, returning the new session id and output path (None for --plan previews).
fn run_assemble(args: &[String]) -> Result<Option<(String, PathBuf)>, i32> {
    let (pos, opts) = parse_opts(args);
    let mode = opts
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("summarize");
    if !matches!(mode, "summarize" | "mask") {
        eprintln!("error: unknown --mode {mode} (expected summarize or mask)");
        return Err(2);
    }
    if pos.is_empty() || (mode == "summarize" && pos.len() < 2) {
        return Err(usage());
    }
    if mode == "mask" && pos.len() >= 2 {
        eprintln!("error: --mode mask takes no summaries file (masking is mechanical)");
        return Err(2);
    }
    let src = PathBuf::from(&pos[0]);
    let keep = keep_window(&opts);
    let split_threshold = opts
        .get("split")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SPLIT_THRESHOLD);
    let target = opts
        .get("target")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<usize>().ok());
    let plan_only = opts.get("plan").and_then(|v| v.as_bool()).unwrap_or(false);
    if plan_only && target.is_none() {
        eprintln!("error: --plan requires --target <tokens>");
        return Err(2);
    }

    let loaded = load_jsonl(&src);
    let (records, _off_path) = select_active(loaded);
    let (head, segs) = segment(&records);
    let plans = plan(&records, &segs, keep);

    let summaries: Value = if mode == "summarize" {
        let summaries_path = PathBuf::from(&pos[1]);
        let mut s = String::new();
        if let Err(e) = fs::File::open(&summaries_path).and_then(|mut f| f.read_to_string(&mut s)) {
            eprintln!("error: cannot read {}: {e}", summaries_path.display());
            return Err(1);
        }
        match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {} is not valid JSON: {e}", summaries_path.display());
                return Err(1);
            }
        }
    } else {
        json!({})
    };
    // Summary cache keyed by content hash: on a repeated recompaction of a continued session,
    // unchanged segments (or parts of split segments) resolve from the cache and only new
    // material needs fresh work.
    let seg_parts: Vec<Vec<Vec<usize>>> = segs
        .iter()
        .map(|sg| split_parts(&records, sg, split_threshold))
        .collect();
    let mut key_hashes: HashMap<String, String> = HashMap::new();
    let mut seg_keys: Vec<Vec<String>> = Vec::new();
    for (s, seg) in segs.iter().enumerate() {
        let parts = &seg_parts[s];
        let split = parts.len() > 1;
        let mut keys = Vec::new();
        for (p, part) in parts.iter().enumerate() {
            let key = if split { format!("{s}.{p}") } else { s.to_string() };
            key_hashes.insert(key.clone(), part_content_hash(&records, seg, part, p == 0));
            keys.push(key);
        }
        seg_keys.push(keys);
    }
    let cache_path = opts.get("cache").and_then(|v| v.as_str()).map(PathBuf::from);
    let cache: Map<String, Value> = cache_path
        .as_ref()
        .filter(|p| p.exists())
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let get_summary = |key: &str| -> Option<String> {
        summaries
            .get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                key_hashes
                    .get(key)
                    .and_then(|h| cache.get(h))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
    };

    // With --target, the planner decides per-unit treatments; the budget is an objective, the
    // salience floors are constraints, and floors win.
    let budget: Option<BudgetPlan> = target.map(|t| {
        plan_budget(
            &records,
            &segs,
            &plans,
            &seg_parts,
            &seg_keys,
            t,
            mode == "summarize",
            |key| get_summary(key).map(|s| s.len() / 4 + 60),
        )
    });
    if plan_only {
        print_budget_plan(budget.as_ref().expect("checked above"));
        return Ok(None); // preview only: nothing validated, nothing written
    }
    let treatments: Option<HashMap<String, Treatment>> = budget
        .as_ref()
        .map(|b| b.units.iter().map(|u| (u.key.clone(), u.treatment)).collect());

    // Validate: every segment (or part) that needs a summary has one (masking needs none).
    let mut cache_hits = 0usize;
    if let Some(b) = &budget {
        if !b.need_summaries.is_empty() {
            eprintln!(
                "error: the --target plan needs summaries for {:?}; run with --plan to preview, then provide them",
                b.need_summaries
            );
            return Err(1);
        }
    } else if mode == "summarize" {
        let mut missing: Vec<String> = Vec::new();
        for (s, p) in plans.iter().enumerate() {
            if p.needs_summary {
                for key in &seg_keys[s] {
                    if summaries.get(key).and_then(|v| v.as_str()).is_some() {
                        // explicit summary
                    } else if key_hashes.get(key).and_then(|h| cache.get(h)).is_some() {
                        cache_hits += 1;
                    } else {
                        missing.push(key.clone());
                    }
                }
            }
        }
        if !missing.is_empty() {
            eprintln!("error: missing summaries for {missing:?} in {}", pos[1]);
            return Err(1);
        }
    }

    // Optional ledger: standing constraints, corrections, and decisions, re-injected verbatim on
    // every pass just before the verbatim tail (models attend best to recent content). A new
    // ledger supersedes any earlier one wholesale; without one, earlier ledgers are carried.
    let ledger_text = if mode == "summarize" {
        summaries.get("ledger").and_then(|v| v.as_str()).map(String::from)
    } else {
        None
    };
    let drop_old_ledgers = ledger_text.is_some();

    let new_session = uuid_v4();
    let src_abs = src
        .canonicalize()
        .unwrap_or(src.clone())
        .to_string_lossy()
        .into_owned();
    let orig_session_id = records
        .iter()
        .find_map(|r| r.get("sessionId").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();

    // Carry-over envelope fields, pulled from the source.
    let cwd = field_str(&records, "cwd");
    let git_branch = field_str(&records, "gitBranch");
    let version = field_str(&records, "version");
    let model = records
        .iter()
        .find(|r| rec_type(r) == "assistant")
        .and_then(|r| r.pointer("/message/model"))
        .and_then(|v| v.as_str())
        .unwrap_or("claude-opus-4-7")
        .to_string();

    // Shared builder for synthetic summary records (classic mode and budget-planned units).
    let make_synthetic = |seg: &Segment, part: &[usize], key: &str, first: bool, summary: String| -> Value {
        let ts = records[seg.user_idx]
            .get("timestamp")
            .cloned()
            .unwrap_or(Value::Null);
        let mut covered: Vec<Value> = Vec::new();
        if first {
            if let Some(u) = rec_uuid(&records[seg.user_idx]) {
                covered.push(Value::String(u.to_string()));
            }
        }
        for &i in part {
            if let Some(u) = rec_uuid(&records[i]) {
                covered.push(Value::String(u.to_string()));
            }
        }
        json!({
            "parentUuid": Value::Null,            // fixed up in the rechain pass
            "isSidechain": false,
            "userType": "external",
            "type": "assistant",
            "uuid": uuid_v4(),
            "timestamp": ts,
            "sessionId": new_session,
            "cwd": cwd,
            "gitBranch": git_branch,
            "version": version,
            "recompactSynthetic": true,           // marks this as a compaction summary, not a real turn
            "recompactProvenance": {
                "source": src_abs,
                "sourceSessionId": orig_session_id,
                "part": key,
                "coveredUuids": covered
            },
            "recompactIndex": segment_index(&records, part),
            "message": {
                "id": format!("msg_recompact_{}", uuid_v4().replace('-', "")),
                "role": "assistant",
                "model": model,
                "type": "message",
                "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": summary }]
            }
        })
    };

    let mut out: Vec<Value> = Vec::new();

    // Head: keep only records that carry a uuid (drop ephemeral scaffolding like queue-operation).
    for &i in &head {
        if drop_old_ledgers && truthy(&records[i], "recompactLedger") {
            continue; // superseded by the new ledger
        }
        if rec_uuid(&records[i]).is_some() {
            out.push(records[i].clone());
        }
    }

    let tail_start = segs.len().saturating_sub(keep);
    let mut ledger_pending = ledger_text.as_ref().map(|text| {
        let ts = segs
            .get(tail_start.min(segs.len().saturating_sub(1)))
            .and_then(|sg| records[sg.user_idx].get("timestamp").cloned())
            .unwrap_or(Value::Null);
        json!({
            "parentUuid": Value::Null,
            "isSidechain": false,
            "userType": "external",
            "type": "assistant",
            "uuid": uuid_v4(),
            "timestamp": ts,
            "sessionId": new_session,
            "cwd": cwd,
            "gitBranch": git_branch,
            "version": version,
            "recompactLedger": true,
            "message": {
                "id": format!("msg_recompact_{}", uuid_v4().replace('-', "")),
                "role": "assistant",
                "model": model,
                "type": "message",
                "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": format!("Standing constraints, corrections, and decisions for this session (recompact ledger; supersedes any earlier ledger):\n{text}") }]
            }
        })
    });

    for (s, seg) in segs.iter().enumerate() {
        if s == tail_start {
            if let Some(l) = ledger_pending.take() {
                out.push(l);
            }
        }
        out.push(records[seg.user_idx].clone());
        if plans[s].kept_verbatim {
            for &i in &seg.activity {
                if drop_old_ledgers && truthy(&records[i], "recompactLedger") {
                    continue;
                }
                if rec_uuid(&records[i]).is_some() {
                    out.push(records[i].clone());
                }
            }
        } else if let Some(tr) = &treatments {
            // Budget-planned emission: each unit gets exactly the treatment the plan chose.
            for (p, part) in seg_parts[s].iter().enumerate() {
                let key = &seg_keys[s][p];
                match tr.get(key.as_str()).copied().unwrap_or(Treatment::Verbatim) {
                    Treatment::Verbatim => {
                        for &i in part {
                            if drop_old_ledgers && truthy(&records[i], "recompactLedger") {
                                continue;
                            }
                            if rec_uuid(&records[i]).is_some() {
                                out.push(records[i].clone());
                            }
                        }
                    }
                    Treatment::Mask => {
                        for &i in part {
                            if drop_old_ledgers && truthy(&records[i], "recompactLedger") {
                                continue;
                            }
                            if rec_uuid(&records[i]).is_some() {
                                match mask_record(&records[i]) {
                                    Masked::Unchanged => out.push(records[i].clone()),
                                    Masked::Replaced(v) => out.push(v),
                                    Masked::Dropped => {}
                                }
                            }
                        }
                    }
                    Treatment::Summarize => {
                        out.push(make_synthetic(seg, part, key, p == 0, get_summary(key).unwrap()));
                    }
                }
            }
        } else if plans[s].needs_summary && mode == "mask" {
            // Mechanical lane: keep every record, elide stale tool-result payloads. Pairs stay
            // atomic (both halves kept), so the structure the Messages API validates is intact.
            for &i in &seg.activity {
                if drop_old_ledgers && truthy(&records[i], "recompactLedger") {
                    continue;
                }
                if rec_uuid(&records[i]).is_some() {
                    match mask_record(&records[i]) {
                        Masked::Unchanged => out.push(records[i].clone()),
                        Masked::Replaced(v) => out.push(v),
                        Masked::Dropped => {}
                    }
                }
            }
        } else if plans[s].needs_summary {
            // One synthetic record per part (oversized segments split at delegation seams), each
            // carrying provenance to the exact raw records it replaced, so `rehydrate` (or any
            // future agent) can recover the verbatim originals from the untouched source.
            for (p, part) in seg_parts[s].iter().enumerate() {
                let key = &seg_keys[s][p];
                out.push(make_synthetic(seg, part, key, p == 0, get_summary(key).unwrap()));
            }
        }
    }

    if let Some(l) = ledger_pending.take() {
        out.push(l); // keep >= segment count: the ledger still lands, at the end
    }

    // Drop any tool_use with no matching tool_result (e.g. an in-flight call at the tail of a live
    // session). The Messages API rejects a tool_use not followed by its tool_result, so a resumable
    // file must not contain one.
    sanitize_tool_pairs(&mut out);

    // Rechain: linear parentUuid over all records that have a uuid; rewrite sessionId everywhere.
    let mut prev: Option<String> = None;
    for r in out.iter_mut() {
        if let Some(obj) = r.as_object_mut() {
            if obj.contains_key("sessionId") {
                obj.insert("sessionId".into(), Value::String(new_session.clone()));
            }
            // Strip stale `usage` metadata. `/context` reads the most recent assistant message's
            // usage (cache_read + cache_creation + input) rather than re-tokenizing — so verbatim
            // records copied from the source would otherwise report the ORIGINAL session's token
            // count (the whole point of compacting is defeated, and autocompact may misfire).
            if let Some(msg) = obj.get_mut("message").and_then(|m| m.as_object_mut()) {
                msg.remove("usage");
            }
            if let Some(u) = obj.get("uuid").and_then(|v| v.as_str()).map(String::from) {
                obj.insert(
                    "parentUuid".into(),
                    prev.clone().map(Value::String).unwrap_or(Value::Null),
                );
                prev = Some(u);
            }
        }
    }
    let leaf = prev.clone().unwrap_or_default();

    // Fresh last-prompt tail pointing at the new leaf.
    let last_prompt = last_prompt_text(&records)
        .or_else(|| segs.last().map(|seg| user_text(&records[seg.user_idx])))
        .unwrap_or_default();
    out.push(json!({
        "type": "last-prompt",
        "leafUuid": leaf,
        "sessionId": new_session,
        "lastPrompt": last_prompt,
    }));

    // Output path: create-new only, in the same dir as the source.
    let out_path = match opts.get("out").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => {
            let dir = src.parent().unwrap_or_else(|| Path::new("."));
            dir.join(format!("{new_session}.jsonl"))
        }
    };
    if out_path.exists() {
        eprintln!(
            "error: refusing to overwrite existing file {}",
            out_path.display()
        );
        return Err(1);
    }
    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: cannot create {}: {e}", out_path.display());
            return Err(1);
        }
    };
    for r in &out {
        if let Err(e) = writeln!(f, "{}", serde_json::to_string(r).unwrap()) {
            eprintln!("error: write failed: {e}");
            return Err(1);
        }
    }

    // Persist explicitly-provided summaries into the cache, keyed by content hash, so the next
    // recompaction of this (continued) session reuses them for unchanged segments.
    drop(get_summary);
    if let Some(cp) = &cache_path {
        let mut cache = cache;
        for (s, pl) in plans.iter().enumerate() {
            if pl.needs_summary {
                for key in &seg_keys[s] {
                    if let Some(text) = summaries.get(key).and_then(|v| v.as_str()) {
                        if let Some(h) = key_hashes.get(key) {
                            cache.insert(h.clone(), Value::String(text.to_string()));
                        }
                    }
                }
            }
        }
        if let Some(parent) = cp.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        if let Err(e) = fs::write(cp, serde_json::to_string_pretty(&Value::Object(cache)).unwrap())
        {
            eprintln!("warning: could not write summary cache {}: {e}", cp.display());
        }
    }

    eprintln!(
        "assemble ({mode}): {} → {} records (~{} → ~{} tokens, keep last {} verbatim, {} summaries from cache).\n  new sessionId: {}\n  wrote: {}",
        records.len(),
        out.len(),
        approx_tokens(&records),
        approx_tokens(&out),
        keep,
        cache_hits,
        new_session,
        out_path.display()
    );
    // Record lineage next to the sessions themselves, so resolution needs no global state.
    if !orig_session_id.is_empty() {
        if let Some(dir) = out_path.parent() {
            lineage_record(dir, &orig_session_id, &new_session, &out_path);
        }
    }
    Ok(Some((new_session, out_path)))
}

fn field_str(records: &[Value], key: &str) -> Value {
    records
        .iter()
        .find_map(|r| r.get(key).filter(|v| !v.is_null()).cloned())
        .unwrap_or(Value::Null)
}

/// Remove `tool_use` content blocks whose id has no matching `tool_result` anywhere in `out`, then
/// drop any record whose content array is thereby emptied. We only ever remove an *unmatched*
/// tool_use, so no `tool_result` is left orphaned (collapsed segments drop both halves together).
pub fn sanitize_tool_pairs(out: &mut Vec<Value>) {
    let mut results: HashSet<String> = HashSet::new();
    for r in out.iter() {
        if let Some(blocks) = r.pointer("/message/content").and_then(|c| c.as_array()) {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                    if let Some(id) = b.get("tool_use_id").and_then(|v| v.as_str()) {
                        results.insert(id.to_string());
                    }
                }
            }
        }
    }
    for r in out.iter_mut() {
        let mut emptied = false;
        if let Some(blocks) = r.pointer_mut("/message/content").and_then(|c| c.as_array_mut()) {
            blocks.retain(|b| {
                let is_orphan_use = b.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                    && b.get("id")
                        .and_then(|v| v.as_str())
                        .map_or(false, |id| !results.contains(id));
                !is_orphan_use
            });
            emptied = blocks.is_empty();
        }
        if emptied {
            if let Some(o) = r.as_object_mut() {
                o.insert("__drop".into(), Value::Bool(true));
            }
        }
    }
    out.retain(|r| !r.get("__drop").and_then(|v| v.as_bool()).unwrap_or(false));
}

// ------------------------------------------------------------------- lineage, continue, resume, scan

fn lineage_path_for(dir: &Path) -> PathBuf {
    dir.join(".recompact-lineage.json")
}

fn lineage_load(dir: &Path) -> Map<String, Value> {
    fs::read_to_string(lineage_path_for(dir))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Record child -> parent lineage next to the session files themselves (create-new sidecar; no
/// global state), so any process in the project can resolve "the newest compacted descendant".
pub fn lineage_record(dir: &Path, parent: &str, child: &str, output: &Path) {
    let mut m = lineage_load(dir);
    m.insert(
        child.to_string(),
        json!({
            "parent": parent,
            "output": output.to_string_lossy(),
            "at": now_secs(),
        }),
    );
    if let Err(e) = fs::write(
        lineage_path_for(dir),
        serde_json::to_string_pretty(&Value::Object(m)).unwrap(),
    ) {
        eprintln!(
            "warning: could not write lineage registry {}: {e}",
            lineage_path_for(dir).display()
        );
    }
}

/// Remove a lineage entry (used when its output file is deleted, e.g. by the churn guard) so
/// resolution can never route to a session that no longer exists.
pub fn lineage_remove(dir: &Path, child: &str) {
    let mut m = lineage_load(dir);
    if m.remove(child).is_some() {
        let _ = fs::write(
            lineage_path_for(dir),
            serde_json::to_string_pretty(&Value::Object(m)).unwrap(),
        );
    }
}

/// Follow the lineage from a session id to its newest compacted descendant. Returns the input id
/// unchanged when it has no descendants (identity resolution keeps this composable).
pub fn lineage_latest(dir: &Path, start: &str) -> String {
    let m = lineage_load(dir);
    let mut cur = start.to_string();
    for _ in 0..1000 {
        let next = m
            .iter()
            .filter(|(_, v)| v.get("parent").and_then(|p| p.as_str()) == Some(cur.as_str()))
            .max_by_key(|(_, v)| v.get("at").and_then(|a| a.as_u64()).unwrap_or(0))
            .map(|(k, _)| k.clone());
        match next {
            Some(n) if n != cur => cur = n,
            _ => break,
        }
    }
    cur
}

/// Claude Code names a project dir after the cwd with every non-alphanumeric character replaced
/// by '-' (verified: `Sideshift_webapp` becomes `Sideshift-webapp`, `/.claude-worktrees/x`
/// becomes `--claude-worktrees-x`).
pub fn munge_project_path(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn project_dir_from_cwd() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let munged = munge_project_path(&cwd.to_string_lossy());
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".claude/projects").join(munged))
}

/// Resolve a `<session.jsonl path | sessionId>` argument to (project dir, session id).
fn resolve_session_arg(arg: &str) -> Result<(PathBuf, String), i32> {
    let given = PathBuf::from(arg);
    if given.exists() {
        let dir = given
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let id = given
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok((dir, id))
    } else {
        let Some(dir) = project_dir_from_cwd() else {
            eprintln!("error: cannot derive the project dir from the cwd");
            return Err(1);
        };
        if !dir.exists() {
            eprintln!(
                "error: {arg} is not a file, and no project dir exists at {}",
                dir.display()
            );
            return Err(1);
        }
        Ok((dir, arg.to_string()))
    }
}

/// Print the newest compacted descendant of a session — the id to `claude --resume`.
pub fn cmd_resume(args: &[String]) -> i32 {
    let (pos, _opts) = parse_opts(args);
    if pos.is_empty() {
        return usage();
    }
    let (dir, id) = match resolve_session_arg(&pos[0]) {
        Ok(v) => v,
        Err(rc) => return rc,
    };
    let latest = lineage_latest(&dir, &id);
    eprintln!("resume with: claude --resume {latest}");
    println!("{latest}");
    0
}

/// Newest session file in a project dir. Interactive resumes mint a new bridge-session id
/// (verified live), so after an interactive stint the live head must be re-discovered from disk
/// rather than assumed stable.
pub fn newest_session(dir: &Path) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    let entries = fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let Ok(md) = e.metadata() else { continue };
        let Ok(t) = md.modified() else { continue };
        if best.as_ref().map_or(true, |(bt, _)| t > *bt) {
            best = Some((t, stem));
        }
    }
    best.map(|(_, id)| id)
}

/// Does the transcript carry an active goal? The goal evaluator writes a goal_status attachment
/// after each turn; the latest one's `met` flag is the live state (verified empirically: this is
/// where goal state persists, and it survives both resume and compaction). Resume does NOT start
/// a turn on its own, so an active goal needs a kick-prompt to re-engage.
pub fn has_active_goal(records: &[Value]) -> bool {
    records
        .iter()
        .rev()
        .find_map(|r| {
            if r.pointer("/attachment/type").and_then(|v| v.as_str()) == Some("goal_status") {
                Some(
                    r.pointer("/attachment/met")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                )
            } else {
                None
            }
        })
        .map(|met| !met)
        .unwrap_or(false)
}

// --------------------------------------------------------------------------- headless summarizer

/// Units (segments split into parts) with keys and content hashes: the shared shape the planner,
/// cache, and summarizer all key on.
pub struct Units {
    pub segs: Vec<Segment>,
    pub plans: Vec<SegPlan>,
    pub seg_parts: Vec<Vec<Vec<usize>>>,
    pub seg_keys: Vec<Vec<String>>,
    pub key_hashes: HashMap<String, String>,
}

pub fn build_units(records: &[Value], keep: usize, split: usize) -> Units {
    let (_, segs) = segment(records);
    let plans = plan(records, &segs, keep);
    let seg_parts: Vec<Vec<Vec<usize>>> = segs
        .iter()
        .map(|sg| split_parts(records, sg, split))
        .collect();
    let mut key_hashes = HashMap::new();
    let mut seg_keys = Vec::new();
    for (s, seg) in segs.iter().enumerate() {
        let parts = &seg_parts[s];
        let is_split = parts.len() > 1;
        let mut keys = Vec::new();
        for (p, part) in parts.iter().enumerate() {
            let key = if is_split { format!("{s}.{p}") } else { s.to_string() };
            key_hashes.insert(key.clone(), part_content_hash(records, seg, part, p == 0));
            keys.push(key);
        }
        seg_keys.push(keys);
    }
    Units {
        segs,
        plans,
        seg_parts,
        seg_keys,
        key_hashes,
    }
}

pub const DIGEST_CAP: usize = 8000;

/// Compact text rendering of one unit for the headless summarizer: the user ask plus the
/// mechanically pre-elided activity, dense enough for a cheap model to write a faithful recap.
pub fn unit_digest(records: &[Value], seg: &Segment, part: &[usize]) -> String {
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for &i in &seg.activity {
        if let Some(blocks) = records[i]
            .pointer("/message/content")
            .and_then(|c| c.as_array())
        {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    if let (Some(id), Some(name)) = (
                        b.get("id").and_then(|v| v.as_str()),
                        b.get("name").and_then(|v| v.as_str()),
                    ) {
                        tool_names.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }
    let mut seen = HashSet::new();
    let mut lines = vec![format!(
        "USER ASKED: {}",
        truncate(&user_text(&records[seg.user_idx]), 600)
    )];
    for &i in part {
        for a in render_record(&records[i], &tool_names, &mut seen) {
            let kind = a["kind"].as_str().unwrap_or("");
            let line = match kind {
                "assistant_text" => Some(format!(
                    "ASSISTANT: {}",
                    truncate(a["text"].as_str().unwrap_or(""), 900)
                )),
                "tool_use" => Some(format!(
                    "TOOL {} {}",
                    a["name"].as_str().unwrap_or("?"),
                    truncate(&a["input"].to_string(), 150)
                )),
                "tool_result" => Some(format!(
                    "RESULT[{},{}]: {}",
                    a["status"].as_str().unwrap_or("?"),
                    a["chars"].as_u64().unwrap_or(0),
                    truncate(a["result"].as_str().unwrap_or(""), 250)
                )),
                "delivered_message" => Some(format!(
                    "AGENT-REPORT({}): {}",
                    a["delivered"].as_str().unwrap_or("?"),
                    truncate(a["text"].as_str().unwrap_or(""), 700)
                )),
                "compact_summary" => Some(format!(
                    "COMPACT-SUMMARY: {}",
                    truncate(a["text"].as_str().unwrap_or(""), 400)
                )),
                "system" => Some(format!(
                    "SYSTEM: {}",
                    truncate(a["text"].as_str().unwrap_or(""), 200)
                )),
                _ => None,
            };
            if let Some(l) = line {
                lines.push(l);
            }
        }
    }
    truncate(&lines.join("\n"), DIGEST_CAP)
}

pub const SUMMARIZER_RUBRIC: &str = "You are compacting a Claude Code session transcript. For EACH unit below, write the replacement summary in first person past tense, as the assistant's own recap. Preserve exactly: decisions and their reasons, rejected approaches, discovered values/names/numbers/ids (quote them verbatim), errors and their outcomes, file paths. State success only where the activity shows it verified; mark observed-but-unverified as such. 3 to 6 sentences per unit. Return ONLY a JSON object mapping each unit key to its summary string.";

const BATCH_MAX_UNITS: usize = 10;
const BATCH_MAX_CHARS: usize = 120_000;
const SUMMARIZE_WAVES: usize = 3;

pub struct SummarizeCfg {
    pub bin: String,
    pub model: String,
    pub escalate_with: Option<String>,
    pub escalate_above: f32,
}

struct BatchJob {
    model: String,
    keys: Vec<String>,
    prompt: String,
}

/// One headless call, run from an empty temp cwd with no MCP servers so per-call overhead is a
/// few seconds and zero side processes (the naive per-unit version from the prototype spawned a
/// project's whole MCP fleet per call).
fn call_claude_stdin(bin: &str, model: &str, prompt: &str) -> Result<String, String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let tmp = std::env::temp_dir().join(format!("recompact-sum-{}", uuid_v4()));
    let _ = fs::create_dir_all(&tmp);
    let mut child = Command::new(bin)
        .current_dir(&tmp)
        .args(["-p", "--model", model, "--strict-mcp-config"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot spawn {bin}: {e}"))?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(prompt.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&tmp);
    if !out.status.success() {
        return Err(format!("{bin} exited {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn extract_json_object(s: &str) -> Option<Map<String, Value>> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    serde_json::from_str::<Value>(&s[start..=end])
        .ok()?
        .as_object()
        .cloned()
}

fn batch_prompt(keys: &[String], body: &str) -> String {
    format!(
        "{SUMMARIZER_RUBRIC}\nThe JSON keys must be exactly {} — the bare identifiers, nothing else.\n{body}",
        serde_json::to_string(keys).unwrap_or_default()
    )
}

fn make_batches(units: &[&(String, f32, String)], model: &str) -> Vec<BatchJob> {
    let mut jobs = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut body = String::new();
    for (k, _, d) in units {
        let block = format!("\n### UNIT {k}\n{d}\n");
        if !keys.is_empty()
            && (keys.len() >= BATCH_MAX_UNITS || body.len() + block.len() > BATCH_MAX_CHARS)
        {
            let prompt = batch_prompt(&keys, &body);
            jobs.push(BatchJob {
                model: model.to_string(),
                keys: std::mem::take(&mut keys),
                prompt,
            });
            body.clear();
        }
        keys.push(k.clone());
        body.push_str(&block);
    }
    if !keys.is_empty() {
        let prompt = batch_prompt(&keys, &body);
        jobs.push(BatchJob {
            model: model.to_string(),
            keys,
            prompt,
        });
    }
    jobs
}

/// Models sometimes echo the marker into the key ("UNIT 3.1" for "3.1"); match tolerantly.
fn lookup_summary<'a>(obj: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    if let Some(s) = obj.get(k).and_then(|v| v.as_str()) {
        return Some(s);
    }
    for (name, v) in obj {
        let t = name.trim().trim_start_matches('#').trim();
        let t = t
            .strip_prefix("UNIT")
            .or_else(|| t.strip_prefix("unit"))
            .map(str::trim)
            .unwrap_or(t);
        if t == k {
            return v.as_str();
        }
    }
    None
}

/// Summarize (key, salience, digest) units headlessly: contiguous batches so consecutive units
/// share narrative context, salience-routed escalation to a stronger model for decision-bearing
/// units, waves of concurrent calls, one retry round for stragglers. Returns key -> summary.
pub fn headless_summarize(
    units: &[(String, f32, String)],
    cfg: &SummarizeCfg,
) -> Result<HashMap<String, String>, String> {
    let mut result: HashMap<String, String> = HashMap::new();
    for _round in 0..2 {
        let remaining: Vec<&(String, f32, String)> = units
            .iter()
            .filter(|(k, _, _)| !result.contains_key(k))
            .collect();
        if remaining.is_empty() {
            break;
        }
        let mut hot: Vec<&(String, f32, String)> = Vec::new();
        let mut cold: Vec<&(String, f32, String)> = Vec::new();
        for &u in &remaining {
            if cfg.escalate_with.is_some() && u.1 >= cfg.escalate_above {
                hot.push(u);
            } else {
                cold.push(u);
            }
        }
        let mut jobs = make_batches(&cold, &cfg.model);
        jobs.extend(make_batches(
            &hot,
            cfg.escalate_with.as_deref().unwrap_or(&cfg.model),
        ));
        eprintln!(
            "summarize: {} unit(s) in {} batch(es), {} at a time",
            remaining.len(),
            jobs.len(),
            SUMMARIZE_WAVES
        );
        for wave in jobs.chunks(SUMMARIZE_WAVES) {
            let outs: Vec<(Vec<String>, Result<String, String>)> = std::thread::scope(|sc| {
                let handles: Vec<_> = wave
                    .iter()
                    .map(|j| {
                        let bin = cfg.bin.clone();
                        let model = j.model.clone();
                        let prompt = j.prompt.clone();
                        let keys = j.keys.clone();
                        sc.spawn(move || (keys, call_claude_stdin(&bin, &model, &prompt)))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for (keys, out) in outs {
                match out {
                    Ok(text) => {
                        if let Some(obj) = extract_json_object(&text) {
                            for k in keys {
                                if let Some(s) = lookup_summary(&obj, &k) {
                                    if !s.trim().is_empty() {
                                        result.insert(k, s.to_string());
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("summarize: batch failed: {e}"),
                }
            }
            eprintln!("summarize: {}/{} units done", result.len(), units.len());
        }
    }
    let missing: Vec<&str> = units
        .iter()
        .map(|(k, _, _)| k.as_str())
        .filter(|k| !result.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing summaries after retry: {missing:?}"));
    }
    Ok(result)
}

// ---------------------------------------------------------------------- continue (shared core)

pub struct ContinueOpts {
    pub threshold: usize,
    pub keep: Option<String>,
    pub split: Option<String>,
    pub summarize: Option<SummarizeCfg>,
}

pub fn summarize_cfg_from_opts(opts: &Map<String, Value>) -> Option<SummarizeCfg> {
    opts.get("summarize-with")
        .and_then(|v| v.as_str())
        .map(|m| SummarizeCfg {
            bin: opts
                .get("claude-bin")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| std::env::var("RECOMPACT_CLAUDE_BIN").ok())
                .unwrap_or_else(|| "claude".into()),
            model: m.to_string(),
            escalate_with: opts
                .get("escalate-with")
                .and_then(|v| v.as_str())
                .map(String::from),
            escalate_above: opts
                .get("escalate-above")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.4),
        })
}

/// Core continuation step, shared by `continue` and `shell`: resolve the newest compacted
/// descendant; when over threshold, compact toward it (mask-only, or the full ladder when a
/// summarizer is configured: plan, summarize the missing units headlessly into the per-project
/// cache, assemble); verify with rollback; churn-guard. Always returns a resumable id, plus an
/// exit code for the CLI.
pub fn continue_session(dir: &Path, start_id: &str, o: &ContinueOpts) -> (String, i32) {
    let latest = lineage_latest(dir, start_id);
    let latest_file = dir.join(format!("{latest}.jsonl"));
    if !latest_file.exists() {
        eprintln!("error: session file not found: {}", latest_file.display());
        return (start_id.to_string(), 1);
    }
    let (active, _) = select_active(load_jsonl(&latest_file));
    let tokens = approx_tokens(&active);
    if tokens <= o.threshold {
        eprintln!(
            "continue: ~{tokens} tokens ≤ threshold {}; nothing to compact",
            o.threshold
        );
        return (latest, 0);
    }
    let keep: usize = o.keep.as_deref().and_then(|s| s.parse().ok()).unwrap_or(1);
    let split: usize = o
        .split
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SPLIT_THRESHOLD);
    let cache_path = dir.join(".recompact-summary-cache.json");

    // Full-ladder pre-pass: find the units the budget plan wants summarized, fill the cache.
    let mut sums_path: Option<PathBuf> = None;
    if let Some(cfg) = &o.summarize {
        let u = build_units(&active, keep, split);
        let cache: Map<String, Value> = fs::read_to_string(&cache_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let b = plan_budget(
            &active,
            &u.segs,
            &u.plans,
            &u.seg_parts,
            &u.seg_keys,
            o.threshold,
            true,
            |key| {
                u.key_hashes
                    .get(key)
                    .and_then(|h| cache.get(h))
                    .and_then(|v| v.as_str())
                    .map(|s| s.len() / 4 + 60)
            },
        );
        let mut work: Vec<(String, f32, String)> = Vec::new();
        for unit in &b.units {
            if unit.treatment == Treatment::Summarize {
                let h = &u.key_hashes[&unit.key];
                if !cache.contains_key(h) {
                    let p: usize = unit
                        .key
                        .split('.')
                        .nth(1)
                        .and_then(|x| x.parse().ok())
                        .unwrap_or(0);
                    let seg = &u.segs[unit.seg];
                    let part = &u.seg_parts[unit.seg][p];
                    work.push((
                        unit.key.clone(),
                        unit.salience,
                        unit_digest(&active, seg, part),
                    ));
                }
            }
        }
        if !work.is_empty() {
            eprintln!(
                "continue: summarizing {} unit(s) with {}{}",
                work.len(),
                cfg.model,
                cfg.escalate_with
                    .as_deref()
                    .map(|m| format!(" (escalating salience ≥ {} to {m})", cfg.escalate_above))
                    .unwrap_or_default()
            );
            let sums = match headless_summarize(&work, cfg) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("continue: summarize failed: {e}");
                    return (latest, 1);
                }
            };
            let mut cache = cache;
            for (k, text) in &sums {
                if let Some(h) = u.key_hashes.get(k) {
                    cache.insert(h.clone(), Value::String(text.clone()));
                }
            }
            if fs::write(
                &cache_path,
                serde_json::to_string_pretty(&Value::Object(cache)).unwrap(),
            )
            .is_err()
            {
                eprintln!("continue: cannot write summary cache");
                return (latest, 1);
            }
        }
        let sp = std::env::temp_dir().join(format!("recompact-empty-{}.json", uuid_v4()));
        let _ = fs::write(&sp, "{}");
        sums_path = Some(sp);
    }

    let mut a_args: Vec<String> = vec![latest_file.to_string_lossy().into_owned()];
    if let Some(sp) = &sums_path {
        a_args.push(sp.to_string_lossy().into_owned());
        a_args.push("--mode".into());
        a_args.push("summarize".into());
        a_args.push("--cache".into());
        a_args.push(cache_path.to_string_lossy().into_owned());
    } else {
        a_args.push("--mode".into());
        a_args.push("mask".into());
    }
    a_args.push("--target".into());
    a_args.push(o.threshold.to_string());
    for (flag, v) in [("keep", &o.keep), ("split", &o.split)] {
        if let Some(v) = v {
            a_args.push(format!("--{flag}"));
            a_args.push(v.clone());
        }
    }
    let (new_id, new_file) = match run_assemble(&a_args) {
        Ok(Some(v)) => v,
        Ok(None) => unreachable!("continue never passes --plan"),
        Err(rc) => return (latest, rc),
    };
    if let Some(sp) = &sums_path {
        let _ = fs::remove_file(sp);
    }
    let v = cmd_verify(&[
        new_file.to_string_lossy().into_owned(),
        "--source".into(),
        latest_file.to_string_lossy().into_owned(),
    ]);
    if v != 0 {
        let _ = fs::remove_file(&new_file);
        lineage_remove(dir, &new_id);
        eprintln!(
            "continue: verification FAILED; removed {} — resuming the previous id is safe",
            new_file.display()
        );
        return (latest, 1);
    }
    // Churn guard: a file that is already mostly incompressible must not spawn descendants every
    // loop iteration.
    let (na, _) = select_active(load_jsonl(&new_file));
    let ntokens = approx_tokens(&na);
    if ntokens * 100 >= tokens * 95 {
        let _ = fs::remove_file(&new_file);
        lineage_remove(dir, &new_id);
        eprintln!("continue: no meaningful reduction (~{tokens} → ~{ntokens}); keeping {latest}");
        return (latest, 0);
    }
    eprintln!("continue: ~{tokens} → ~{ntokens} tokens; resume with: claude --resume {new_id}");
    (new_id, 0)
}

/// The autonomous continuation step, CLI form. Stdout is always a resumable id, so a driver loop
/// can do: ID=$(recompact continue "$ID"); claude -p --resume "$ID" "next step".
pub fn cmd_continue(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    if pos.is_empty() {
        return usage();
    }
    let (dir, start_id) = match resolve_session_arg(&pos[0]) {
        Ok(v) => v,
        Err(rc) => return rc,
    };
    let o = ContinueOpts {
        threshold: opts
            .get("threshold")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(60_000),
        keep: opts.get("keep").and_then(|v| v.as_str()).map(String::from),
        split: opts.get("split").and_then(|v| v.as_str()).map(String::from),
        summarize: summarize_cfg_from_opts(&opts),
    };
    let (id, rc) = continue_session(&dir, &start_id, &o);
    println!("{id}");
    rc
}

// ----------------------------------------------------------------------------- subcommand: shell

/// One continuous self-compacting session at the terminal: spawn claude interactively (inherited
/// stdio), and when it exits, adopt the live head (interactive resume mints new bridge ids),
/// compact if over threshold, and respawn. An active goal is re-engaged with a kick-prompt (a
/// resumed goal does not start a turn on its own); --goal arms one on the first spawn.
pub fn cmd_shell(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    let dir = match opts.get("dir").and_then(|v| v.as_str()) {
        Some(d) => PathBuf::from(d),
        None => match project_dir_from_cwd() {
            Some(d) => d,
            None => {
                eprintln!("error: cannot derive the project dir from the cwd");
                return 1;
            }
        },
    };
    let _ = fs::create_dir_all(&dir);
    let mut id: Option<String> = pos.first().cloned();
    let auto = opts.get("auto").and_then(|v| v.as_bool()).unwrap_or(false);
    let max_cycles: usize = opts
        .get("max-cycles")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let goal = opts.get("goal").and_then(|v| v.as_str()).map(String::from);
    let kick = opts
        .get("kick")
        .and_then(|v| v.as_str())
        .unwrap_or("continue")
        .to_string();
    let bin = opts
        .get("claude-bin")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| std::env::var("RECOMPACT_CLAUDE_BIN").ok())
        .unwrap_or_else(|| "claude".into());
    let copts = ContinueOpts {
        threshold: opts
            .get("threshold")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(60_000),
        keep: opts.get("keep").and_then(|v| v.as_str()).map(String::from),
        split: opts.get("split").and_then(|v| v.as_str()).map(String::from),
        summarize: summarize_cfg_from_opts(&opts),
    };
    let mut first = true;
    let mut cycles = 0usize;
    loop {
        cycles += 1;
        if max_cycles > 0 && cycles > max_cycles {
            break;
        }
        let mut cmd = std::process::Command::new(&bin);
        if let Some(cur) = id.clone() {
            let (next, rc) = continue_session(&dir, &cur, &copts);
            if rc != 0 {
                eprintln!("shell: continue reported an error; resuming {next}");
            } else if next != cur {
                eprintln!("shell: compacted {cur} -> {next}");
            }
            id = Some(next.clone());
            cmd.arg("--resume").arg(&next);
            if first && goal.is_some() {
                cmd.arg(format!("/goal {}", goal.clone().unwrap()));
            } else if has_active_goal(&load_jsonl(&dir.join(format!("{next}.jsonl")))) {
                cmd.arg(&kick);
            }
        } else if let Some(g) = &goal {
            cmd.arg(format!("/goal {g}"));
        }
        first = false;
        match cmd.status() {
            Ok(st) => {
                if !st.success() {
                    eprintln!("shell: claude exited with {st}");
                }
            }
            Err(e) => {
                eprintln!("shell: cannot spawn {bin}: {e}");
                return 1;
            }
        }
        if let Some(live) = newest_session(&dir) {
            if id.as_deref() != Some(live.as_str()) {
                eprintln!(
                    "shell: live head moved {} -> {live}",
                    id.as_deref().unwrap_or("<none>")
                );
                id = Some(live);
            }
        }
        if !auto {
            eprintln!(
                "shell: session {} ended. Enter = compact+respawn, q = quit",
                id.as_deref().unwrap_or("<none>")
            );
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                break;
            }
            if line.trim() == "q" {
                break;
            }
        }
    }
    if let Some(i) = &id {
        println!("{i}");
    }
    0
}

/// Discovery: what is in this project dir, how big, how compressible, and which sessions are
/// already compacted descendants of something else.
pub fn cmd_scan(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    let estimate = opts.get("estimate").and_then(|v| v.as_bool()).unwrap_or(false);
    let dir = match pos.first() {
        Some(p) => PathBuf::from(p),
        None => match project_dir_from_cwd() {
            Some(d) => d,
            None => {
                eprintln!("error: cannot derive the project dir from the cwd");
                return 1;
            }
        },
    };
    if !dir.is_dir() {
        eprintln!("error: {} is not a directory", dir.display());
        return 1;
    }
    let lineage = lineage_load(&dir);
    let superseded: HashSet<&str> = lineage
        .values()
        .filter_map(|v| v.get("parent").and_then(|p| p.as_str()))
        .collect();

    let mut rows: Vec<(usize, String)> = Vec::new(); // (active_tokens, line)
    let Ok(entries) = fs::read_dir(&dir) else {
        eprintln!("error: cannot read {}", dir.display());
        return 1;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (active, _) = select_active(load_jsonl(&path));
        let tokens = approx_tokens(&active);
        // The mask estimate re-serializes every record; on big projects that is the slow part,
        // so it is opt-in via --estimate.
        let mask_est = if estimate {
            let indices: Vec<usize> = (0..active.len()).collect();
            format!("{:>8}", mask_tokens_of(&active, &indices))
        } else {
            "       -".to_string()
        };
        let genuine = active.iter().filter(|r| is_genuine_user(r)).count();
        let delivered = active.iter().filter(|r| delivered_kind(r).is_some()).count();
        let mut flags: Vec<&str> = Vec::new();
        if active.iter().any(|r| truthy(r, "recompactSynthetic") || truthy(r, "recompactMasked")) {
            flags.push("compacted");
        }
        if superseded.contains(id.as_str()) {
            flags.push("superseded");
        }
        let line = format!(
            "  {:<38} ~{:>8} tok  mask→~{}  turns={:<3} delivered={:<3} {}",
            truncate(&id, 38),
            tokens,
            mask_est,
            genuine,
            delivered,
            flags.join(",")
        );
        rows.push((tokens, line));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    eprintln!("scan: {} ({} sessions)", dir.display(), rows.len());
    for (_, line) in &rows {
        eprintln!("{line}");
    }
    eprintln!("  (superseded sessions have a newer compacted descendant; `recompact resume <id>` resolves it)");
    0
}

// ----------------------------------------------------------------------------------- subcommand: probe

const KNOWN_RECORD_TYPES: &[&str] = &[
    "user",
    "assistant",
    "system",
    "summary",
    "last-prompt",
    "attachment",
    "mode",
    "permission-mode",
    "bridge-session",
    "ai-title",
    "file-history-snapshot",
    "file-history-delta",
    "pr-link",
    "queue-operation",
    "custom-title",
    "agent-name",
    "relocated",
    "worktree-state",
];
const KNOWN_BLOCK_TYPES: &[&str] = &[
    "text",
    "thinking",
    "redacted_thinking",
    "tool_use",
    "tool_result",
    "image",
    "document",
    "fallback",
    "server_tool_use",
    "web_search_tool_result",
];

/// Schema drift alarm. The `.jsonl` format is reverse-engineered and undocumented; run probe after
/// a Claude Code update, before any surgery. Unknown record/block types are warnings (the tool
/// fails open on retention, so unknowns are kept, not lost); a session we cannot even segment is a
/// hard failure.
pub fn cmd_probe(args: &[String]) -> i32 {
    use std::collections::BTreeMap;
    let (pos, _opts) = parse_opts(args);
    if pos.is_empty() {
        return usage();
    }
    let path = PathBuf::from(&pos[0]);
    let records = load_jsonl(&path);
    let with_uuid = records.iter().filter(|r| rec_uuid(r).is_some()).count();

    let mut type_hist: BTreeMap<String, usize> = BTreeMap::new();
    for r in &records {
        *type_hist.entry(rec_type(r).to_string()).or_default() += 1;
    }
    let unknown_types: Vec<&String> = type_hist
        .keys()
        .filter(|t| !KNOWN_RECORD_TYPES.contains(&t.as_str()))
        .collect();

    let mut block_hist: BTreeMap<String, usize> = BTreeMap::new();
    for r in &records {
        if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
            for b in blocks {
                if let Some(t) = b.get("type").and_then(|v| v.as_str()) {
                    *block_hist.entry(t.to_string()).or_default() += 1;
                }
            }
        }
    }
    let unknown_blocks: Vec<&String> = block_hist
        .keys()
        .filter(|t| !KNOWN_BLOCK_TYPES.contains(&t.as_str()))
        .collect();

    let leaf_from_last_prompt = records
        .iter()
        .rev()
        .find(|r| rec_type(r) == "last-prompt")
        .and_then(|r| r.get("leafUuid").and_then(|v| v.as_str()))
        .map(|u| records.iter().any(|r| rec_uuid(r) == Some(u)))
        .unwrap_or(false);

    let (active, off_path) = select_active(records.clone());
    let (_, segs) = segment(&active);
    let genuine_users = segs.len();

    eprintln!("probe: {}", path.display());
    eprintln!("  records: {} ({} with uuid)", records.len(), with_uuid);
    let fmt_hist = |h: &BTreeMap<String, usize>| {
        h.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    eprintln!("  record types: {}", fmt_hist(&type_hist));
    eprintln!("  content blocks: {}", fmt_hist(&block_hist));
    eprintln!(
        "  active path: {} records, {} off-path",
        active.len(),
        off_path
    );
    eprintln!("  genuine user turns (active path): {genuine_users}");
    let (mut teammate, mut notif) = (0usize, 0usize);
    for r in &active {
        match delivered_kind(r) {
            Some("teammate_message") => teammate += 1,
            Some("task_notification") => notif += 1,
            _ => {}
        }
    }
    if teammate + notif > 0 {
        eprintln!(
            "  delivered content (agent-authored, compressible): {teammate} teammate messages, {notif} task notifications"
        );
    }

    let mut warnings = 0;
    if !unknown_types.is_empty() {
        eprintln!("  warning: unknown record types {unknown_types:?} (kept verbatim, but re-verify surgery)");
        warnings += 1;
    }
    if !unknown_blocks.is_empty() {
        eprintln!("  warning: unknown content block types {unknown_blocks:?}");
        warnings += 1;
    }
    if !leaf_from_last_prompt {
        eprintln!("  warning: no resolvable last-prompt leafUuid; active path falls back to the last uuid record");
        warnings += 1;
    }

    let mut hard = 0;
    if with_uuid == 0 {
        eprintln!("  FAIL: no records carry a uuid; this does not look like a session transcript");
        hard += 1;
    }
    if genuine_users == 0 {
        eprintln!("  FAIL: no genuine user turns found on the active path");
        hard += 1;
    }

    if hard > 0 {
        eprintln!("probe: FAILED ({hard} hard failure(s), {warnings} warning(s))");
        1
    } else if warnings > 0 {
        eprintln!("probe: OK with {warnings} warning(s) — possible format drift, proceed with care");
        0
    } else {
        eprintln!("probe: OK, no drift indicators");
        0
    }
}

// ----------------------------------------------------------------------------------- subcommand: rehydrate

/// Recover the verbatim raw records behind a synthetic summary, from the untouched original
/// transcript. Without an ordinal, lists the summaries. With one, dumps the covered records as
/// raw JSONL on stdout.
pub fn cmd_rehydrate(args: &[String]) -> i32 {
    let (pos, _opts) = parse_opts(args);
    if pos.is_empty() {
        return usage();
    }
    let compacted = load_jsonl(Path::new(&pos[0]));
    let synths: Vec<&Value> = compacted
        .iter()
        .filter(|r| truthy(r, "recompactSynthetic"))
        .collect();

    if pos.len() < 2 {
        eprintln!("{} synthetic summaries:", synths.len());
        for (n, r) in synths.iter().enumerate() {
            let text = r
                .pointer("/message/content/0/text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let covered = r
                .pointer("/recompactProvenance/coveredUuids")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            eprintln!(
                "  [{n}] covers {covered} records: {}",
                truncate(&text.replace('\n', " "), 100)
            );
        }
        if synths.iter().any(|r| r.get("recompactProvenance").is_none()) {
            eprintln!("note: some summaries lack provenance (assembled by an older version)");
        }
        return 0;
    }

    let Ok(n) = pos[1].parse::<usize>() else {
        return usage();
    };
    let Some(rec) = synths.get(n) else {
        eprintln!("error: no synthetic summary [{n}] (have {})", synths.len());
        return 1;
    };
    let Some(prov) = rec.get("recompactProvenance") else {
        eprintln!("error: summary [{n}] has no provenance (assembled by an older version)");
        return 1;
    };
    let src = PathBuf::from(prov.get("source").and_then(|v| v.as_str()).unwrap_or(""));
    if !src.exists() {
        eprintln!(
            "error: original transcript not found at {} (moved or deleted?)",
            src.display()
        );
        return 1;
    }
    let want: HashSet<&str> = prov
        .get("coveredUuids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|u| u.as_str()).collect())
        .unwrap_or_default();
    let source = load_jsonl(&src);
    let mut printed = 0;
    for r in &source {
        if rec_uuid(r).is_some_and(|u| want.contains(u)) {
            println!("{}", serde_json::to_string(r).unwrap());
            printed += 1;
        }
    }
    eprintln!(
        "rehydrate: {printed} of {} covered records recovered from {}",
        want.len(),
        src.display()
    );
    if printed == 0 {
        return 1;
    }
    0
}

// ----------------------------------------------------------------------------------- subcommand: verify

/// Structural checks on an assembled session. With --source, additionally proves the genuine user
/// turns survived verbatim (compared against the source's ACTIVE PATH, since off-path user turns
/// are dropped by design).
pub fn cmd_verify(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    if pos.is_empty() {
        return usage();
    }
    let new = load_jsonl(Path::new(&pos[0]));
    let mut checks: Vec<(&str, bool, String)> = Vec::new();

    // Single sessionId across every record that carries one.
    let ids: HashSet<&str> = new
        .iter()
        .filter_map(|r| r.get("sessionId").and_then(|v| v.as_str()))
        .collect();
    checks.push(("single sessionId", ids.len() == 1, format!("found {}", ids.len())));

    // Linear parent chain over uuid-carrying records: root has parentUuid null, each next record
    // points at the previous one.
    let mut prev: Option<&str> = None;
    let mut chain_ok = true;
    let mut chain_detail = String::new();
    for r in &new {
        if let Some(u) = rec_uuid(r) {
            let p = r.get("parentUuid").and_then(|v| v.as_str());
            if p != prev {
                chain_ok = false;
                chain_detail = format!("record {u}: parentUuid {p:?}, expected {prev:?}");
                break;
            }
            prev = Some(u);
        }
    }
    checks.push(("linear parent chain", chain_ok, chain_detail));

    // Tool pairing: every tool_use has a later tool_result; every tool_result has an earlier
    // tool_use. The Messages API 400s on violations, which bricks resume.
    let mut seen_uses: HashSet<String> = HashSet::new();
    let mut pending: HashSet<String> = HashSet::new();
    let mut orphan_result: Option<String> = None;
    for r in &new {
        if let Some(blocks) = r.pointer("/message/content").and_then(|c| c.as_array()) {
            for b in blocks {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("tool_use") => {
                        if let Some(id) = b.get("id").and_then(|v| v.as_str()) {
                            seen_uses.insert(id.to_string());
                            pending.insert(id.to_string());
                        }
                    }
                    Some("tool_result") => {
                        if let Some(id) = b.get("tool_use_id").and_then(|v| v.as_str()) {
                            if !seen_uses.contains(id) {
                                orphan_result = Some(id.to_string());
                            }
                            pending.remove(id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    checks.push((
        "no dangling tool_use",
        pending.is_empty(),
        format!("{pending:?}"),
    ));
    checks.push((
        "no orphan tool_result",
        orphan_result.is_none(),
        orphan_result.clone().unwrap_or_default(),
    ));

    // Stale usage metadata must be stripped, else /context reports the original session's size.
    let usage_left = new
        .iter()
        .filter(|r| r.pointer("/message/usage").is_some())
        .count();
    checks.push((
        "usage stripped",
        usage_left == 0,
        format!("{usage_left} records still carry message.usage"),
    ));

    // Tail: file ends on a last-prompt whose leafUuid is the final uuid record.
    let last_uuid = new.iter().rev().find_map(rec_uuid);
    let tail_ok = new
        .last()
        .map(|r| {
            rec_type(r) == "last-prompt"
                && r.get("leafUuid").and_then(|v| v.as_str()) == last_uuid
        })
        .unwrap_or(false);
    checks.push((
        "last-prompt tail points at leaf",
        tail_ok,
        format!("leaf {last_uuid:?}"),
    ));

    // Optional fidelity check against the source: genuine user turns, in order, must be identical.
    if let Some(srcp) = opts.get("source").and_then(|v| v.as_str()) {
        let (src, _) = select_active(load_jsonl(Path::new(srcp)));
        let texts = |rs: &[Value]| -> Vec<String> {
            rs.iter().filter(|r| is_genuine_user(r)).map(user_text).collect()
        };
        let (a, b) = (texts(&src), texts(&new));
        checks.push((
            "user turns preserved verbatim",
            a == b,
            format!("source has {}, assembled has {}", a.len(), b.len()),
        ));
    }

    let mut fails = 0;
    for (name, ok, detail) in &checks {
        if *ok {
            eprintln!("ok   {name}");
        } else {
            eprintln!("FAIL {name}: {detail}");
            fails += 1;
        }
    }
    if fails == 0 {
        eprintln!("verify: all checks passed");
        0
    } else {
        eprintln!("verify: {fails} check(s) failed");
        1
    }
}
