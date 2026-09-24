//! recompact — deterministic structural surgery for offline, segment-wise compaction of
//! Claude Code session `.jsonl` files. The lossy summarization is NOT done here; it is done by
//! Claude, live, between the two subcommands. This crate only:
//!   extract  — parse a session, select the active path, classify + segment, emit a worksheet
//!   assemble — rebuild a shorter, resume-compatible session from hand-written per-segment summaries
//!   verify   — structural checks on an assembled session (chain, tool pairs, user-turn fidelity)
//!
//! Invariants: the original is opened read-only and never written; the
//! output is create-new-only and lands in the same project transcript dir as the original.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

mod accounting;
mod anchor;
mod brief;
mod launcher;
pub use accounting::*;
pub use anchor::*;
pub use brief::*;
pub use launcher::*;

pub const TOOL_RESULT_TRUNC: usize = 1500;

pub const USAGE: &str = "usage:\n  \
recompact extract   <session.jsonl> [--out work/segments.json] [--keep K]\n  \
recompact assemble  <session.jsonl> <summaries.json> [--keep K] [--out <path>]\n  \
recompact assemble  <session.jsonl> --mode mask [--keep K] [--out <path>]\n  \
recompact verify    <assembled.jsonl> [--source <session.jsonl>]\n  \
recompact probe     <session.jsonl>\n  \
recompact recall    [id] [--query \"words\"] [--session S]   (read back what compaction\n                      \
removed; inside Claude Code the current session is known)\n  \
recompact rehydrate <compacted.jsonl> [part-key | ordinal | uuid | uuid-prefix>=8]\n  \
recompact mcp       [project-dir]  (MCP stdio server exposing `recall`;\n                      \
started by the plugin, not normally run by hand)\n  \
recompact hook      <event>  (plugin hooks: session-start orients a resumed twin;\n                      \
installed by the plugin)\n  \
recompact continue  [session.jsonl | sessionId] [--threshold T] [--keep K]\n                      \
[--summarize-with M [--escalate-with M2] [--escalate-above S]]\n                      \
(no session: the current Claude Code session)\n  \
recompact shell     [--at T] [--target T] [--mask] [--no-auto] [claude args...]\n                      \
(run claude with compaction in place: a bare /recompact, or a\n                      \
turn ending over --at (default 400k on 1M models, else 140k),\n                      \
compacts and resumes in the same terminal with the same flags)\n  \
recompact handoff   [sessionId] [--continue-after]  (compact the current session:\n                      \
queued for turn end under `shell`; otherwise compacts now and\n                      \
prints the resume command)\n  \
recompact prewarm   <session> [--target T]  (summarize ahead into the cache)\n  \
recompact resume    <session.jsonl | sessionId>\n  \
recompact scan      [project-dir] [--estimate]\n\n\
  Token numbers are context tokens (what /context shows), calibrated from the\n  \
  transcript's own usage records when it has them.\n\n\
  --keep K       number of most-recent turns kept verbatim (default 1)\n  \
  --tail-budget <tok>  cap on the verbatim tail (default 80000); an oversized\n                 \
last turn keeps only its newest parts verbatim. 0 disables\n  \
  --mode mask    no-LLM compaction: keep every record, replace old tool-result\n                 \
payloads with placeholders (errors kept verbatim, head+tail)\n  \
  --cache <path> summary cache keyed by segment content hash: unchanged\n                 \
segments reuse their summaries on repeated recompactions\n  \
  --split <tok>  split turns bigger than this (model-visible tokens) into\n                 \
parts at safe seams, delegation boundaries first (default 20000;\n                 \
0 disables). Pass the same value to extract and assemble\n  \
  --target <tok> plan per-unit treatments (verbatim/mask/summarize) toward\n                 \
this budget; salience floors may exceed it, with reasons\n  \
  --plan         with --target: print the plan table and exit without\n                 \
validating or writing anything\n  \
  --error-floor  keep error-bearing units at mask instead of summarizing them\n                 \
(by default they may be summarized: their error text is carried\n                 \
verbatim beneath the summary)";

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
pub(crate) fn content(r: &Value) -> Option<&Value> {
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

/// `user_text` for the verify fidelity comparison: image-elision markers are the one synthetic
/// text an assembled user turn may contain, so they are excluded before comparing to the source.
pub fn fidelity_text(r: &Value) -> String {
    match content(r) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .filter(|t| !t.starts_with("[recompact: image elided"))
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

/// The record set every unit-hashing path must agree on: the active path minus the PREVIOUS
/// generation's orientation preamble.
///
/// `assemble` strips old preambles wholesale (it mints a fresh one), so any path that computes
/// unit keys or content hashes has to strip them too. When it does not, the one unit containing
/// the preamble hashes differently on each side and its cached summary can never be found —
/// which stalls every re-compaction of an already-compacted lineage on exactly that unit,
/// deterministically, no matter how many times it retries.
pub fn select_active_for_units(records: Vec<Value>) -> (Vec<Value>, usize) {
    let (mut kept, dropped) = select_active(records);
    kept.retain(|r| !truthy(r, "recompactPreamble"));
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
    /// In the keep-K tail (kept for recency, not pinned for content).
    pub tail: bool,
    /// For a tail segment too big for the tail budget: the part indices kept verbatim (the most
    /// recent ones). Its other parts compact like any older unit.
    pub pinned_parts: Vec<usize>,
}

/// Default budget for the verbatim tail, in conversation tokens. Keep-K keeps whole turns, and
/// one autonomous "continue" turn can run for hours: real twins carried final turns of 1-3 MB
/// (one 557k tokens), which floored every compaction of them. Over budget, only the most recent
/// parts of the tail stay verbatim.
pub const DEFAULT_TAIL_BUDGET: usize = 80_000;

pub fn tail_budget(opts: &Map<String, Value>) -> usize {
    opts.get("tail-budget")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TAIL_BUDGET)
}

/// Post-split plan refinements every command applies identically (extract, assemble and
/// continue must derive the same units, or cached summaries land on the wrong unit):
/// 1. A turn that `plan` pinned whole because it holds earlier summaries (or a ledger, or a native
///    compaction summary) next to raw records keeps only the parts holding them pinned; its raw
///    parts compact like any other. Otherwise a tail the tail budget cut in one generation would
///    stay frozen in every generation after it.
/// 2. The tail budget (see `DEFAULT_TAIL_BUDGET`), which never unpins such parts either.
pub fn refine_plans(
    records: &[Value],
    segs: &[Segment],
    plans: &mut [SegPlan],
    seg_parts: &[Vec<Vec<usize>>],
    tail_budget_tokens: usize,
) {
    let holds_pinned = |part: &[usize]| part.iter().any(|&i| is_pinned_content(&records[i]));
    for s in 0..segs.len() {
        if !plans[s].kept_verbatim || plans[s].tail {
            continue;
        }
        let parts = &seg_parts[s];
        let pinned: Vec<usize> = (0..parts.len()).filter(|&p| holds_pinned(&parts[p])).collect();
        let raw_with_activity = (0..parts.len())
            .any(|p| !pinned.contains(&p) && parts[p].iter().any(|&i| rec_uuid(&records[i]).is_some()));
        if !pinned.is_empty() && raw_with_activity {
            plans[s].kept_verbatim = false;
            plans[s].needs_summary = true;
            plans[s].pinned_parts = pinned;
        }
    }
    apply_tail_budget(records, segs, plans, seg_parts, tail_budget_tokens);
}

/// Enforce the tail budget on the plan. Measured in visible chars at the fixed default ratio, not
/// the session's calibration, so extract, assemble and continue always derive the same units.
/// The newest part is always kept, and so is any part holding earlier summaries; a budget of 0
/// disables it.
pub fn apply_tail_budget(
    records: &[Value],
    segs: &[Segment],
    plans: &mut [SegPlan],
    seg_parts: &[Vec<Vec<usize>>],
    budget_tokens: usize,
) {
    if budget_tokens == 0 {
        return;
    }
    let budget_chars = (budget_tokens as f64 * DEFAULT_CHARS_PER_TOKEN) as usize;
    let mut used = 0usize;
    let mut first = true;
    for s in (0..segs.len()).rev() {
        if !plans[s].tail {
            break;
        }
        used += visible_chars(&records[segs[s].user_idx]);
        let parts = &seg_parts[s];
        let mut pinned: Vec<usize> = Vec::new();
        for p in (0..parts.len()).rev() {
            let c: usize = parts[p].iter().map(|&i| visible_chars(&records[i])).sum();
            let holds_summaries = parts[p].iter().any(|&i| is_pinned_content(&records[i]));
            if holds_summaries || first || used + c <= budget_chars {
                used += c;
                pinned.push(p);
                first = false;
            } else {
                // Recency is contiguous: once one part misses, every older one does too.
                used = budget_chars + 1;
            }
        }
        if pinned.len() < parts.len() && has_agent_activity(records, &segs[s]) {
            pinned.reverse();
            plans[s].kept_verbatim = false;
            plans[s].needs_summary = true;
            plans[s].pinned_parts = pinned;
        }
    }
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
/// Records this tool must never re-summarize or reshape: its own summaries, ledgers, and native
/// compaction summaries.
pub fn is_pinned_content(r: &Value) -> bool {
    truthy(r, "recompactSynthetic") || truthy(r, "recompactLedger") || truthy(r, "isCompactSummary")
}

fn tool_ids(r: &Value) -> (Vec<&str>, Vec<&str>) {
    let (mut uses, mut results) = (Vec::new(), Vec::new());
    if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
        for b in blocks {
            match b.get("type").and_then(|v| v.as_str()) {
                Some("tool_use") => uses.extend(b.get("id").and_then(|v| v.as_str())),
                Some("tool_result") => results.extend(b.get("tool_use_id").and_then(|v| v.as_str())),
                _ => {}
            }
        }
    }
    (uses, results)
}

pub fn split_parts(records: &[Value], seg: &Segment, threshold: usize) -> Vec<Vec<usize>> {
    let pinned = |i: usize| is_pinned_content(&records[i]);
    let mixed = seg.activity.iter().any(|&i| pinned(i))
        && seg
            .activity
            .iter()
            .any(|&i| !pinned(i) && rec_uuid(&records[i]).is_some());
    if !mixed {
        return split_run(records, &seg.activity, threshold);
    }
    // A turn holding earlier summaries next to raw records (a final turn the tail budget cut in
    // an earlier generation, a ledger at the end of a turn): cut at every boundary between the
    // two, so the summaries stay exactly as they are while the raw remainder stays compactable.
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_kind: Option<bool> = None;
    let mut pending: HashSet<String> = HashSet::new();
    for &i in &seg.activity {
        let kind = rec_uuid(&records[i]).map(|_| pinned(i));
        if let (Some(k), Some(ck)) = (kind, cur_kind) {
            if k != ck && pending.is_empty() && !cur.is_empty() {
                runs.push(std::mem::take(&mut cur));
                cur_kind = None;
            }
        }
        if cur_kind.is_none() {
            cur_kind = kind;
        }
        let (uses, results) = tool_ids(&records[i]);
        pending.extend(uses.into_iter().map(str::to_string));
        for id in results {
            pending.remove(id);
        }
        cur.push(i);
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    let mut parts = Vec::new();
    for run in runs {
        if run.iter().any(|&i| pinned(i)) {
            parts.push(run);
        } else {
            parts.extend(split_run(records, &run, threshold));
        }
    }
    parts
}

/// Size-split one run of records at safe seams.
fn split_run(records: &[Value], run: &[usize], threshold: usize) -> Vec<Vec<usize>> {
    // Sized by what the model sees, at the fixed default ratio (never the session's calibration,
    // so every command cuts identical units). Raw record size is dominated by envelope and by
    // unrendered attachments — current Claude Code writes ~200 KB prompt snapshots — which cut a
    // two-line exchange into three units, two of them with nothing to summarize.
    let rec_tokens = |i: usize| (visible_chars(&records[i]) as f64 / DEFAULT_CHARS_PER_TOKEN) as usize;
    let run_tokens: usize = run.iter().map(|&i| rec_tokens(i)).sum();
    if threshold == 0 || run_tokens <= threshold {
        return vec![run.to_vec()];
    }
    let part_budget = (threshold / 2).max(1);
    let min_part = (threshold / 10).max(1);
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for &i in run {
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
    for &i in run {
        let r = &records[i];
        let mut delegation_end = delivered_kind(r).is_some();
        let (uses, results) = tool_ids(r);
        pending.extend(uses.into_iter().map(str::to_string));
        for id in results {
            pending.remove(id);
            if tool_names
                .get(id)
                .is_some_and(|n| DELEGATION_TOOLS.contains(&n.as_str()))
            {
                delegation_end = true;
            }
        }
        cur.push(i);
        cur_tokens += rec_tokens(i);
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
    plan_ex(records, segs, keep, false)
}

/// With `epochs`, this tool's own summaries (recompactSynthetic) no longer pin their segment:
/// the budget planner may consolidate runs of them into coarser epoch digests re-derived from the
/// RAW records their provenance covers — never from the summary text — so an arbitrarily long
/// self-compaction loop stays bounded instead of monotonically accumulating old summaries.
/// Native compactor summaries (isCompactSummary) still pin: they carry no provenance to re-derive
/// from. Only the budget path uses `epochs`; classic assembly keeps the hard pin.
pub fn plan_ex(records: &[Value], segs: &[Segment], keep: usize, epochs: bool) -> Vec<SegPlan> {
    segs.iter()
        .enumerate()
        .map(|(s, seg)| {
            let tail = s + keep >= segs.len();
            let pinned = seg.activity.iter().any(|&i| {
                truthy(&records[i], "isCompactSummary")
                    || (!epochs && truthy(&records[i], "recompactSynthetic"))
                    || truthy(&records[i], "recompactLedger")
            });
            let kept_verbatim = tail || pinned;
            let needs_summary = has_agent_activity(records, seg) && !kept_verbatim;
            SegPlan {
                kept_verbatim,
                needs_summary,
                tail,
                pinned_parts: Vec::new(),
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
    match parse_jsonl(&buf) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// Parse transcript text. A malformed LAST line is skipped rather than fatal: Claude Code appends
/// to a live session line by line, so a file read mid-write can end in half a record — the normal
/// state of a session compacting itself. A malformed line anywhere else is corruption.
pub fn parse_jsonl(buf: &str) -> Result<Vec<Value>, String> {
    let lines: Vec<&str> = buf.lines().map(str::trim).collect();
    let last = lines.iter().rposition(|l| !l.is_empty());
    let mut out = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => out.push(v),
            Err(_) if Some(n) == last => {}
            Err(e) => return Err(format!("line {} is not valid JSON: {e}", n + 1)),
        }
    }
    Ok(out)
}

/// Estimated chars a base64 image block contributes to context. The API bills an image by its
/// rendered tiles (~1.6k tokens), not its bytes — a 600KB screenshot must not read as 150k
/// tokens, or thresholds fire on phantom mass and compaction chases weight it cannot remove.
pub const IMAGE_EST_CHARS: usize = 6400;

/// Conversation tokens the model is sent for these records on resume, calibrated from their own
/// `usage` records when they carry enough (see accounting.rs for the measurements behind this).
pub fn approx_tokens(records: &[Value]) -> usize {
    calibrate(records).conv_tokens(records)
}

/// Calibration for a transcript: the model's tokenizer ratio (falling back to the model a
/// previous assembly stamped, for a twin with no real turns left) and the live size.
pub fn calibrate_lineage(records: &[Value]) -> Calib {
    let mut c = calibrate(records);
    if c.model.is_none() {
        if let Some(m) = records
            .iter()
            .rev()
            .find_map(|r| r.pointer("/recompactCalibration/model").and_then(|v| v.as_str()))
        {
            c.tokens_per_char = 1.0 / chars_per_token_for(m);
            c.model = Some(m.to_string());
        }
    }
    c
}

/// `calibrate_lineage` plus an explicit `--overhead` from the command line.
pub fn calibrate_opts(records: &[Value], opts: &Map<String, Value>) -> Calib {
    let mut c = calibrate_lineage(records);
    if let Some(o) = opts.get("overhead").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
        c.overhead = o;
    }
    c
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
    // Boolean flags take no value.
    const FLAGS: &[&str] = &[
        "plan",
        "auto",
        "estimate",
        "epochs",
        "summarize-errors",
        "error-floor",
        "json",
        "force",
        "mask",
        "continue-after",
    ];
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
    /// True when the error floor was relaxed (`--summarize-errors`): error-bearing units
    /// could be summarized. Their summaries MUST preserve the error evidence.
    pub allow_error_summarize: bool,
}

/// Visible chars a record set costs once emitted outside the keep tail (ceremony dropped).
fn chars_of(records: &[Value], indices: &[usize]) -> usize {
    indices
        .iter()
        .filter(|&&i| !is_ceremony(&records[i]))
        .map(|&i| visible_chars(&records[i]))
        .sum()
}

fn mask_chars_of(records: &[Value], indices: &[usize]) -> usize {
    indices
        .iter()
        .filter(|&&i| !is_ceremony(&records[i]))
        .map(|&i| match mask_record(&records[i]) {
            Masked::Unchanged => visible_chars(&records[i]),
            Masked::Replaced(v) => visible_chars(&v),
            Masked::Dropped => 0,
        })
        .sum()
}

/// Fixed per-assembly cost of what assemble adds: the orientation preamble with its brief.
const PREAMBLE_EST_TOKENS: usize = 1500;

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
///
/// The error floor (an error-bearing unit never demotes below mask) is a blunt proxy: it fires on
/// any `is_error` result, and cannot tell a benign `grep` exit-1 from a load-bearing failure —
/// on real sessions it pinned 285k-546k tokens of mostly-successful work behind a few KB of
/// no-match probes. Summaries of error-bearing units now carry their error text mechanically
/// (see `error_evidence`), so `allow_error_summarize` is the CLI default; the units keep the +0.4
/// salience bump and are demoted last. Passing false restores the hard floor.
#[allow(clippy::too_many_arguments)]
pub fn plan_budget(
    records: &[Value],
    segs: &[Segment],
    plans: &[SegPlan],
    seg_parts: &[Vec<Vec<usize>>],
    seg_keys: &[Vec<String>],
    target: usize,
    allow_summarize: bool,
    allow_error_summarize: bool,
    epochs: &HashMap<String, Option<String>>,
    summary_tokens: impl Fn(&str) -> Option<usize>,
) -> BudgetPlan {
    plan_budget_calibrated(
        records,
        segs,
        plans,
        seg_parts,
        seg_keys,
        target,
        allow_summarize,
        allow_error_summarize,
        epochs,
        &Calib::for_model(None),
        400,
        summary_tokens,
    )
}

/// What a summary adds beyond its own text: the footer and the carried lines beneath it.
pub const SUMMARY_OVERHEAD_TOKENS: usize = 150;

/// The planner's price for a summary nobody has written yet: the mean of the summaries already
/// in the cache (continue and assemble read the same cache, so they price identically).
pub fn unknown_summary_tokens(cache: &Map<String, Value>, calib: &Calib) -> usize {
    let lens: Vec<usize> = cache.values().filter_map(|v| v.as_str()).map(str::len).collect();
    let mean = if lens.is_empty() { 600 } else { lens.iter().sum::<usize>() / lens.len() };
    calib.tokens(mean) + SUMMARY_OVERHEAD_TOKENS
}

/// `plan_budget` with an explicit token calibration. `target` is conversation tokens: callers
/// holding a context-level budget subtract `calib.overhead` first.
#[allow(clippy::too_many_arguments)]
pub fn plan_budget_calibrated(
    records: &[Value],
    segs: &[Segment],
    plans: &[SegPlan],
    seg_parts: &[Vec<Vec<usize>>],
    seg_keys: &[Vec<String>],
    target: usize,
    allow_summarize: bool,
    allow_error_summarize: bool,
    epochs: &HashMap<String, Option<String>>,
    calib: &Calib,
    unknown_summary: usize,
    summary_tokens: impl Fn(&str) -> Option<usize>,
) -> BudgetPlan {
    let tokens_of = |indices: &[usize]| calib.tokens(chars_of(records, indices));
    // Fixed cost: head + every genuine user turn + pinned/tail segments kept whole + the preamble.
    // Tail activity keeps its ceremony attachments; everything older loses them.
    let mut fixed = PREAMBLE_EST_TOKENS;
    for (s, seg) in segs.iter().enumerate() {
        fixed += calib.tokens(visible_chars(&records[seg.user_idx]));
        if plans[s].kept_verbatim {
            fixed += if plans[s].tail {
                calib.tokens(seg.activity.iter().map(|&i| visible_chars(&records[i])).sum())
            } else {
                tokens_of(&seg.activity)
            };
        }
        for &p in &plans[s].pinned_parts {
            fixed += calib.tokens(seg_parts[s][p].iter().map(|&i| visible_chars(&records[i])).sum());
        }
    }

    // Per-unit file sets for the future-reference signal, then a reverse-scan suffix union.
    let mut unit_meta: Vec<(usize, usize)> = Vec::new(); // (seg, part)
    for (s, parts) in seg_parts.iter().enumerate() {
        if plans[s].kept_verbatim {
            continue;
        }
        for p in 0..parts.len() {
            if !plans[s].pinned_parts.contains(&p) {
                unit_meta.push((s, p));
            }
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
        let verbatim = tokens_of(part);
        let mask = calib.tokens(mask_chars_of(records, part)).min(verbatim);
        let summary = summary_tokens(&key).unwrap_or(unknown_summary).min(mask);
        // Parts touching this tool's own prior summaries are epoch material: consolidatable only
        // when every record is synthetic AND its provenance resolves back to raw (the digest was
        // buildable). Anything else — mixed parts, unresolvable provenance, epochs disabled —
        // stays verbatim: summarizing summary text is the recursive loss this tool exists to
        // avoid.
        let syn = part
            .iter()
            .filter(|&&i| truthy(&records[i], "recompactSynthetic"))
            .count();
        if syn > 0 {
            match epochs.get(&key) {
                Some(Some(_)) => {
                    salience = 0.0; // oldest, already-compressed: first to coarsen
                    floor = "";
                }
                _ => floor = "pinned",
            }
        }
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
            if u.floor == "pinned" {
                continue;
            }
            // The error floor blocks summarization unless the operator lifted it explicitly.
            let summarizable = allow_summarize && (u.floor != "error" || allow_error_summarize);
            let next = match u.treatment {
                // Pure-prose units mask to zero savings; they may skip straight to Summarize,
                // or the ladder would strand them at Verbatim forever.
                Treatment::Verbatim if u.cost[1] < u.cost[0] => Treatment::Mask,
                Treatment::Verbatim if summarizable => Treatment::Summarize,
                Treatment::Mask if summarizable => Treatment::Summarize,
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
        allow_error_summarize,
    }
}

fn print_budget_plan(b: &BudgetPlan, calib: &Calib) {
    eprintln!(
        "plan: target {} context tokens = {} conversation + ~{} system/tools ({})",
        b.target + calib.overhead,
        b.target,
        calib.overhead,
        calib.describe()
    );
    eprintln!(
        "plan: fixed {} (head + user turns + pinned/tail + preamble); unit costs below are conversation tokens",
        b.fixed_tokens
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
            "plan: total {} conversation tokens, {} OVER target: floors and fixed cost hold; this is the price of retention",
            b.planned_total,
            b.planned_total - b.target
        );
    } else {
        eprintln!(
            "plan: total {} conversation tokens (target {})",
            b.planned_total, b.target
        );
    }
    if b.allow_error_summarize {
        let n = b
            .units
            .iter()
            .filter(|u| u.floor == "error" && u.treatment == Treatment::Summarize)
            .count();
        if n > 0 {
            eprintln!(
                "plan: {n} error-bearing unit(s) will be summarized; their error text is carried verbatim beneath the summary (--error-floor keeps them masked instead)"
            );
        }
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
    let (active, off_path) = select_active(loaded);
    let calib = calibrate_opts(&active, &opts);
    let records: Vec<Value> = active
        .into_iter()
        .filter(|r| !truthy(r, "recompactPreamble"))
        .collect();
    let (head, segs) = segment(&records);
    let mut plans = plan(&records, &segs, keep);
    let seg_parts: Vec<Vec<Vec<usize>>> = segs
        .iter()
        .map(|sg| split_parts(&records, sg, split_threshold))
        .collect();
    refine_plans(&records, &segs, &mut plans, &seg_parts, tail_budget(&opts));
    let tokens_of = |idx: &mut dyn Iterator<Item = usize>| -> usize {
        calib.tokens(idx.map(|i| visible_chars(&records[i])).sum())
    };

    let mut needs_keys: Vec<String> = Vec::new();
    let mut mechanical_keys: Vec<String> = Vec::new();
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
        let parts = &seg_parts[s];
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
            let pinned = plans[s].kept_verbatim || plans[s].pinned_parts.contains(&p);
            let empty = unit_is_empty(&records, part);
            if plans[s].needs_summary && !pinned {
                if empty {
                    mechanical_keys.push(key.clone());
                } else {
                    needs_keys.push(key.clone());
                }
            }
            parts_json.push(json!({
                "key": key,
                "kept_verbatim": pinned,
                "no_agent_activity": empty,
                "covered_uuids": covered,
                "content_hash": part_content_hash(&records, seg, part, p == 0),
                "approx_tokens": tokens_of(&mut part.iter().copied()),
                "activity": activity,
            }));
        }

        let mut seg_obj = json!({
            "index": s,
            "user_text": user_text(&records[seg.user_idx]),
            "has_agent_activity": has_agent_activity(&records, seg),
            "needs_summary": plans[s].needs_summary,
            "kept_verbatim": plans[s].kept_verbatim,
            "content_hash": segment_content_hash(&records, seg),
            "derived_index": segment_index(&records, &seg.activity),
            "approx_tokens": tokens_of(&mut std::iter::once(seg.user_idx).chain(seg.activity.iter().copied())),
        });
        if split || !plans[s].pinned_parts.is_empty() {
            seg_obj["parts"] = Value::Array(parts_json);
        } else if let Some(single) = parts_json.pop() {
            seg_obj["covered_uuids"] = single["covered_uuids"].clone();
            seg_obj["activity"] = single["activity"].clone();
            seg_obj["no_agent_activity"] = single["no_agent_activity"].clone();
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
        "approx_tokens_total": calib.context_tokens(&records),
        "token_calibration": calib.describe(),
        "keep_verbatim_last": keep,
        "split_threshold": split_threshold,
        "segments_needing_summary": needs_keys,
        "segments_summarized_mechanically": mechanical_keys,
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
        "extract: {} records in file, {} on active path ({} off-path dropped) → {} segments ({} head), context ~{} tokens ({}). Summaries needed for {:?}{}. Worksheet: {}",
        total_in_file,
        records.len(),
        off_path,
        segs.len(),
        head.len(),
        calib.context_tokens(&records),
        calib.describe(),
        needs_keys,
        if mechanical_keys.is_empty() {
            String::new()
        } else {
            format!(" (plus {} with no agent activity, summarized mechanically)", mechanical_keys.len())
        },
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
/// Characters of a masked result's head kept in its marker.
pub const MASK_PREVIEW_CHARS: usize = 60;
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
    // Model-visible selector for every marker this function writes: the envelope uuid is never
    // shown at inference time, so a resumed model can only ask for what the marker itself names.
    let sel = rec_uuid(r)
        .map(|u| u.get(..8).unwrap_or(u).to_string())
        .unwrap_or_default();
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
                    // Screenshot-style results carry image blocks in their content array; the
                    // bytes get the same flat treatment as top-level images.
                    if let Some(arr) = b.get_mut("content").and_then(|c| c.as_array_mut()) {
                        for e in arr.iter_mut() {
                            if e.get("type").and_then(|v| v.as_str()) == Some("image") {
                                let n = e
                                    .pointer("/source/data")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.len())
                                    .unwrap_or(0);
                                if n > IMAGE_EST_CHARS {
                                    *e = json!({"type": "text", "text": format!("[recompact: image elided (~{} KB); rehydrate {sel} to recover it]", n / 1024)});
                                    changed = true;
                                }
                            }
                        }
                    }
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
                        // A one-line head preview (ARC's stub design): enough to tell a git status
                        // from a test log without spending a recall on it.
                        let head: String = text
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .chars()
                            .take(MASK_PREVIEW_CHARS)
                            .collect();
                        format!("[recompact: elided {chars}-char result, begins \"{head}…\"; rehydrate {sel} to recover it]")
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
                Some("image") => {
                    let n = b
                        .pointer("/source/data")
                        .and_then(|v| v.as_str())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    if n > IMAGE_EST_CHARS {
                        *b = json!({"type": "text", "text": format!("[recompact: image elided (~{} KB); rehydrate {sel} to recover it]", n / 1024)});
                        changed = true;
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

/// Attachment types that are harness ceremony once their turn has passed: per-turn reminders,
/// tool/skill/agent/environment announcements, and CLAUDE.md copies. Resume re-announces the
/// current version of every stateful one — verified by resuming a twin with them stripped: the
/// first new turn gained fresh `instructions`, `skill_listing`, `deferred_tools_delta`,
/// `agent_listing_delta`, `mcp_instructions_delta`, `environment`, `model` and `date`
/// attachments — so dropping old copies outside the keep tail loses nothing and swaps stale text
/// for current text. Their rendered text is replayed on resume (removing 102 of them cut 18k real
/// tokens from an 86k twin). Types not listed here are kept: human messages sent mid-turn arrive
/// as `queued_command` attachments, and unknown types fail open.
const CEREMONY_ATTACHMENTS: &[&str] = &[
    "agent_listing_delta",
    "auto_mode",
    "auto_mode_exit",
    "bash_output_audience_note",
    "batching_reminder_sent",
    "budget_usd",
    "command_permissions",
    "credential_org",
    "date",
    "date_change",
    "deferred_tools_delta",
    "diagnostics",
    "edited_text_file",
    "environment",
    "hook_cancelled",
    "hook_non_blocking_error",
    "hook_success",
    "hook_system_message",
    "instructions",
    "mcp_instructions_delta",
    "model",
    "nested_memory",
    "output_style",
    "output_style_instructions",
    "prompt_snapshot",
    "read_truncation_notice",
    "remote_session_change",
    "session_context",
    "silent_turn_reminder",
    "skill_listing",
    "task_reminder",
    "thinking_drop",
    "todo_reminder",
    "total_tokens_reminder",
    "ultrathink_effort",
];

/// Marker that opens this tool's own SessionStart orientation; a stale one is ceremony too.
pub const ORIENT_MARKER: &str = "[recompact orientation]";

pub fn attachment_type(r: &Value) -> Option<&str> {
    if rec_type(r) != "attachment" {
        return None;
    }
    r.pointer("/attachment/type").and_then(|v| v.as_str())
}

pub fn is_ceremony(r: &Value) -> bool {
    match attachment_type(r) {
        Some(t) if CEREMONY_ATTACHMENTS.contains(&t) => true,
        Some("hook_additional_context") => {
            serde_json::to_string(r.get("attachment").unwrap_or(&Value::Null))
                .is_ok_and(|s| s.contains(ORIENT_MARKER))
        }
        _ => false,
    }
}

/// A message the human typed while the agent was working. Claude Code delivers it as a
/// `queued_command` attachment rather than a user record, so it is the only model-visible copy
/// of that instruction ("lets not land anything yet, keep unmerged" was one): it must survive
/// every treatment verbatim, exactly like a user turn. Messages from peer agents carry
/// `origin.kind: "peer"` and `isMeta`, and are delivered content like any other.
pub fn is_human_queued(r: &Value) -> bool {
    attachment_type(r) == Some("queued_command")
        && r.pointer("/attachment/commandMode").and_then(|v| v.as_str()) == Some("prompt")
        && !r
            .pointer("/attachment/isMeta")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        && r.pointer("/attachment/origin/kind")
            .and_then(|v| v.as_str())
            .is_none_or(|k| k == "human")
}

pub fn human_queued_text(r: &Value) -> String {
    match r.pointer("/attachment/prompt") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Emission-time trimming, applied to every record assemble copies:
/// - persisted thinking is removed everywhere. It never reaches the model on resume (Claude Code
///   strips it; two resumes differing only in thinking hit the same prompt cache), and a thinking
///   block replayed after rewritten history is exactly what strict preserved-thinking checks
///   reject;
/// - outside the keep tail, ceremony attachments are dropped (see `CEREMONY_ATTACHMENTS`).
pub fn trim_record(r: &Value, outside_tail: bool) -> Masked {
    if outside_tail && is_ceremony(r) {
        return Masked::Dropped;
    }
    let has_thinking = r
        .pointer("/message/content")
        .and_then(|c| c.as_array())
        .is_some_and(|a| {
            a.iter().any(|b| {
                matches!(
                    b.get("type").and_then(|v| v.as_str()),
                    Some("thinking") | Some("redacted_thinking")
                )
            })
        });
    if !has_thinking {
        return Masked::Unchanged;
    }
    let mut m = r.clone();
    if let Some(blocks) = m.pointer_mut("/message/content").and_then(|c| c.as_array_mut()) {
        blocks.retain(|b| {
            !matches!(
                b.get("type").and_then(|v| v.as_str()),
                Some("thinking") | Some("redacted_thinking")
            )
        });
        if blocks.is_empty() {
            return Masked::Dropped;
        }
    }
    if let Some(obj) = m.as_object_mut() {
        obj.insert("recompactThinkingStripped".into(), Value::Bool(true));
    }
    Masked::Replaced(m)
}

/// Push a record through trimming, then (for the mask lane) masking.
fn emit_trimmed(out: &mut Vec<Value>, r: &Value, outside_tail: bool, mask: bool) {
    let trimmed = match trim_record(r, outside_tail) {
        Masked::Dropped => return,
        Masked::Unchanged => None,
        Masked::Replaced(v) => Some(v),
    };
    let base = trimmed.as_ref().unwrap_or(r);
    if !mask {
        out.push(base.clone());
        return;
    }
    match mask_record(base) {
        Masked::Unchanged => out.push(base.clone()),
        Masked::Replaced(v) => out.push(v),
        Masked::Dropped => {}
    }
}

/// Replace base64 image blocks with rehydratable text markers, leaving every text block intact.
/// Applied to user turns outside the keep tail: a user turn's TEXT is sacred (verify compares it
/// verbatim), but an old screenshot's bytes are the heaviest thing a session can pin — the model
/// has already acted on it, recent images stay, and the original file retains what is elided.
pub fn elide_images(r: &Value) -> Option<Value> {
    let mut m = r.clone();
    let mut count = 0usize;
    let sel = rec_uuid(r)
        .map(|u| u.get(..8).unwrap_or(u).to_string())
        .unwrap_or_default();
    if let Some(blocks) = m.pointer_mut("/message/content").and_then(|c| c.as_array_mut()) {
        for b in blocks.iter_mut() {
            if b.get("type").and_then(|v| v.as_str()) == Some("image") {
                let n = b
                    .pointer("/source/data")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                if n > IMAGE_EST_CHARS {
                    *b = json!({"type": "text", "text": format!("[recompact: image elided (~{} KB); rehydrate {sel} to recover it]", n / 1024)});
                    count += 1;
                }
            }
        }
    }
    if count > 0 {
        if let Some(obj) = m.as_object_mut() {
            obj.insert("recompactImagesElided".into(), json!(count));
        }
        Some(m)
    } else {
        None
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

/// Summary for a unit in which no agent acted (system notices, resume scaffolding). A model asked
/// to summarize nothing returns nothing, and one such unit used to abort a whole `continue` run
/// after every other summary had been paid for.
pub const EMPTY_UNIT_SUMMARY: &str =
    "(No agent actions in this span: only harness records such as system notices or resume scaffolding.)";

/// Does this unit contain anything an agent said or did?
pub fn unit_is_empty(records: &[Value], part: &[usize]) -> bool {
    part.iter().all(|&i| {
        let r = &records[i];
        if truthy(r, "recompactSynthetic")
            || truthy(r, "isCompactSummary")
            || is_human_queued(r)
            || delivered_kind(r).is_some()
        {
            return false;
        }
        match content(r) {
            Some(Value::Array(blocks)) => !blocks.iter().any(|b| {
                match b.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        rec_type(r) == "assistant"
                            && b.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.trim().is_empty())
                    }
                    Some("tool_use") | Some("tool_result") | Some("image") => true,
                    _ => false,
                }
            }),
            Some(Value::String(t)) => rec_type(r) != "assistant" || t.trim().is_empty(),
            _ => true,
        }
    })
}

/// This plugin's own skill body, injected when /recompact is invoked (~21k chars of procedure).
/// Once the compaction it drove has run, it is pure weight — and left in the tail it makes the
/// resumed model's most recent "instruction" the compaction procedure itself.
pub fn is_own_skill_body(r: &Value) -> bool {
    rec_type(r) == "user"
        && truthy(r, "isMeta")
        && first_text(r).is_some_and(|t| {
            t.starts_with("Base directory for this skill:") && t.contains("skills/recompact")
        })
}

/// The model-visible selector line closing every summary. The uuid prefix is the summary
/// record's own, which survives every later generation unchanged and resolves project-wide, so
/// `recall` never needs to know which file the reader is in. Part keys alone did not: all five
/// real recall calls in the month before this change used one, and all five failed.
pub fn summary_footer(key: &str, uuid: &str) -> String {
    format!("[recompact summary {key} · recall {}]", uuid.get(..8).unwrap_or(uuid))
}

/// Bring an inherited summary's footer to the current form, idempotently. Only the footer line
/// is touched; summary prose is never rewritten.
fn upgrade_footer(r: &mut Value) {
    let key = r
        .pointer("/recompactProvenance/part")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let uuid = rec_uuid(r).unwrap_or("").to_string();
    let Some(slot) = r.pointer_mut("/message/content/0/text") else {
        return;
    };
    let Some(text) = slot.as_str().map(str::to_string) else {
        return;
    };
    if text.contains(" · recall ") {
        return;
    }
    let body = match text.rfind("\n[recompact summary ") {
        Some(i) if text[i..].contains("rehydratable]") => text[..i].to_string(),
        _ => text,
    };
    *slot = Value::String(format!("{body}\n{}", summary_footer(&key, &uuid)));
}

/// Mechanical additions beneath a summary: files the unit changed, its errors verbatim, and the
/// identifiers later turns still use. Each line is only emitted for what the summary text does
/// not already contain.
pub fn augment_summary(
    records: &[Value],
    part: &[usize],
    summary: &str,
    mentions: &Mentions,
    skip: &HashSet<String>,
    root: Option<&str>,
) -> String {
    let rel = |p: &str| -> String { relative_to(p, root) };
    let mut out = summary.trim_end().to_string();
    let idx = segment_index(records, part);
    if let Some(files) = idx["files"].as_object() {
        let changed: Vec<String> = files
            .iter()
            .filter(|(_, roles)| {
                roles
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x == "edited" || x == "written"))
            })
            .map(|(p, _)| p.clone())
            .filter(|p| {
                let base = p.rsplit('/').next().unwrap_or(p);
                !summary.contains(base)
            })
            .collect();
        if !changed.is_empty() {
            let shown: Vec<String> = changed.iter().take(8).map(|p| format!("`{}`", rel(p))).collect();
            let more = changed.len().saturating_sub(8);
            out.push_str(&format!(
                "\n⟨carried⟩ files changed: {}{}",
                shown.join(", "),
                if more > 0 { format!(", +{more} more") } else { String::new() }
            ));
        }
    }
    for (tool, err) in error_evidence(records, part, 3, 300) {
        let probe: String = err.chars().take(40).collect();
        if !summary.contains(probe.trim()) {
            out.push_str(&format!("\n⟨carried⟩ error from {tool}: {err}"));
        }
    }
    let anchors = hindsight_anchors(records, part, mentions, &out, skip, 12, 480);
    if !anchors.is_empty() {
        let shown: Vec<String> = anchors.iter().map(|a| format!("`{}`", rel(a))).collect();
        out.push_str(&format!("\n⟨carried⟩ used later: {}", shown.join(", ")));
    }
    out
}

/// ACE's "context collapse": a ledger rewritten wholesale sheds items a pass at a time. Warn when
/// a new ledger drops identifiers the previous one carried, so the drop is a decision, not drift.
fn ledger_collapse_warning(records: &[Value], new_ledger: &str) {
    let Some(old) = records
        .iter()
        .rev()
        .find(|r| truthy(r, "recompactLedger"))
        .and_then(|r| r.pointer("/message/content/0/text").and_then(|v| v.as_str()))
    else {
        return;
    };
    let (mut a, mut b) = (Vec::new(), Vec::new());
    identifiers(old, &mut a);
    identifiers(new_ledger, &mut b);
    let kept: HashSet<&String> = b.iter().collect();
    let mut dropped: Vec<&String> = a.iter().filter(|x| !kept.contains(x)).collect();
    dropped.sort();
    dropped.dedup();
    if !dropped.is_empty() {
        let shown: Vec<String> = dropped.iter().take(12).map(|x| format!("`{x}`")).collect();
        eprintln!(
            "warning: the new ledger drops {} identifier(s) the previous ledger carried: {}{}. Confirm they are obsolete",
            dropped.len(),
            shown.join(", "),
            if dropped.len() > 12 { ", …" } else { "" }
        );
    }
}

/// The session's display title, carried to the twin with a generation suffix. Twins used to carry
/// none, so `claude --resume "<title>"` silently reopened the uncompacted original.
fn title_record(records: &[Value], new_session: &str, generation: u64) -> Option<Value> {
    let title = records.iter().rev().find_map(|r| match rec_type(r) {
        "custom-title" => r.get("customTitle").and_then(|v| v.as_str()),
        "agent-name" => r.get("agentName").and_then(|v| v.as_str()),
        _ => None,
    });
    let title = title.or_else(|| {
        records
            .iter()
            .rev()
            .find(|r| rec_type(r) == "ai-title")
            .and_then(|r| r.get("aiTitle").and_then(|v| v.as_str()))
    })?;
    let base = match title.rfind(" (recompact") {
        Some(i) if title.ends_with(')') => &title[..i],
        _ => title,
    };
    Some(json!({
        "type": "custom-title",
        "customTitle": format!("{base} (recompact {generation})"),
        "sessionId": new_session,
    }))
}

/// Core of assemble, returning the new session id and output path (None for --plan previews).
fn run_assemble(args: &[String]) -> Result<Option<(String, PathBuf)>, i32> {
    let (pos, opts) = parse_opts(args);
    let flag = |k: &str| opts.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
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
    let plan_only = flag("plan");
    if plan_only && target.is_none() {
        eprintln!("error: --plan requires --target <tokens>");
        return Err(2);
    }
    // Error-bearing units summarize by default: their error text rides beneath the summary
    // verbatim. `--summarize-errors` (the old opt-in) is accepted and now a no-op;
    // `--error-floor` restores the hard floor at mask.
    let allow_error_summarize = !flag("error-floor");
    // Content hashes of units a summarizer was asked for and never covered (continue writes
    // them): those mask instead of failing the run. Only those — a unit that appeared because
    // the session grew mid-run must still fail validation, so continue re-syncs and summarizes it.
    let mask_units: HashSet<String> = opts
        .get("mask-units")
        .and_then(|v| v.as_str())
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .collect();
    let epochs_on = flag("epochs") && target.is_some();

    let (active, _off_path) = select_active(load_jsonl(&src));
    let calib = calibrate_opts(&active, &opts);
    let prior_preamble = active
        .iter()
        .rev()
        .find(|r| truthy(r, "recompactPreamble"))
        .cloned();
    // Old orientation preambles are pure boilerplate for the PREVIOUS generation; strip them
    // wholesale (and mint a fresh one below), the same way every unit-hashing path must.
    let records: Vec<Value> = active
        .into_iter()
        .filter(|r| !truthy(r, "recompactPreamble"))
        .collect();
    let (head, segs) = segment(&records);
    let mut plans = plan_ex(&records, &segs, keep, epochs_on);
    let seg_parts: Vec<Vec<Vec<usize>>> = segs
        .iter()
        .map(|sg| split_parts(&records, sg, split_threshold))
        .collect();
    refine_plans(&records, &segs, &mut plans, &seg_parts, tail_budget(&opts));

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
    let mut key_hashes: HashMap<String, String> = HashMap::new();
    let mut seg_keys: Vec<Vec<String>> = Vec::new();
    let mut empty_units: HashSet<String> = HashSet::new();
    for (s, seg) in segs.iter().enumerate() {
        let parts = &seg_parts[s];
        let split = parts.len() > 1;
        let mut keys = Vec::new();
        for (p, part) in parts.iter().enumerate() {
            let key = if split { format!("{s}.{p}") } else { s.to_string() };
            key_hashes.insert(key.clone(), part_content_hash(&records, seg, part, p == 0));
            if unit_is_empty(&records, part) {
                empty_units.insert(key.clone());
            }
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
    // (text, source): source is "provided", "cache", or "mechanical".
    let resolve = |key: &str| -> Option<(String, &'static str)> {
        summaries
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| (s.to_string(), "provided"))
            .or_else(|| {
                key_hashes
                    .get(key)
                    .and_then(|h| cache.get(h))
                    .and_then(|v| v.as_str())
                    .map(|s| (s.to_string(), "cache"))
            })
            .or_else(|| {
                empty_units
                    .contains(key)
                    .then(|| (EMPTY_UNIT_SUMMARY.to_string(), "mechanical"))
            })
    };

    // With --target, the planner decides per-unit treatments; the budget is an objective, the
    // salience floors are constraints, and floors win. The target is context tokens (what
    // /context shows); the planner works in conversation tokens.
    let epoch_map = if epochs_on {
        epoch_digests(&records, &segs, &seg_parts, &seg_keys, keep)
    } else {
        HashMap::new()
    };
    let budget: Option<BudgetPlan> = target.map(|t| {
        plan_budget_calibrated(
            &records,
            &segs,
            &plans,
            &seg_parts,
            &seg_keys,
            t.saturating_sub(calib.overhead),
            mode == "summarize",
            allow_error_summarize,
            &epoch_map,
            &calib,
            unknown_summary_tokens(&cache, &calib),
            |key| resolve(key).map(|(s, _)| calib.tokens(s.len()) + SUMMARY_OVERHEAD_TOKENS),
        )
    });
    if plan_only {
        print_budget_plan(budget.as_ref().expect("checked above"), &calib);
        return Ok(None); // preview only: nothing validated, nothing written
    }
    let mut treatments: Option<HashMap<String, Treatment>> = budget
        .as_ref()
        .map(|b| b.units.iter().map(|u| (u.key.clone(), u.treatment)).collect());

    // Validate: every unit that will be summarized has a summary (masking needs none).
    let mut fallbacks: Vec<String> = Vec::new();
    if let Some(b) = &budget {
        let (maskable, missing): (Vec<String>, Vec<String>) = b
            .need_summaries
            .iter()
            .cloned()
            .partition(|k| key_hashes.get(k).is_some_and(|h| mask_units.contains(h)));
        if !missing.is_empty() {
            eprintln!(
                "error: the --target plan needs summaries for {missing:?}; run with --plan to preview, then provide them"
            );
            return Err(1);
        }
        if !maskable.is_empty() {
            if let Some(tr) = treatments.as_mut() {
                for k in &maskable {
                    tr.insert(k.clone(), Treatment::Mask);
                }
            }
            eprintln!("warning: no summary for {maskable:?}; masking those units instead");
            fallbacks = maskable;
        }
    } else if mode == "summarize" {
        let mut missing: Vec<String> = Vec::new();
        for (s, p) in plans.iter().enumerate() {
            if !p.needs_summary {
                continue;
            }
            for (pi, key) in seg_keys[s].iter().enumerate() {
                if !p.pinned_parts.contains(&pi) && resolve(key).is_none() {
                    missing.push(key.clone());
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
    if let Some(l) = &ledger_text {
        ledger_collapse_warning(&records, l);
    }
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
    // Records assemble mints carry the model the session last ran: a resume that reads the model
    // from the transcript must not land on the one the session started with.
    let profile = ResumeProfile::of(&records);
    let model = profile.model.clone().unwrap_or_else(|| "claude-opus-4-7".to_string());
    let mentions = Mentions::build(&records);
    // Carried paths are shown relative to the directory the session's edits share.
    let path_root = session_path_root(&records);
    let mut turn_of_record = vec![0usize; records.len()];
    for (s, seg) in segs.iter().enumerate() {
        turn_of_record[seg.user_idx] = s + 1;
        for &i in &seg.activity {
            turn_of_record[i] = s + 1;
        }
    }
    let mut workdirs: Vec<String> = records
        .iter()
        .filter_map(|r| r.get("cwd").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    workdirs.sort();
    workdirs.dedup();
    let anchor_skip = anchor_skip_set(&mentions, &turn_of_record, segs.len(), &workdirs);

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
        let uuid = uuid_v4();
        let text = format!(
            "{}\n{}",
            augment_summary(&records, part, &summary, &mentions, &anchor_skip, path_root.as_deref()),
            summary_footer(key, &uuid)
        );
        json!({
            "parentUuid": Value::Null,            // fixed up in the rechain pass
            "isSidechain": false,
            "userType": "external",
            "type": "assistant",
            "uuid": uuid,
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
                "content": [{ "type": "text", "text": text }]
            }
        })
    };

    // Orientation preamble: the one message every model resumed into this file is guaranteed to
    // see, emitted LAST — the recency peak, immediately before the resumed model's own first
    // turn. It carries a mechanical brief of where the work stood (files, git, PRs, the user's
    // standing instructions verbatim), because the envelope (uuids, provenance, snapshots) is
    // never shown at inference time.
    let last_cwd = records
        .iter()
        .rev()
        .find_map(|r| r.get("cwd").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let snapshot = git_snapshot(&last_cwd);
    // Older twins never stamped a generation; each generation that left summaries behind shows up
    // as a distinct provenance source.
    let sources: HashSet<&str> = records
        .iter()
        .filter_map(|r| r.pointer("/recompactProvenance/sourceSessionId").and_then(|v| v.as_str()))
        .collect();
    let generation = prior_preamble
        .as_ref()
        .and_then(|p| p.get("recompactGeneration").and_then(|v| v.as_u64()))
        .unwrap_or(0)
        .max(sources.len() as u64)
        .max(u64::from(records.iter().any(|r| truthy(r, "recompactMasked"))))
        + 1;
    let assembled_at = now_unix();
    let preamble_text = preamble_text(&PreambleInput {
        path_root: path_root.as_deref(),
        new_session: &new_session,
        source_session: &orig_session_id,
        generation,
        assembled_at,
        snapshot: snapshot.as_ref(),
        brief: state_brief(&records),
        constraints: constraint_lane(&records),
        last_check: last_check(&records),
    });
    let preamble = json!({
        "parentUuid": Value::Null,
        "isSidechain": false,
        "userType": "external",
        "type": "assistant",
        "uuid": uuid_v4(),
        // Last record, so it carries the latest timestamp in the file (the `last-prompt` tail
        // has none) — a first-record timestamp here would read as out-of-order history.
        "timestamp": records.iter().rev().find_map(|r| r.get("timestamp").cloned()).unwrap_or(Value::Null),
        "sessionId": new_session,
        "cwd": cwd,
        "gitBranch": git_branch,
        "version": version,
        "recompactPreamble": true,
        "recompactAssembledAt": assembled_at,
        "recompactGeneration": generation,
        "recompactSource": {"sessionId": orig_session_id, "path": src_abs},
        "recompactSnapshot": snapshot.clone().unwrap_or(Value::Null),
        "recompactCalibration": profile.stamp(),
        "message": {
            "id": format!("msg_recompact_{}", uuid_v4().replace('-', "")),
            "role": "assistant",
            "model": model,
            "type": "message",
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": preamble_text }]
        }
    });

    let keep_rec = |i: usize| -> bool {
        rec_uuid(&records[i]).is_some()
            && !(drop_old_ledgers && truthy(&records[i], "recompactLedger"))
            && !is_own_skill_body(&records[i])
    };
    let mut out: Vec<Value> = Vec::new();

    // Head: keep only records that carry a uuid (drop ephemeral scaffolding like queue-operation).
    for &i in &head {
        if keep_rec(i) {
            emit_trimmed(&mut out, &records[i], true, false);
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

    let (mut n_provided, mut n_cache, mut n_mech) = (0usize, 0usize, 0usize);
    for (s, seg) in segs.iter().enumerate() {
        if s == tail_start {
            if let Some(l) = ledger_pending.take() {
                out.push(l);
            }
        }
        let outside = s < tail_start;
        // User turns are pinned, but their embedded screenshots are not: outside the keep tail
        // the image bytes give way to markers (text verbatim; the original retains the pixels).
        if outside {
            match elide_images(&records[seg.user_idx]) {
                Some(v) => out.push(v),
                None => out.push(records[seg.user_idx].clone()),
            }
        } else {
            out.push(records[seg.user_idx].clone());
        }
        if plans[s].kept_verbatim {
            for &i in &seg.activity {
                if keep_rec(i) {
                    emit_trimmed(&mut out, &records[i], outside, false);
                }
            }
            continue;
        }
        for (p, part) in seg_parts[s].iter().enumerate() {
            let key = &seg_keys[s][p];
            let pinned = plans[s].pinned_parts.contains(&p);
            let treatment = if pinned || !plans[s].needs_summary {
                Treatment::Verbatim
            } else if let Some(tr) = &treatments {
                tr.get(key.as_str()).copied().unwrap_or(Treatment::Verbatim)
            } else if mode == "mask" {
                Treatment::Mask
            } else {
                Treatment::Summarize
            };
            match treatment {
                Treatment::Verbatim | Treatment::Mask => {
                    for &i in part {
                        if keep_rec(i) {
                            emit_trimmed(&mut out, &records[i], !pinned, treatment == Treatment::Mask);
                        }
                    }
                }
                Treatment::Summarize => {
                    let (text, source) = resolve(key).expect("validated above");
                    match source {
                        "provided" => n_provided += 1,
                        "cache" => n_cache += 1,
                        _ => n_mech += 1,
                    }
                    // One synthetic record per part, carrying provenance to the exact raw records
                    // it replaced, so recall can recover the verbatim originals.
                    out.push(make_synthetic(seg, part, key, p == 0, text));
                    // What the human typed mid-turn is a user turn in all but record type.
                    for &i in part {
                        if is_human_queued(&records[i]) {
                            out.push(records[i].clone());
                        }
                    }
                }
            }
        }
    }

    if let Some(l) = ledger_pending.take() {
        out.push(l); // keep >= segment count: the ledger still lands, at the end
    }
    // The preamble closes the file. It follows a completed turn (or the head, on a segment-less
    // file), so it never lands between a tool_use and its tool_result.
    out.push(preamble);

    // Summaries inherited from older generations carry older footers (or none); bring every one
    // to the current selector form. Idempotent, and the summary prose itself is never rewritten.
    for r in out.iter_mut() {
        if truthy(r, "recompactSynthetic") {
            upgrade_footer(r);
        }
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
            if obj.contains_key("session_id") {
                obj.insert("session_id".into(), Value::String(new_session.clone()));
            }
            // A twin is not a branch: copied `forkedFrom` fields would make its source's parent
            // treat it as one (lineage is tracked in the sidecar).
            obj.remove("forkedFrom");
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

    if let Some(t) = title_record(&records, &new_session, generation) {
        out.push(t);
    }
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
    if let Some(cp) = &cache_path {
        let mut cache = cache.clone();
        for keys in &seg_keys {
            for key in keys {
                if let (Some(text), Some(h)) = (
                    summaries.get(key).and_then(|v| v.as_str()),
                    key_hashes.get(key),
                ) {
                    cache.insert(h.clone(), Value::String(text.to_string()));
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

    let (kept_ids, total_ids, missing_ids) = retention(
        &mentions,
        &visible_identifiers(&out, &mentions.vocab),
        &|i| turn_of_record.get(i).copied().unwrap_or(0),
    );
    let pinned_tail: usize = plans.iter().map(|p| p.pinned_parts.len()).sum();
    eprintln!(
        "assemble ({mode}): {} → {} records; context ~{} → ~{} tokens ({})",
        records.len(),
        out.len(),
        calib.context_tokens(&records),
        calib.context_tokens(&out),
        calib.describe()
    );
    eprintln!(
        "  summaries: {n_provided} provided, {n_cache} from cache, {n_mech} mechanical{}; last {keep} turn(s) verbatim{}",
        if fallbacks.is_empty() {
            String::new()
        } else {
            format!(", {} masked for lack of one", fallbacks.len())
        },
        if pinned_tail > 0 {
            format!(" (the oversized tail keeps its newest {pinned_tail} part(s); --tail-budget adjusts)")
        } else {
            String::new()
        }
    );
    if total_ids > 0 {
        eprintln!(
            "  retention: {kept_ids}/{total_ids} cross-referenced identifiers still visible ({:.1}%){}",
            100.0 * kept_ids as f64 / total_ids as f64,
            if missing_ids.is_empty() {
                String::new()
            } else {
                format!(
                    "; recall-only, e.g. {}",
                    missing_ids.iter().map(|m| format!("`{}`", truncate(m, 40))).collect::<Vec<_>>().join(", ")
                )
            }
        );
    }
    eprintln!(
        "  new sessionId: {}\n  wrote: {}\n  resume with: {}",
        new_session,
        out_path.display(),
        resume_command(&new_session, &profile.flags())
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
fn session_mtime(dir: &Path, id: &str) -> Option<std::time::SystemTime> {
    fs::metadata(dir.join(format!("{id}.jsonl")))
        .and_then(|m| m.modified())
        .ok()
}

/// Sessions in `dir` that `/branch` (or `--fork-session`) copied from another: child -> parent.
/// The copy's records carry `forkedFrom.sessionId`, and the lineage sidecar never hears of it —
/// so without this, continue would compact the stale parent a user had branched away from.
pub fn fork_parents(dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(mut f) = fs::File::open(&p) else { continue };
        let mut buf = vec![0u8; 256 * 1024];
        let n = f.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        if !head.contains("\"forkedFrom\"") {
            continue;
        }
        let parent = head.lines().find_map(|l| {
            serde_json::from_str::<Value>(l)
                .ok()?
                .pointer("/forkedFrom/sessionId")?
                .as_str()
                .map(str::to_string)
        });
        if let Some(parent) = parent {
            let child = stem_of(&p);
            if parent != child {
                out.insert(child, parent);
            }
        }
    }
    out
}

pub fn lineage_latest(dir: &Path, start: &str) -> String {
    let m = lineage_load(dir);
    let forks = fork_parents(dir);
    let mut cur = start.to_string();
    for _ in 0..1000 {
        // A descendant is only followed while it is FRESHER than its parent: a twin cut
        // mid-session goes stale the moment the parent session keeps appending (its unique
        // turns are missing from the twin), and resuming it would silently drop them from the
        // continued thread. A stale or deleted twin is skipped; continue then re-compacts from
        // the parent, which the summary cache makes cheap. Branch copies compete on the same
        // terms: whichever copy of the thread moved last is its live head.
        let cur_mtime = session_mtime(dir, &cur);
        let fresh = |k: &str| match (session_mtime(dir, k), cur_mtime) {
            (Some(child), Some(parent)) => child >= parent,
            (child, _) => child.is_some(),
        };
        let twin = m
            .iter()
            .filter(|(k, v)| v.get("parent").and_then(|p| p.as_str()) == Some(cur.as_str()) && fresh(k))
            .max_by_key(|(_, v)| v.get("at").and_then(|a| a.as_u64()).unwrap_or(0))
            .map(|(k, _)| k.clone());
        let branch = forks
            .iter()
            .filter(|(k, p)| p.as_str() == cur && fresh(k))
            .max_by_key(|(k, _)| session_mtime(dir, k))
            .map(|(k, _)| k.clone());
        let next = match (twin, branch) {
            (Some(t), Some(b)) => Some(if session_mtime(dir, &b) > session_mtime(dir, &t) { b } else { t }),
            (t, b) => t.or(b),
        };
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
        // A bare id: the cwd's project dir first, then every project dir — a session created in
        // the main checkout and resumed from a worktree (or run from elsewhere) lives in another.
        let cwd_dir = project_dir_from_cwd();
        let hint = cwd_dir.as_ref().map(|d| d.join(format!("{arg}.jsonl")));
        if let Some(found) = locate_session(hint.as_deref(), arg) {
            let dir = found.parent().unwrap_or(Path::new(".")).to_path_buf();
            return Ok((dir, arg.to_string()));
        }
        match cwd_dir {
            Some(dir) if dir.exists() => Ok((dir, arg.to_string())),
            _ => {
                eprintln!("error: {arg} is not a file, and no session by that id exists under ~/.claude/projects");
                Err(1)
            }
        }
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
    build_units_ex(records, keep, split, false)
}

pub fn build_units_ex(records: &[Value], keep: usize, split: usize, epochs: bool) -> Units {
    build_units_full(records, keep, split, epochs, DEFAULT_TAIL_BUDGET)
}

/// Units exactly as assemble will cut them, tail budget included — continue plans on these and
/// then hands the same parameters to assemble, so the two can never disagree about unit keys.
pub fn build_units_full(
    records: &[Value],
    keep: usize,
    split: usize,
    epochs: bool,
    tail_budget_tokens: usize,
) -> Units {
    let (_, segs) = segment(records);
    let mut plans = plan_ex(records, &segs, keep, epochs);
    let seg_parts: Vec<Vec<Vec<usize>>> = segs
        .iter()
        .map(|sg| split_parts(records, sg, split))
        .collect();
    refine_plans(records, &segs, &mut plans, &seg_parts, tail_budget_tokens);
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

// ------------------------------------------------------------------------- epoch consolidation

/// Leniently-parsed session files keyed by path, shared across one consolidation pass. Lenient on
/// purpose: a missing or corrupt raw file must make its epoch unresolvable (stays verbatim), not
/// kill the run the way load_jsonl's hard exit would.
type RawCache = HashMap<String, Vec<Value>>;

fn raw_records<'a>(cache: &'a mut RawCache, path: &str) -> &'a [Value] {
    cache.entry(path.to_string()).or_insert_with(|| {
        fs::read_to_string(path)
            .map(|s| {
                s.lines()
                    .filter_map(|l| serde_json::from_str(l.trim()).ok())
                    .collect()
            })
            .unwrap_or_default()
    })
}

const EPOCH_MAX_DEPTH: usize = 8;

/// Render the RAW records behind one synthetic summary into digest lines, following provenance
/// through earlier generations until ground truth. Summary text is never rendered: feeding a
/// summary to the summarizer is the drift-compounding loss the whole design forbids. Returns
/// false when any hop cannot be resolved — the caller then leaves the unit verbatim.
fn epoch_lines(
    cache: &mut RawCache,
    synth: &Value,
    depth: usize,
    lines: &mut Vec<String>,
) -> bool {
    if depth >= EPOCH_MAX_DEPTH {
        return false;
    }
    let Some(prov) = synth.get("recompactProvenance") else {
        return false;
    };
    let Some(srcp) = resolve_source(prov).map(|p| p.to_string_lossy().into_owned()) else {
        return false;
    };
    let covered: HashSet<String> = prov
        .get("coveredUuids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if covered.is_empty() {
        return false;
    }
    let raw: Vec<Value> = raw_records(cache, &srcp)
        .iter()
        .filter(|r| rec_uuid(r).map_or(false, |u| covered.contains(u)))
        .cloned()
        .collect();
    if raw.is_empty() {
        return false; // file unreadable, or none of the covered uuids found
    }
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for r in &raw {
        if let Some(blocks) = r.pointer("/message/content").and_then(|c| c.as_array()) {
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
    for r in &raw {
        if truthy(r, "recompactSynthetic") {
            if !epoch_lines(cache, r, depth + 1, lines) {
                return false;
            }
            continue;
        }
        if truthy(r, "recompactLedger") {
            continue;
        }
        for a in render_record(r, &tool_names, &mut seen) {
            let kind = a["kind"].as_str().unwrap_or("");
            let line = match kind {
                "assistant_text" => Some(format!(
                    "ASSISTANT: {}",
                    truncate(a["text"].as_str().unwrap_or(""), 400)
                )),
                "tool_use" => Some(format!(
                    "TOOL {} {}",
                    a["name"].as_str().unwrap_or("?"),
                    truncate(&a["input"].to_string(), 100)
                )),
                "tool_result" => Some(format!(
                    "RESULT[{},{}]: {}",
                    a["status"].as_str().unwrap_or("?"),
                    a["chars"].as_u64().unwrap_or(0),
                    truncate(a["result"].as_str().unwrap_or(""), 120)
                )),
                "delivered_message" => Some(format!(
                    "AGENT-REPORT: {}",
                    truncate(a["text"].as_str().unwrap_or(""), 300)
                )),
                _ => None,
            };
            if let Some(l) = line {
                lines.push(l);
            }
        }
    }
    true
}

/// For every non-tail part made entirely of this tool's own summaries: key → Some(digest built
/// from the raw records their provenance covers), or None when any provenance hop is
/// unresolvable (that part then stays verbatim via the planner's pin floor). Deterministic on
/// the session file + raw files, so continue and assemble derive identical eligibility.
pub fn epoch_digests(
    records: &[Value],
    segs: &[Segment],
    seg_parts: &[Vec<Vec<usize>>],
    seg_keys: &[Vec<String>],
    keep: usize,
) -> HashMap<String, Option<String>> {
    let mut out = HashMap::new();
    let mut cache: RawCache = HashMap::new();
    let tail_start = segs.len().saturating_sub(keep);
    for (s, parts) in seg_parts.iter().enumerate() {
        if s >= tail_start {
            break;
        }
        for (p, part) in parts.iter().enumerate() {
            if part.is_empty()
                || !part
                    .iter()
                    .all(|&i| truthy(&records[i], "recompactSynthetic"))
            {
                continue;
            }
            let mut lines = vec![
                format!(
                    "USER ASKED: {}",
                    truncate(&user_text(&records[segs[s].user_idx]), 300)
                ),
                "EARLIER, ALREADY-SUMMARIZED WORK — below is the RAW activity it covered. Write ONE consolidated recap of the whole span.".to_string(),
            ];
            let ok = part
                .iter()
                .all(|&i| epoch_lines(&mut cache, &records[i], 0, &mut lines));
            out.insert(
                seg_keys[s][p].clone(),
                if ok {
                    Some(truncate(&lines.join("\n"), DIGEST_CAP))
                } else {
                    None
                },
            );
        }
    }
    out
}

pub const SUMMARIZER_RUBRIC: &str = "You are compacting a Claude Code session transcript. For EACH unit below, write the replacement summary in first person past tense, as the assistant's own recap, so that the NEXT user turn still makes sense after the raw activity is gone. Cover: what was asked; what I did; the outcome; decisions and why; approaches tried and rejected, with the reason (a successor that does not know X failed will try X again); and anything left unfinished at the end of the unit. Quote exact values, names, numbers, ids, and commands verbatim — paraphrase is where drift starts. Grade every outcome: write VERIFIED only when the activity shows proof (exit 0, test output, a query result), OBSERVED for partial or interrupted output, CLAIMED when I only asserted it; a killed, timed-out, or erroring command is never a success. Files changed and error text are appended beneath your summary mechanically, so spend your words on reasoning, decisions, and outcomes rather than lists. 3 to 8 sentences per unit. Return ONLY a JSON object mapping each unit key to its summary string.";

const BATCH_MAX_UNITS: usize = 10;
const BATCH_MAX_CHARS: usize = 120_000;
const SUMMARIZE_WAVES: usize = 3;

#[derive(Clone, Debug)]
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
    // One fixed, empty cwd: Claude Code creates a project dir per cwd even without persistence,
    // and a fresh cwd per call left a directory behind for every batch.
    let tmp = std::env::temp_dir().join("recompact-summarizer");
    let _ = fs::create_dir_all(&tmp);
    let mut child = Command::new(bin)
        .current_dir(&tmp)
        .args(["-p", "--model", model, "--strict-mcp-config", "--no-session-persistence"])
        // Our own hooks stay out of the summarizer, and it must never signal a launcher.
        .env("RECOMPACT_INTERNAL", "1")
        .env_remove("RECOMPACT_SHELL")
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
    if !out.status.success() {
        return Err(format!("{bin} exited {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn extract_json_object(s: &str) -> Option<Map<String, Value>> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end < start {
        return None; // truncated output: a closing brace before the first opening one
    }
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
/// units, waves of concurrent calls, one retry round for stragglers. Returns key -> summary, and
/// fails if any unit is still missing.
pub fn headless_summarize(
    units: &[(String, f32, String)],
    cfg: &SummarizeCfg,
) -> Result<HashMap<String, String>, String> {
    let (result, missing) = headless_summarize_partial(units, cfg, &mut |_| {});
    if !missing.is_empty() {
        return Err(format!("missing summaries after retry: {missing:?}"));
    }
    Ok(result)
}

/// As `headless_summarize`, but hands every wave's results to `on_wave` as they land (so a
/// caller can persist them — a run that fails on its last unit used to lose every summary it had
/// paid for) and returns whatever is still missing instead of failing.
pub fn headless_summarize_partial(
    units: &[(String, f32, String)],
    cfg: &SummarizeCfg,
    on_wave: &mut dyn FnMut(&HashMap<String, String>),
) -> (HashMap<String, String>, Vec<String>) {
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
            on_wave(&result);
            eprintln!("summarize: {}/{} units done", result.len(), units.len());
        }
    }
    let missing: Vec<String> = units
        .iter()
        .map(|(k, _, _)| k.clone())
        .filter(|k| !result.contains_key(k))
        .collect();
    (result, missing)
}

// ---------------------------------------------------------------------- continue (shared core)

#[derive(Clone, Debug)]
pub struct ContinueOpts {
    pub threshold: usize,
    pub keep: Option<String>,
    pub split: Option<String>,
    pub summarize: Option<SummarizeCfg>,
    /// `--tail-budget`, passed through verbatim so continue and assemble cut identical units.
    pub tail_budget: Option<String>,
    /// `--error-floor`: keep error-bearing units at mask instead of summarizing them.
    pub error_floor: bool,
    /// `--overhead`: system+tools tokens of the environment the twin resumes into.
    pub overhead: Option<String>,
    /// `--target`: size to compact toward; defaults to the threshold.
    pub target: Option<usize>,
    /// `--force`: compact even when under the threshold (an explicit request).
    pub force: bool,
}

impl ContinueOpts {
    pub fn target(&self) -> usize {
        self.target.unwrap_or(self.threshold)
    }
}

/// Parse the continue/shell options shared by both commands.
pub fn continue_opts_from(opts: &Map<String, Value>) -> ContinueOpts {
    ContinueOpts {
        threshold: opts
            .get("threshold")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_THRESHOLD),
        keep: opts.get("keep").and_then(|v| v.as_str()).map(String::from),
        split: opts.get("split").and_then(|v| v.as_str()).map(String::from),
        summarize: summarize_cfg_from_opts(opts),
        tail_budget: opts.get("tail-budget").and_then(|v| v.as_str()).map(String::from),
        error_floor: opts.get("error-floor").and_then(|v| v.as_bool()).unwrap_or(false),
        overhead: opts.get("overhead").and_then(|v| v.as_str()).map(String::from),
        target: opts
            .get("target")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok()),
        force: opts.get("force").and_then(|v| v.as_bool()).unwrap_or(false),
    }
}

/// Default compaction threshold, in context tokens (what `/context` reports).
pub const DEFAULT_THRESHOLD: usize = 120_000;

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

/// Full-ladder pre-pass: plan exactly as assemble will (same units, same pricing: real lengths
/// for cached summaries, the mechanical text for empty units, the cache's mean for the rest),
/// summarize what the plan wants into the content-hash cache, and re-plan with the real lengths
/// until it wants nothing new. A single pass is not enough: when real summaries cost more than
/// the estimate, assemble's plan shifts onto units that were never summarized and refuses.
/// Returns the content hashes of units the summarizer never covered; assemble masks them.
/// `prewarm` runs this alone, ahead of time, so a later compaction finds the cache warm.
pub fn summarize_for_plan(
    active: &[Value],
    calib: &Calib,
    cfg: &SummarizeCfg,
    o: &ContinueOpts,
    cache_path: &Path,
) -> HashSet<String> {
    let keep: usize = o.keep.as_deref().and_then(|s| s.parse().ok()).unwrap_or(1);
    let split: usize = o
        .split
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SPLIT_THRESHOLD);
    let tail_budget_tokens: usize = o
        .tail_budget
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TAIL_BUDGET);
    let u = build_units_full(active, keep, split, true, tail_budget_tokens);
    let epoch_map = epoch_digests(active, &u.segs, &u.seg_parts, &u.seg_keys, keep);
    let empty: HashSet<String> = u
        .seg_parts
        .iter()
        .zip(u.seg_keys.iter())
        .flat_map(|(parts, keys)| {
            parts
                .iter()
                .zip(keys.iter())
                .filter(|(part, _)| unit_is_empty(active, part))
                .map(|(_, k)| k.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    let empty_cost = calib.tokens(EMPTY_UNIT_SUMMARY.len()) + SUMMARY_OVERHEAD_TOKENS;
    let mut failed: HashSet<String> = HashSet::new(); // content hashes
    const ROUNDS: usize = 5;
    for round in 1..=ROUNDS {
        let cache: Map<String, Value> = fs::read_to_string(cache_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let price = |key: &str| -> Option<usize> {
            if let Some(text) = u.key_hashes.get(key).and_then(|h| cache.get(h)).and_then(|v| v.as_str()) {
                return Some(calib.tokens(text.len()) + SUMMARY_OVERHEAD_TOKENS);
            }
            empty.contains(key).then_some(empty_cost)
        };
        let b = plan_budget_calibrated(
            &active,
            &u.segs,
            &u.plans,
            &u.seg_parts,
            &u.seg_keys,
            o.target().saturating_sub(calib.overhead),
            true,
            !o.error_floor,
            &epoch_map,
            &calib,
            unknown_summary_tokens(&cache, &calib),
            price,
        );
        let mut work: Vec<(String, f32, String)> = Vec::new();
        for unit in &b.units {
            if unit.treatment != Treatment::Summarize || price(&unit.key).is_some() {
                continue;
            }
            let h = &u.key_hashes[&unit.key];
            if failed.contains(h) {
                continue;
            }
            let p: usize = unit
                .key
                .split('.')
                .nth(1)
                .and_then(|x| x.parse().ok())
                .unwrap_or(0);
            let seg = &u.segs[unit.seg];
            let part = &u.seg_parts[unit.seg][p];
            // Epoch units summarize from RAW (via provenance); everything else from
            // the current records.
            let digest = match epoch_map.get(&unit.key) {
                Some(Some(d)) => d.clone(),
                _ => unit_digest(active, seg, part),
            };
            work.push((unit.key.clone(), unit.salience, digest));
        }
        if work.is_empty() {
            break;
        }
        if round == ROUNDS {
            // Out of rounds: whatever is still unwritten is masked instead.
            failed.extend(work.iter().map(|(k, _, _)| u.key_hashes[k].clone()));
            break;
        }
        eprintln!(
            "continue: summarizing {} unit(s) with {}{}{}",
            work.len(),
            cfg.model,
            cfg.escalate_with
                .as_deref()
                .map(|m| format!(" (escalating salience ≥ {} to {m})", cfg.escalate_above))
                .unwrap_or_default(),
            if round > 1 { format!(" (re-plan {round})") } else { String::new() }
        );
        let mut cache = cache;
        let mut persist = |sums: &HashMap<String, String>| {
            for (k, text) in sums {
                if let Some(h) = u.key_hashes.get(k) {
                    cache.insert(h.clone(), Value::String(text.clone()));
                }
            }
            if fs::write(
                cache_path,
                serde_json::to_string_pretty(&Value::Object(cache.clone())).unwrap(),
            )
            .is_err()
            {
                eprintln!("continue: warning: cannot write summary cache {}", cache_path.display());
            }
        };
        let (_sums, missing) = headless_summarize_partial(&work, cfg, &mut persist);
        if !missing.is_empty() {
            eprintln!(
                "continue: no summary came back for {missing:?}; those units will be masked instead"
            );
            failed.extend(missing.iter().filter_map(|k| u.key_hashes.get(k).cloned()));
        }
    }
    failed
}

/// Core continuation step, shared by `continue` and `shell`: resolve the newest compacted
/// descendant; when over threshold, compact toward it (mask-only, or the full ladder when a
/// summarizer is configured); verify with rollback; churn-guard.
///
/// A LIVE session can grow while summaries are being written, shifting segment keys between the
/// planning snapshot and assemble's re-read (observed in production: "plan needs summaries for
/// [42, 43]" after minutes of summarizing). So the plan → summarize → assemble → verify cycle
/// RETRIES, re-syncing to the file's current state on each attempt; the content-hash cache makes
/// all previously-summarized material free, so a retry only pays for the turns that appeared in
/// the gap. Summaries are written to the cache after every wave, and a unit the summarizer never
/// covers is masked rather than failing the run. Always returns a resumable id, plus an exit code
/// for the CLI.
pub fn continue_session(dir: &Path, start_id: &str, o: &ContinueOpts) -> (String, i32) {
    let latest = lineage_latest(dir, start_id);
    let latest_file = dir.join(format!("{latest}.jsonl"));
    if !latest_file.exists() {
        eprintln!("error: session file not found: {}", latest_file.display());
        return (start_id.to_string(), 1);
    }
    let cache_path = dir.join(".recompact-summary-cache.json");

    const ATTEMPTS: usize = 3;
    for attempt in 1..=ATTEMPTS {
        // Unit keys built here must match the ones assemble computes below, so this shares
        // assemble's view of the records (preamble stripped) rather than the raw active path.
        let (all, _) = select_active(load_jsonl(&latest_file));
        let mut calib = calibrate_lineage(&all);
        if let Some(ov) = o.overhead.as_deref().and_then(|s| s.parse().ok()) {
            calib.overhead = ov;
        }
        let active: Vec<Value> = all
            .into_iter()
            .filter(|r| !truthy(r, "recompactPreamble"))
            .collect();
        // Whether to compact is decided on the live size (last usage, preserved thinking and
        // all); the churn guard below compares estimates, like with like.
        let current = calib.current_tokens(&active);
        let tokens = calib.context_tokens(&active);
        if !o.force && current <= o.threshold {
            eprintln!(
                "continue: ~{current} context tokens ≤ threshold {}; nothing to compact ({})",
                o.threshold,
                calib.describe()
            );
            return (latest, 0);
        }

        // Full-ladder pre-pass: summarize what assemble's plan will want, then hand it the cache.
        let mut sums_path: Option<PathBuf> = None;
        let mut mask_path: Option<PathBuf> = None;
        if let Some(cfg) = &o.summarize {
            let failed = summarize_for_plan(&active, &calib, cfg, o, &cache_path);
            if !failed.is_empty() {
                let hashes: Vec<&String> = failed.iter().collect();
                let mp = std::env::temp_dir().join(format!("recompact-mask-{}.json", uuid_v4()));
                let _ = fs::write(&mp, serde_json::to_string(&hashes).unwrap_or_default());
                mask_path = Some(mp);
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
            a_args.push("--epochs".into());
            if let Some(mp) = &mask_path {
                a_args.push("--mask-units".into());
                a_args.push(mp.to_string_lossy().into_owned());
            }
            a_args.push("--cache".into());
            a_args.push(cache_path.to_string_lossy().into_owned());
        } else {
            a_args.push("--mode".into());
            a_args.push("mask".into());
        }
        a_args.push("--target".into());
        a_args.push(o.target().to_string());
        if o.error_floor {
            a_args.push("--error-floor".into());
        }
        for (flag, v) in [
            ("keep", &o.keep),
            ("split", &o.split),
            ("tail-budget", &o.tail_budget),
            ("overhead", &o.overhead),
        ] {
            if let Some(v) = v {
                a_args.push(format!("--{flag}"));
                a_args.push(v.clone());
            }
        }
        let assembled = run_assemble(&a_args);
        for p in sums_path.iter().chain(mask_path.iter()) {
            let _ = fs::remove_file(p);
        }
        let (new_id, new_file) = match assembled {
            Ok(Some(v)) => v,
            Ok(None) => unreachable!("continue never passes --plan"),
            Err(rc) => {
                if attempt < ATTEMPTS {
                    eprintln!(
                        "continue: source changed during compaction; re-syncing (attempt {}/{ATTEMPTS})",
                        attempt + 1
                    );
                    continue;
                }
                return (latest, rc);
            }
        };
        let v = cmd_verify(&[
            new_file.to_string_lossy().into_owned(),
            "--source".into(),
            latest_file.to_string_lossy().into_owned(),
        ]);
        if v != 0 {
            let _ = fs::remove_file(&new_file);
            lineage_remove(dir, &new_id);
            if attempt < ATTEMPTS {
                // A new human turn landing between assemble and verify fails the fidelity check;
                // that is the same race, so re-sync rather than give up.
                eprintln!(
                    "continue: verification mismatch (source changed?); re-syncing (attempt {}/{ATTEMPTS})",
                    attempt + 1
                );
                continue;
            }
            eprintln!(
                "continue: verification FAILED; removed {} — resuming the previous id is safe",
                new_file.display()
            );
            return (latest, 1);
        }
        // Churn guard: a file that is already mostly incompressible must not spawn descendants
        // every loop iteration. The twin carries no usage yet, so it is measured with the
        // source's calibration.
        let (na, _) = select_active(load_jsonl(&new_file));
        let ntokens = calib.context_tokens(&na);
        if ntokens * 100 >= tokens * 95 {
            let _ = fs::remove_file(&new_file);
            lineage_remove(dir, &new_id);
            eprintln!("continue: no meaningful reduction (~{tokens} → ~{ntokens}); keeping {latest}");
            return (latest, 0);
        }
        eprintln!(
            "continue: ~{tokens} → ~{ntokens} context tokens; resume with: {}",
            resume_command(&new_id, &resume_flags(&active))
        );
        return (new_id, 0);
    }
    (latest, 1)
}

/// The autonomous continuation step, CLI form. Stdout is always a resumable id, so a driver loop
/// can do: ID=$(recompact continue "$ID"); claude -p --resume "$ID" "next step".
pub fn cmd_continue(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    let Some(arg) = pos.first().cloned().or_else(current_session_id) else {
        return usage();
    };
    let (dir, start_id) = match resolve_session_arg(&arg) {
        Ok(v) => v,
        Err(rc) => return rc,
    };
    let o = continue_opts_from(&opts);
    let (id, rc) = continue_session(&dir, &start_id, &o);
    println!("{id}");
    rc
}

// ----------------------------------------------------------------------------- subcommand: shell

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
        let calib = calibrate_opts(&active, &opts);
        let tokens = calib.current_tokens(&active);
        // The mask estimate re-serializes every record; on big projects that is the slow part,
        // so it is opt-in via --estimate.
        let mask_est = if estimate {
            let indices: Vec<usize> = (0..active.len()).collect();
            format!("{:>8}", calib.overhead + calib.tokens(mask_chars_of(&active, &indices)))
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
        if calib.live.is_none() {
            flags.push("est");
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
    "atis-latch",
    "cost-state",
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
/// Resolve a rehydration selector against a compacted session's synthetic records and return the
/// verbatim originals from the source transcript. Selectors, tried in order:
///   - a part key exactly matching `recompactProvenance.part` (what provenance advertises)
///   - a plain 0-based ordinal into the synthetic records (the `rehydrate` listing's [n])
///   - a record uuid found in some summary's coveredUuids — returns just that one record
/// How deep rehydration may follow provenance chains. Generations accumulate one hop per
/// recompaction cycle; ten covers months of daily cycles while still bounding a pointer loop.
const REHYDRATE_MAX_DEPTH: usize = 10;

/// A record is ground truth when it is neither synthetic nor a mechanically reduced copy.
fn is_ground(r: &Value) -> bool {
    !truthy(r, "recompactSynthetic")
        && !truthy(r, "recompactMasked")
        && r.get("recompactImagesElided").is_none()
        && !truthy(r, "recompactThinkingStripped")
}

/// Does this copy still show all of its text? Thinking-stripped records and user turns with
/// elided images do (they are not originals, but a search of "what compaction removed" must not
/// return text the session can already read).
fn text_visible(r: &Value) -> bool {
    !truthy(r, "recompactSynthetic") && !truthy(r, "recompactMasked")
}

/// Every `~/.claude/projects/*` dir, primary first. Sessions move between project dirs (a session
/// created in the main checkout and resumed from a worktree relocates its file), so a recorded
/// absolute path is a hint, never the only place to look.
pub fn project_dirs_near(primary: &Path) -> Vec<PathBuf> {
    let mut out = vec![primary.to_path_buf()];
    if let Some(root) = primary.parent() {
        if let Ok(entries) = fs::read_dir(root) {
            let mut others: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p != primary)
                .collect();
            others.sort();
            out.extend(others);
        }
    }
    out
}

/// Locate `<session>.jsonl`: `hint` if it exists, else the same file name in any project dir
/// under the hint's projects root (or `~/.claude/projects`). Newest wins when several match.
pub fn locate_session(hint: Option<&Path>, session: &str) -> Option<PathBuf> {
    if let Some(h) = hint {
        if h.exists() {
            return Some(h.to_path_buf());
        }
    }
    let name = format!("{}.jsonl", session.strip_suffix(".jsonl").unwrap_or(session));
    let root = hint
        .and_then(|h| h.parent())
        .and_then(|d| d.parent())
        .map(Path::to_path_buf)
        .filter(|r| r.is_dir())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".claude/projects"))
        })?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in fs::read_dir(&root).ok()?.flatten() {
        let p = e.path().join(&name);
        if let Ok(t) = fs::metadata(&p).and_then(|m| m.modified()) {
            if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                best = Some((t, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// The raw transcript a synthetic summary points at, surviving relocation: the recorded path, or
/// the recorded session id wherever it lives now.
pub fn resolve_source(prov: &Value) -> Option<PathBuf> {
    let path = prov.get("source").and_then(|v| v.as_str()).map(PathBuf::from);
    let sid = prov.get("sourceSessionId").and_then(|v| v.as_str()).unwrap_or("");
    match (&path, sid.is_empty()) {
        (Some(p), _) if p.exists() => Some(p.clone()),
        (_, false) => locate_session(path.as_deref(), sid),
        _ => None,
    }
}

fn synth_sources(records: &[Value]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for r in records {
        if let Some(prov) = r.get("recompactProvenance") {
            if let Some(p) = resolve_source(prov) {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        // A mask-only twin has no summaries to point home, but its preamble names its source.
        if let Some(src) = r.get("recompactSource") {
            let prov = json!({"source": src.get("path"), "sourceSessionId": src.get("sessionId")});
            if let Some(p) = resolve_source(&prov) {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Every transcript reachable from `start` through synthetic provenance pointers, BFS order
/// (nearest generation first). Missing files are skipped: rehydration degrades to the nearest
/// surviving copy rather than failing outright.
fn provenance_files(
    start: &[Value],
    cache: &mut HashMap<PathBuf, Vec<Value>>,
) -> Vec<PathBuf> {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut frontier: Vec<PathBuf> = synth_sources(start);
    let mut i = 0;
    while i < frontier.len() {
        let p = frontier[i].clone();
        i += 1;
        if order.contains(&p) || !p.exists() {
            continue;
        }
        if !cache.contains_key(&p) {
            // Skip on a malformed transcript for the same reason a missing one is skipped: an
            // unreadable ancestor should degrade rehydration, not abort it.
            let Ok(loaded) = load_jsonl_soft(&p) else { continue };
            cache.insert(p.clone(), loaded);
        }
        order.push(p.clone());
        for next in synth_sources(&cache[&p]) {
            if !order.contains(&next) && !frontier.contains(&next) {
                frontier.push(next);
            }
        }
    }
    order
}

/// Selector-to-uuid matching: exact, or a prefix of at least 8 chars — the length markers embed,
/// since a full uuid is model-visible only when a marker chooses to carry it.
fn uuid_matches(u: &str, sel: &str) -> bool {
    u == sel || (sel.len() >= 8 && u.starts_with(sel))
}

/// The best available copy of one record: a ground-truth copy from anywhere in the provenance
/// graph, else the least-degraded copy seen (masked beats absent).
fn ground_truth_by_uuid(
    uuid: &str,
    start: &[Value],
    cache: &mut HashMap<PathBuf, Vec<Value>>,
) -> Option<Value> {
    let mut fallback: Option<Value> = None;
    let scan = |records: &[Value], fallback: &mut Option<Value>| -> Option<Value> {
        for r in records {
            if rec_uuid(r).is_some_and(|u| uuid_matches(u, uuid)) {
                if is_ground(r) {
                    return Some(r.clone());
                }
                if fallback.is_none() {
                    *fallback = Some(r.clone());
                }
            }
        }
        None
    };
    if let Some(hit) = scan(start, &mut fallback) {
        return Some(hit);
    }
    for f in provenance_files(start, cache) {
        let records = cache[&f].clone();
        if let Some(mut hit) = scan(&records, &mut fallback) {
            stamp_origin(&mut hit, &f);
            return Some(hit);
        }
    }
    fallback
}

/// Transient note of which transcript a recalled copy came from, so recall can say where a
/// verbatim original was found rather than naming the file the lookup started in. Never written.
pub const RECALLED_FROM: &str = "__recalledFrom";

fn stamp_origin(r: &mut Value, file: &Path) {
    if let Some(o) = r.as_object_mut() {
        o.insert(RECALLED_FROM.into(), Value::String(stem_of(file)));
    }
}

/// Expand one synthetic to the raw records it stands for, following nested synthetics and
/// swapping masked copies for clean ones where the provenance graph still has them.
fn expand_synthetic(
    rec: &Value,
    cache: &mut HashMap<PathBuf, Vec<Value>>,
    depth: usize,
) -> Vec<Value> {
    if depth == 0 {
        return vec![rec.clone()];
    }
    let Some(src) = rec.get("recompactProvenance").and_then(resolve_source) else {
        // Deleted transcript: the summary itself is the best copy left.
        return vec![rec.clone()];
    };
    // Soft: this runs inside the long-lived recall server, where one unreadable ancestor must
    // degrade a lookup, not exit the process.
    cache
        .entry(src.clone())
        .or_insert_with(|| load_jsonl_soft(&src).unwrap_or_default());
    let covered: Vec<String> = rec
        .pointer("/recompactProvenance/coveredUuids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|u| u.as_str()).map(str::to_string).collect())
        .unwrap_or_default();
    let source_records = cache[&src].clone();
    let mut out = Vec::new();
    for r in &source_records {
        let Some(u) = rec_uuid(r) else { continue };
        if !covered.iter().any(|c| c == u) {
            continue;
        }
        if truthy(r, "recompactSynthetic") {
            out.extend(expand_synthetic(r, cache, depth - 1));
        } else if !is_ground(r) {
            let u = u.to_string();
            out.push(ground_truth_by_uuid(&u, &source_records, cache).unwrap_or_else(|| r.clone()));
        } else {
            let mut hit = r.clone();
            stamp_origin(&mut hit, &src);
            out.push(hit);
        }
    }
    out
}

pub fn rehydrate_select(compacted: &[Value], selector: &str) -> Result<Vec<Value>, String> {
    let synths: Vec<&Value> = compacted
        .iter()
        .filter(|r| truthy(r, "recompactSynthetic"))
        .collect();
    let part_of = |r: &Value| {
        r.pointer("/recompactProvenance/part")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let mut cache: HashMap<PathBuf, Vec<Value>> = HashMap::new();

    // Part-key / ordinal selectors address this file's own summaries. Summaries carried from
    // older generations keep their original keys, so one key can name several summaries.
    let key_hits: Vec<&&Value> = synths
        .iter()
        .filter(|r| part_of(r).as_deref() == Some(selector))
        .collect();
    if key_hits.len() > 1 {
        let ids: Vec<String> = key_hits
            .iter()
            .filter_map(|r| rec_uuid(r))
            .map(|u| u.get(..8).unwrap_or(u).to_string())
            .collect();
        return Err(format!(
            "summary key {selector:?} is carried by {} summaries from different compaction generations; \
             use the id from the footer you are reading: one of {ids:?}",
            key_hits.len()
        ));
    }
    let chosen: Option<&Value> = if let Some(hit) = key_hits.first().map(|r| **r) {
        Some(hit)
    } else if let Ok(n) = selector.parse::<usize>().ok().filter(|_| selector.len() < 8).ok_or(()) {
        // A numeric selector can also be an unsplit segment's part key (part "3"), handled above;
        // here it is the listing ordinal. Eight-plus digits is never an ordinal — it falls
        // through to uuid-prefix matching (markers embed 8-char prefixes, which can be all-digit).
        match synths.get(n) {
            Some(r) => Some(r),
            None => {
                return Err(format!(
                    "no synthetic summary [{n}] (have {}); selectors are a part key, a listing ordinal, or a covered uuid",
                    synths.len()
                ))
            }
        }
    } else {
        None
    };
    if let Some(rec) = chosen {
        if rec.get("recompactProvenance").is_none() {
            return Err("summary has no provenance (assembled by an older version)".into());
        }
        let recovered = expand_synthetic(rec, &mut cache, REHYDRATE_MAX_DEPTH);
        if recovered.len() == 1 && !is_ground(&recovered[0]) {
            return Err(format!(
                "original transcript not found for part {selector:?} (moved or deleted?)"
            ));
        }
        return Ok(recovered);
    }

    // Uuid selector (full or >=8-char prefix, as embedded in elision markers): the record may
    // live any number of generations back — search the whole provenance graph, not just this
    // file's own summaries.
    let uuidish = selector.len() >= 8
        && selector.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if uuidish {
        if let Some(hit) = ground_truth_by_uuid(selector, compacted, &mut cache) {
            if truthy(&hit, "recompactSynthetic") {
                return Ok(expand_synthetic(&hit, &mut cache, REHYDRATE_MAX_DEPTH));
            }
            return Ok(vec![hit]);
        }
        let searched = provenance_files(compacted, &mut cache).len();
        return Err(format!(
            "uuid {selector:?} not found in this file or any of the {searched} transcript(s) reachable through provenance"
        ));
    }

    let known: Vec<String> = synths.iter().filter_map(|r| part_of(r)).collect();
    Err(format!(
        "selector {selector:?} matches no part key, ordinal, or covered uuid; known part keys: {known:?}"
    ))
}

pub fn cmd_rehydrate(args: &[String]) -> i32 {
    let (pos, _opts) = parse_opts(args);
    if pos.is_empty() {
        return usage();
    }
    let compacted = load_jsonl(Path::new(&pos[0]));

    if pos.len() < 2 {
        let synths: Vec<&Value> = compacted
            .iter()
            .filter(|r| truthy(r, "recompactSynthetic"))
            .collect();
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
            let part = r
                .pointer("/recompactProvenance/part")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            eprintln!(
                "  [{n}] part {part} covers {covered} records: {}",
                truncate(&text.replace('\n', " "), 100)
            );
        }
        if synths.iter().any(|r| r.get("recompactProvenance").is_none()) {
            eprintln!("note: some summaries lack provenance (assembled by an older version)");
        }
        return 0;
    }

    match rehydrate_select(&compacted, &pos[1]) {
        Ok(records) => {
            for r in &records {
                let mut r = r.clone();
                if let Some(o) = r.as_object_mut() {
                    o.remove(RECALLED_FROM);
                }
                println!("{}", serde_json::to_string(&r).unwrap());
            }
            eprintln!("rehydrate: {} verbatim record(s) recovered", records.len());
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
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
    let mut new = load_jsonl(Path::new(&pos[0]));

    // Assemble writes exactly one last-prompt record, as the file's final line. A twin that has
    // been resumed grows past it (the CLI appends live records, including further last-prompt
    // records per user turn). Structural guarantees only ever applied to the assembled product,
    // so verify the prefix up to the first last-prompt and report growth informationally —
    // otherwise every resumed twin fails on its own post-assembly life.
    let grown = match new.iter().position(|r| rec_type(r) == "last-prompt") {
        Some(b) => new.split_off(b + 1),
        None => Vec::new(),
    };
    if !grown.is_empty() {
        eprintln!(
            "note: {} record(s) appended after the assembly boundary (session has been resumed); \
             verifying the assembled prefix of {} record(s)",
            grown.len(),
            new.len()
        );
    }
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

    // Optional fidelity check against the source: genuine user turns, in order, must be
    // identical. The source itself may have grown after assembly (session teardown appends
    // interrupt markers and notification records when the original session exits), so turns the
    // assembly never saw are fine ONLY as a trailing suffix: the assembly knew a record either by
    // keeping it (uuid in the prefix) or by summarizing it (uuid in some coveredUuids) — an
    // unknown turn followed by a known one means a hole in the middle, which is real corruption.
    if let Some(srcp) = opts.get("source").and_then(|v| v.as_str()) {
        let (src, _) = select_active(load_jsonl(Path::new(srcp)));
        let mut known: HashSet<String> = new
            .iter()
            .filter_map(rec_uuid)
            .map(str::to_string)
            .collect();
        for r in &new {
            if let Some(covered) = r
                .pointer("/recompactProvenance/coveredUuids")
                .and_then(|v| v.as_array())
            {
                known.extend(covered.iter().filter_map(|u| u.as_str()).map(str::to_string));
            }
        }
        let mut known_texts: Vec<String> = Vec::new();
        let mut growth = 0usize;
        let mut hole = false;
        for r in src.iter().filter(|r| is_genuine_user(r)) {
            if rec_uuid(r).is_some_and(|u| known.contains(u)) {
                if growth > 0 {
                    hole = true;
                }
                known_texts.push(fidelity_text(r));
            } else {
                growth += 1;
            }
        }
        let assembled: Vec<String> =
            new.iter().filter(|r| is_genuine_user(r)).map(fidelity_text).collect();
        let ok = !hole && known_texts == assembled;
        let mut detail = format!(
            "source has {} known to assembly, assembled has {}",
            known_texts.len(),
            assembled.len()
        );
        if hole {
            detail.push_str("; a turn the assembly never saw sits BEFORE turns it kept — records were dropped");
        }
        checks.push(("user turns preserved verbatim", ok, detail));
        if ok && growth > 0 {
            eprintln!("note: source gained {growth} user turn(s) after assembly (session teardown or continued use); assembled turns match everything the assembly saw");
        }
        // Messages typed mid-turn arrive as attachments, not user records, so the check above
        // cannot see them; each one the assembly knew about must still be in the output.
        let present: HashSet<&str> = new.iter().filter(|r| is_human_queued(r)).filter_map(rec_uuid).collect();
        let lost: Vec<String> = src
            .iter()
            .filter(|r| is_human_queued(r))
            .filter_map(rec_uuid)
            .filter(|u| known.contains(*u) && !present.contains(u))
            .map(|u| u.get(..8).unwrap_or(u).to_string())
            .collect();
        checks.push((
            "mid-turn user messages preserved",
            lost.is_empty(),
            format!("missing {lost:?}"),
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

// ---------------------------------------------------------------------------------------------
// Recall: rehydration as a tool the model can call, not just an operator CLI.
//
// The first version resolved uuid prefixes project-wide but answered "which session am I?" by
// guessing the newest file, and summary footers named part keys that only mean something inside
// one file. In the month after it shipped, resumed sessions called it five times; all five
// failed (a worktree session's server looked in the wrong project dir, a key resolved against
// somebody else's newest session, a guessed session id was the twin's parent). Now the server
// knows its session (Claude Code sets CLAUDE_CODE_SESSION_ID for it), every footer carries a
// project-wide uuid selector, lookups fall back across project dirs, and `query` searches what
// compaction removed so the model does not need an address at all.
// ---------------------------------------------------------------------------------------------

/// Per-response payload budget. ARC (arXiv:2607.25066) caps a recall at 8k chars and chunks the
/// rest; we copy the cap for a sharper reason. ARC can evict a recalled body back to a citation
/// stub when its budget is exceeded — we cannot: once a tool result is in the transcript it is
/// there for good. Never over-delivering is the only control left, so the cap is load-bearing.
pub const RECALL_CHUNK_CHARS: usize = 8000;

/// Read a transcript without the CLI's exit-on-error behavior. `load_jsonl` aborts the process on
/// an unreadable or malformed file, which is right for a one-shot command and fatal for a
/// long-lived MCP server: one corrupt transcript anywhere in a project dir would take recall down
/// for the whole session.
pub fn load_jsonl_soft(path: &Path) -> Result<Vec<Value>, String> {
    let mut buf = String::new();
    fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut buf))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse_jsonl(&buf).map_err(|e| format!("{}: {e}", path.display()))
}

/// The project transcript dir for an explicit working directory. `project_dir_from_cwd` reads the
/// *process* cwd, which is wrong for an MCP server: it is spawned by the client, not by the user,
/// and its cwd need not be the session's.
pub fn project_dir_for(cwd: &Path) -> Option<PathBuf> {
    let munged = munge_project_path(&cwd.to_string_lossy());
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".claude/projects").join(munged))
}

/// Where recall looks, and for whom: the primary project dir (the others under the same root are
/// searched when it has nothing) and the session being served, when known.
pub struct RecallCtx {
    pub dir: PathBuf,
    pub session: Option<String>,
}

impl From<&Path> for RecallCtx {
    fn from(dir: &Path) -> Self {
        RecallCtx {
            dir: dir.to_path_buf(),
            session: None,
        }
    }
}

impl From<&PathBuf> for RecallCtx {
    fn from(dir: &PathBuf) -> Self {
        RecallCtx::from(dir.as_path())
    }
}

impl From<PathBuf> for RecallCtx {
    fn from(dir: PathBuf) -> Self {
        RecallCtx { dir, session: None }
    }
}

impl RecallCtx {
    /// A session named by id or path, wherever it lives now.
    pub fn session_file(&self, session: &str) -> Option<PathBuf> {
        let p = PathBuf::from(session);
        if session.ends_with(".jsonl") && p.exists() {
            return Some(p);
        }
        locate_session(Some(&session_path(&self.dir, session)), session)
    }
}

/// Every session in `dir` holding a record whose uuid matches `sel`, best entry point first:
/// ground-truth copies before degraded ones, then newest by mtime. A ground hit IS the original,
/// so the common case resolves with no provenance walk at all.
///
/// The substring pre-filter matters: the scan reads whole transcripts (this machine's largest
/// project dir is 288 files / 727 MB) but only parses lines that could match, so a full sweep
/// costs tens of milliseconds rather than parsing ~700 MB of JSON.
pub fn find_uuid_sessions(dir: &Path, sel: &str) -> (Vec<PathBuf>, Vec<String>) {
    let mut hits: Vec<(bool, std::time::SystemTime, PathBuf)> = Vec::new();
    let mut distinct: Vec<String> = Vec::new();
    // The pre-filter is deliberately loose: it matches the bare selector anywhere in the file,
    // including parentUuid, coveredUuids and prose. Anchoring it on `"uuid":"` would skip a few
    // line parses but assumes compact serialization, and a transcript written with spaces after
    // the colons would then resolve to nothing — silently, which is the worst failure this tool
    // can have. A full sweep of the largest project dir here (288 files, 727 MB) costs ~64 ms,
    // so the parses this would save are not worth a format assumption.
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let mut buf = String::new();
        if fs::File::open(&path)
            .and_then(|mut f| f.read_to_string(&mut buf))
            .is_err()
        {
            continue;
        }
        if !buf.contains(sel) {
            continue;
        }
        let mut ground = false;
        let mut found = false;
        for line in buf.lines() {
            if !line.contains(sel) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(u) = rec_uuid(&v).filter(|u| uuid_matches(u, sel)) {
                found = true;
                if !distinct.iter().any(|d| d == u) {
                    distinct.push(u.to_string());
                }
                if is_ground(&v) {
                    ground = true;
                }
            }
        }
        if found {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            hits.push((ground, mtime, path));
        }
    }
    // ground first, then newest.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    (hits.into_iter().map(|(_, _, p)| p).collect(), distinct)
}

/// `find_uuid_sessions` over the primary project dir, then — only when it holds nothing — every
/// other project dir under the same root.
fn find_uuid_anywhere(ctx: &RecallCtx, sel: &str) -> (Vec<PathBuf>, Vec<String>) {
    let (found, distinct) = find_uuid_sessions(&ctx.dir, sel);
    if !found.is_empty() {
        return (found, distinct);
    }
    let mut all_found = Vec::new();
    let mut all_distinct: Vec<String> = Vec::new();
    for d in project_dirs_near(&ctx.dir).into_iter().skip(1) {
        let (f, ds) = find_uuid_sessions(&d, sel);
        all_found.extend(f);
        for u in ds {
            if !all_distinct.contains(&u) {
                all_distinct.push(u);
            }
        }
    }
    (all_found, all_distinct)
}

/// What a recall resolved to, and where from — the caller reports the source so a model can tell
/// a verbatim original from a degraded copy.
pub struct Recalled {
    pub records: Vec<Value>,
    pub session: String,
}

const NO_SESSION_FOR_KEY: &str = "summary keys and ordinals index one session's summaries, and this \
recall server does not know which session you are in. Use the 8-character id from the summary \
footer (\"recall <id>\") instead, which resolves anywhere, or pass session=<session id>.";

/// Resolve a selector to verbatim records, for a caller that knows only the project dir.
pub fn recall_select(
    dir: &Path,
    selector: &str,
    session: Option<&str>,
) -> Result<Recalled, String> {
    recall_select_ctx(&RecallCtx::from(dir), selector, session)
}

/// Resolve a selector to verbatim records.
///
/// uuid / uuid-prefix selectors (what markers and footers print) need no session. Part keys and
/// ordinals index one file's summary list: they resolve against the session named in the call,
/// else the session the server serves — never a guess.
pub fn recall_select_ctx(
    ctx: &RecallCtx,
    selector: &str,
    session: Option<&str>,
) -> Result<Recalled, String> {
    let uuidish =
        selector.len() >= 8 && selector.chars().all(|c| c.is_ascii_hexdigit() || c == '-');

    // A named session is a hint, not a constraint: for a uuid it is tried first, then the rest of
    // the project. Naming the wrong session should not make a resolvable uuid unresolvable.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(sess) = session {
        match ctx.session_file(sess) {
            Some(p) => candidates.push(p),
            None => {
                return Err(format!(
                    "no session {sess:?} under {} or any sibling project dir. Uuid prefixes need no \
                     session at all — pass the prefix alone.",
                    ctx.dir.display()
                ))
            }
        }
    } else if let Some(p) = ctx.session.as_deref().and_then(|s| ctx.session_file(s)) {
        candidates.push(p);
    }
    if uuidish {
        let (found, distinct) = find_uuid_anywhere(ctx, selector);
        if distinct.len() > 1 {
            return Err(format!(
                "{selector:?} is ambiguous: it prefixes {} different records ({}). \
                 Use more characters of the uuid.",
                distinct.len(),
                distinct
                    .iter()
                    .map(|u| truncate(u, 12))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if found.is_empty() && candidates.is_empty() {
            return Err(format!(
                "uuid {selector:?} is not in any transcript under {}. \
                 Check the prefix against the marker that printed it.",
                ctx.dir.parent().unwrap_or(&ctx.dir).display()
            ));
        }
        for f in found {
            if !candidates.contains(&f) {
                candidates.push(f);
            }
        }
    } else if candidates.is_empty() {
        return Err(NO_SESSION_FOR_KEY.to_string());
    }

    let mut last_err = String::new();
    for path in &candidates {
        let records = match load_jsonl_soft(path) {
            Ok(r) => r,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        match rehydrate_select(&records, selector) {
            Ok(mut records) => {
                // Name the transcript the originals came from, not the one the lookup began in.
                let origin = records
                    .iter()
                    .find_map(|r| r.get(RECALLED_FROM).and_then(|v| v.as_str()).map(str::to_string))
                    .unwrap_or_else(|| stem_of(path));
                for r in records.iter_mut() {
                    if let Some(o) = r.as_object_mut() {
                        o.remove(RECALLED_FROM);
                    }
                }
                return Ok(Recalled {
                    records,
                    session: origin,
                });
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn session_path(dir: &Path, session: &str) -> PathBuf {
    let s = session.strip_suffix(".jsonl").unwrap_or(session);
    dir.join(format!("{s}.jsonl"))
}

fn stem_of(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ------------------------------------------------------------------------------ recall: query

/// Every transcript the session descends from: synthetic provenance, the preamble's recorded
/// source (mask-only twins have no summaries to point home), the lineage sidecar, and the
/// session a `/branch` or `--fork-session` copy was forked from. Nearest first.
pub fn lineage_files(start: &Path) -> Vec<PathBuf> {
    let mut order: Vec<PathBuf> = vec![start.to_path_buf()];
    let mut i = 0;
    while i < order.len() && order.len() < 64 {
        let cur = order[i].clone();
        i += 1;
        let Ok(records) = load_jsonl_soft(&cur) else { continue };
        let mut next = synth_sources(&records);
        if let Some(dir) = cur.parent() {
            let id = stem_of(&cur);
            if let Some(parent) = lineage_load(dir)
                .get(&id)
                .and_then(|v| v.get("parent"))
                .and_then(|v| v.as_str())
            {
                if let Some(p) = locate_session(Some(&dir.join(format!("{parent}.jsonl"))), parent) {
                    next.push(p);
                }
            }
            if let Some(parent) = records
                .iter()
                .find_map(|r| r.pointer("/forkedFrom/sessionId").and_then(|v| v.as_str()))
            {
                if parent != id {
                    if let Some(p) = locate_session(Some(&dir.join(format!("{parent}.jsonl"))), parent) {
                        next.push(p);
                    }
                }
            }
        }
        for p in next {
            if !order.contains(&p) {
                order.push(p);
            }
        }
    }
    order
}

/// Every record uuid in a session's history: the records it holds in any form, plus everything
/// its summaries replaced, following provenance through every older generation.
fn history_uuids(start: &[Value]) -> HashSet<String> {
    let mut known: HashSet<String> = start.iter().filter_map(rec_uuid).map(str::to_string).collect();
    let mut frontier: Vec<Value> = start
        .iter()
        .filter(|r| truthy(r, "recompactSynthetic"))
        .cloned()
        .collect();
    let mut expanded: HashSet<String> = HashSet::new();
    let mut files: HashMap<PathBuf, Vec<Value>> = HashMap::new();
    while let Some(syn) = frontier.pop() {
        let Some(id) = rec_uuid(&syn).map(str::to_string) else { continue };
        if !expanded.insert(id) || expanded.len() > 50_000 {
            continue;
        }
        let Some(prov) = syn.get("recompactProvenance") else { continue };
        let covered: HashSet<String> = prov
            .get("coveredUuids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|u| u.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if let Some(src) = resolve_source(prov) {
            let recs = files
                .entry(src.clone())
                .or_insert_with(|| load_jsonl_soft(&src).unwrap_or_default());
            frontier.extend(
                recs.iter()
                    .filter(|r| truthy(r, "recompactSynthetic") && rec_uuid(r).is_some_and(|u| covered.contains(u)))
                    .cloned(),
            );
        }
        known.extend(covered);
    }
    known
}

/// Query terms: whitespace-separated words, or "quoted phrases", lowercased.
fn query_terms(q: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut rest = q.trim();
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix('"') {
            let end = stripped.find('"').unwrap_or(stripped.len());
            let t = stripped[..end].trim().to_lowercase();
            if !t.is_empty() {
                terms.push(t);
            }
            rest = stripped.get(end + 1..).unwrap_or("").trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let t = rest[..end].trim_matches(|c: char| c == ',' || c == ';').to_lowercase();
            if t.len() >= 2 {
                terms.push(t);
            }
            rest = rest[end..].trim_start();
        }
    }
    terms.dedup();
    terms
}

fn snippet_around(text: &str, lower: &str, term: &str, width: usize) -> String {
    let at = lower.find(term).unwrap_or(0);
    // Char boundaries on both sides; lower/text share byte offsets only for ASCII, so re-find a
    // safe boundary in `text`.
    let mut start = at.saturating_sub(width / 2);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (at + term.len() + width / 2).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    let body: String = text
        .get(start..end)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body,
        if end < text.len() { "…" } else { "" }
    )
}

fn record_kind(r: &Value) -> &'static str {
    if is_human_queued(r) {
        return "user (mid-turn)";
    }
    match rec_type(r) {
        "assistant" => {
            let tools = content(r)
                .and_then(|c| c.as_array())
                .is_some_and(|a| a.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use")));
            if tools {
                "tool call"
            } else {
                "assistant"
            }
        }
        "user" if is_genuine_user(r) => "user",
        "user" => {
            if content(r)
                .and_then(|c| c.as_array())
                .is_some_and(|a| a.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result")))
            {
                "tool result"
            } else {
                "delivered"
            }
        }
        _ => "record",
    }
}

/// Search the originals of everything compaction took out of this session's view: records its
/// summaries replaced, originals of masked records, and older generations' material. Records the
/// session still shows verbatim are skipped — the model can already see them.
pub fn recall_query(ctx: &RecallCtx, query: &str, session: Option<&str>, limit: usize) -> Result<String, String> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Err("query is empty: pass words or \"a quoted phrase\" to search for.".into());
    }
    let start = match session.or(ctx.session.as_deref()) {
        Some(s) => ctx
            .session_file(s)
            .ok_or_else(|| format!("no session {s:?} under {} or any sibling project dir", ctx.dir.display()))?,
        None => {
            return Err("this recall server does not know which session you are in, so it cannot tell \
                        what compaction removed from it; pass session=<session id>."
                .into())
        }
    };
    let files = lineage_files(&start);
    let start_records = load_jsonl_soft(&start)?;
    // The text the session already shows is not a search result.
    let visible: HashSet<String> = start_records
        .iter()
        .filter(|r| text_visible(r))
        .filter_map(rec_uuid)
        .map(str::to_string)
        .collect();
    let known = history_uuids(&start_records);
    let mut seen: HashSet<String> = HashSet::new();
    // (terms matched, occurrences, file order, record) — ranked below.
    let mut hits: Vec<(usize, usize, usize, String, Value)> = Vec::new();
    let mut searched = 0usize;
    for (fi, f) in files.iter().enumerate() {
        let Ok(records) = load_jsonl_soft(f) else { continue };
        let sid = stem_of(f);
        for r in records {
            let Some(u) = rec_uuid(&r).map(str::to_string) else { continue };
            // Only this session's own history: an ancestor file also holds turns written after
            // the twin was cut, and a branch's parent holds turns after the branch point.
            if !known.contains(&u) {
                continue;
            }
            if !is_ground(&r) || truthy(&r, "recompactPreamble") || truthy(&r, "recompactLedger") {
                continue;
            }
            if visible.contains(&u) || !seen.insert(u.clone()) {
                continue;
            }
            if rec_type(&r) == "attachment" && !is_human_queued(&r) {
                continue;
            }
            searched += 1;
            let text = if is_human_queued(&r) {
                human_queued_text(&r)
            } else {
                payload_of(std::slice::from_ref(&r)).text
            };
            if text.is_empty() {
                continue;
            }
            let lower = text.to_lowercase();
            let matched = terms.iter().filter(|t| lower.contains(t.as_str())).count();
            if matched == 0 {
                continue;
            }
            let occ: usize = terms.iter().map(|t| lower.matches(t.as_str()).count().min(5)).sum();
            hits.push((matched, occ, fi, sid.clone(), r));
        }
    }
    if hits.is_empty() {
        return Ok(format!(
            "no match for {query:?} among {searched} record(s) compaction removed from view (searched {} transcript(s) of this session's lineage). \
             Try fewer or different words; exact identifiers work best.",
            files.len()
        ));
    }
    // All terms first, then more occurrences, then nearer generations, then newer records.
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.cmp(&b.2))
            .then_with(|| {
                let ta = a.4.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
                let tb = b.4.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
                tb.cmp(ta)
            })
    });
    let total = hits.len();
    let rarest = terms
        .iter()
        .min_by_key(|t| hits.iter().filter(|h| {
            let tx = payload_of(std::slice::from_ref(&h.4)).text.to_lowercase();
            tx.contains(t.as_str())
        }).count())
        .cloned()
        .unwrap_or_default();
    let mut out = format!(
        "{total} match(es) for {query:?} in what compaction removed ({} transcript(s) searched){}:\n",
        files.len(),
        if total > limit { format!("; best {limit} shown") } else { String::new() }
    );
    for (matched, _, _, sid, r) in hits.into_iter().take(limit) {
        let u = rec_uuid(&r).unwrap_or("????????");
        let text = if is_human_queued(&r) {
            human_queued_text(&r)
        } else {
            payload_of(std::slice::from_ref(&r)).text
        };
        let lower = text.to_lowercase();
        let term = if lower.contains(rarest.as_str()) {
            rarest.clone()
        } else {
            terms.iter().find(|t| lower.contains(t.as_str())).cloned().unwrap_or_default()
        };
        let ts = r
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(|t| t.get(..16).unwrap_or(t).replace('T', " "))
            .unwrap_or_default();
        out.push_str(&format!(
            "- {} · {} · {} · session {}{}: {}\n",
            u.get(..8).unwrap_or(u),
            record_kind(&r),
            ts,
            short_id(&sid),
            if matched < terms.len() { format!(" ({matched}/{} terms)", terms.len()) } else { String::new() },
            snippet_around(&text, &lower, &term, 300)
        ));
        if out.chars().count() > RECALL_CHUNK_CHARS - 400 {
            out.push_str("… (budget reached; narrow the query)\n");
            break;
        }
    }
    out.push_str("recall(selector=\"<id>\") returns any of these in full.");
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// MCP server: `recall` as a tool the model calls mid-inference.
//
// Newline-delimited JSON-RPC 2.0 over stdio, hand-rolled on serde_json. The crate is already a
// JSONL processor with two dependencies and builds from source on every plugin install, so an SDK
// would cost more than the ~200 lines it saves.
// ---------------------------------------------------------------------------------------------

/// What the client is told the tool is for. This reaches the model through `tools/list` without a
/// skill load, so it has to teach the whole convention in a paragraph.
const RECALL_DESC: &str = "Read back anything compaction removed from this session, verbatim. \
Two ways in: (1) query — words or an exact identifier; searches the originals behind every \
summary and elision marker in this session's history and returns matching snippets with ids. \
(2) selector — the id a marker or summary footer printed (\"[recompact: elided …; rehydrate \
<id>]\", \"[recompact summary <key> · recall <id>]\"); returns that item in full. Use this before \
re-running a search, re-reading a file, or guessing at a detail a summary dropped: reading back \
what the session already produced is cheaper and exact. Call with no arguments to list this \
session's summaries.";

const INSTRUCTIONS: &str = "This session's transcript may have been compacted by \
segment_recompact. Summaries ending \"[recompact summary <key> · recall <id>]\" and markers \
like \"[recompact: elided …; rehydrate <id>]\" stand for content that still exists verbatim. \
Use the recall tool — query=\"words\" to search it, selector=\"<id>\" to read one item — rather \
than re-deriving it.";

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// One recalled payload, split the way a tool result has to be delivered: prose that can be
/// chunked, and images that cannot.
struct Payload {
    text: String,
    images: Vec<(String, String)>, // (base64 data, mime)
}

/// Pull the model-meaningful content out of raw transcript records. The envelope (uuids, parent
/// links, provenance) is never shown at inference and would spend the budget on bookkeeping, so
/// only message content survives.
fn payload_of(records: &[Value]) -> Payload {
    let mut text = String::new();
    let mut images = Vec::new();
    for r in records {
        if is_human_queued(r) {
            text.push_str(&human_queued_text(r));
            text.push('\n');
            continue;
        }
        let Some(content) = r.pointer("/message/content") else {
            continue;
        };
        match content {
            Value::String(s) => {
                text.push_str(s);
                text.push('\n');
            }
            Value::Array(blocks) => {
                for b in blocks {
                    collect_block(b, &mut text, &mut images);
                }
            }
            _ => {}
        }
    }
    Payload {
        text: text.trim_end().to_string(),
        images,
    }
}

fn collect_block(b: &Value, text: &mut String, images: &mut Vec<(String, String)>) {
    match b.get("type").and_then(|v| v.as_str()) {
        Some("text") => {
            if let Some(s) = b.get("text").and_then(|v| v.as_str()) {
                text.push_str(s);
                text.push('\n');
            }
        }
        Some("thinking") => {
            if let Some(s) = b.get("thinking").and_then(|v| v.as_str()) {
                if !s.trim().is_empty() {
                    text.push_str(s);
                    text.push('\n');
                }
            }
        }
        Some("image") => {
            let data = b.pointer("/source/data").and_then(|v| v.as_str());
            let mime = b
                .pointer("/source/media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png");
            if let Some(d) = data {
                images.push((d.to_string(), mime.to_string()));
            }
        }
        Some("tool_use") => {
            if let Some(name) = b.get("name").and_then(|v| v.as_str()) {
                text.push_str(&format!("[tool_use {name}]\n"));
            }
            if let Some(input) = b.get("input") {
                text.push_str(&serde_json::to_string(input).unwrap_or_default());
                text.push('\n');
            }
        }
        Some("tool_result") => match b.get("content") {
            Some(Value::String(s)) => {
                text.push_str(s);
                text.push('\n');
            }
            Some(Value::Array(inner)) => {
                for ib in inner {
                    collect_block(ib, text, images);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

/// Chunk boundaries are char-based, not byte-based, so a multi-byte character is never split.
/// An out-of-range chunk is an error rather than a clamp: silently serving the last chunk under a
/// number the caller did not ask for makes a model believe it has read past the end.
fn chunk_of(text: &str, chunk: usize) -> Result<(String, usize), String> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len().div_ceil(RECALL_CHUNK_CHARS).max(1);
    if chunk > total {
        return Err(format!(
            "chunk {chunk} is past the end: this payload has {total} chunk(s)."
        ));
    }
    let start = (chunk - 1) * RECALL_CHUNK_CHARS;
    let end = (start + RECALL_CHUNK_CHARS).min(chars.len());
    Ok((chars[start..end].iter().collect(), total))
}

/// One line per covered record, so a multi-record expansion stays inside a single response and the
/// model can then ask for the one record it wants. Concatenating them instead makes finding a
/// detail a walk through every chunk — the cost recall exists to avoid.
fn digest_of(records: &[Value]) -> String {
    let mut out = format!(
        "{} records. Recall any single one by its id:\n",
        records.len()
    );
    for r in records {
        let uuid = rec_uuid(r).unwrap_or("????????");
        let p = payload_of(std::slice::from_ref(r));
        let preview = p.text.replace('\n', " ");
        let imgs = if p.images.is_empty() {
            String::new()
        } else {
            format!(" [{} image(s)]", p.images.len())
        };
        out.push_str(&format!(
            "  {} {}{imgs} — {}\n",
            uuid.get(..8).unwrap_or(uuid),
            record_kind(r),
            truncate(&preview, 120)
        ));
    }
    out
}

fn list_summaries(ctx: &RecallCtx, session: Option<&str>) -> Value {
    let Some(path) = session
        .or(ctx.session.as_deref())
        .and_then(|s| ctx.session_file(s))
    else {
        return tool_text(
            "this recall server does not know which session you are in; pass session=<session id> \
             to list its summaries. Ids printed in markers and footers need no session."
                .to_string(),
            true,
        );
    };
    let records = match load_jsonl_soft(&path) {
        Ok(r) => r,
        Err(e) => return tool_text(e, true),
    };
    let synths: Vec<&Value> = records
        .iter()
        .filter(|r| truthy(r, "recompactSynthetic"))
        .collect();
    if synths.is_empty() {
        return tool_text(
            format!(
                "{} holds no compaction summaries. Ids from elision markers still resolve, and \
                 query= searches everything compaction removed.",
                stem_of(&path)
            ),
            false,
        );
    }
    let mut out = format!("{} summaries in {}:\n", synths.len(), stem_of(&path));
    for r in &synths {
        let part = r
            .pointer("/recompactProvenance/part")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let covered = r
            .pointer("/recompactProvenance/coveredUuids")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let text = r
            .pointer("/message/content/0/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let id = rec_uuid(r).map(|u| u.get(..8).unwrap_or(u)).unwrap_or("?");
        out.push_str(&format!(
            "  {id} (part {part}, {covered} records): {}\n",
            truncate(&text.replace('\n', " "), 100)
        ));
    }
    tool_text(out, false)
}

fn call_recall(ctx: &RecallCtx, args: &Value) -> Value {
    let selector = args.get("selector").and_then(|v| v.as_str());
    let query = args.get("query").and_then(|v| v.as_str());
    let session = args
        .get("session")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let chunk = args
        .get("chunk")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .max(1) as usize;

    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        return match recall_query(ctx, q, session, 12) {
            Ok(t) => tool_text(t, false),
            Err(e) => tool_text(e, true),
        };
    }
    let Some(selector) = selector.filter(|s| !s.trim().is_empty()) else {
        return list_summaries(ctx, session);
    };

    let recalled = match recall_select_ctx(ctx, selector.trim(), session) {
        Ok(r) => r,
        Err(e) => return tool_text(e, true),
    };
    let payload = payload_of(&recalled.records);
    if payload.text.is_empty() && payload.images.is_empty() {
        return tool_text(
            format!(
                "{selector:?} resolved to {} record(s) in {} but they carry no readable content.",
                recalled.records.len(),
                recalled.session
            ),
            false,
        );
    }

    // A multi-record expansion that will not fit in one response becomes an index instead of a
    // wall of concatenated text; a single record is always served as itself.
    let multi = recalled.records.len() > 1;
    let oversized = payload.text.chars().count() > RECALL_CHUNK_CHARS;
    if multi && oversized {
        return tool_text(
            format!(
                "recall {selector:?} — {} records from {}\n\n{}",
                recalled.records.len(),
                recalled.session,
                digest_of(&recalled.records)
            ),
            false,
        );
    }

    let (body, total) = match chunk_of(&payload.text, chunk) {
        Ok(v) => v,
        Err(e) => return tool_text(e, true),
    };
    let mut header = format!(
        "recall {selector:?} — {} record(s) from {}",
        recalled.records.len(),
        recalled.session
    );
    if total > 1 {
        let next = if chunk < total {
            format!(" (call again with chunk={} for the next)", chunk + 1)
        } else {
            " (final chunk)".to_string()
        };
        header.push_str(&format!("; chunk {chunk}/{total}{next}"));
    }

    let mut content = vec![json!({"type": "text", "text": format!("{header}\n\n{body}")})];
    // Images ride the first chunk only, so walking a long payload does not re-deliver them.
    if chunk == 1 {
        for (data, mime) in &payload.images {
            content.push(json!({"type": "image", "data": data, "mimeType": mime}));
        }
    }
    json!({"content": content, "isError": false})
}

fn tools_list() -> Value {
    json!({"tools": [{
        "name": "recall",
        "title": "Recall compacted content",
        "description": RECALL_DESC,
        "annotations": {"readOnlyHint": true, "openWorldHint": false},
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Words, an exact identifier, or \"a quoted phrase\" to search for in everything compaction removed from this session (originals behind summaries and markers, across all compaction generations). Returns ranked snippets with ids."
                },
                "selector": {
                    "type": "string",
                    "description": "An id a marker or footer printed (8+ hex characters, e.g. from \"recall 1a2b3c4d\" or \"rehydrate 1a2b3c4d\"); returns that item in full. Summary keys like \"12.3\" also work within one session."
                },
                "chunk": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based chunk for a payload too large to return at once. Defaults to 1; the response says how many there are."
                },
                "session": {
                    "type": "string",
                    "description": "Session id to resolve against. Rarely needed: the server already knows the session it serves, and ids resolve everywhere."
                }
            }
        }
    }]})
}

/// Serve MCP over any line-oriented pair of streams. Generic rather than bound to stdio so the
/// tests can drive it in-process, as every other test in this crate does.
pub fn mcp_serve<R: std::io::BufRead, W: Write, C: Into<RecallCtx>>(
    input: R,
    mut output: W,
    ctx: C,
) -> i32 {
    let ctx: RecallCtx = ctx.into();
    for line in input.lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else {
            // No id to answer with, and the spec has no way to report a parse error on a stream
            // whose framing may itself be broken. Skip and keep serving.
            continue;
        };
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = req.get("id").cloned();
        // Notifications carry no id and MUST NOT be answered.
        let Some(id) = id else { continue };

        let resp = match method {
            "initialize" => {
                // Echo the client's protocol version: the spec requires a server that supports the
                // requested version to answer with that same version, and a tools-only server
                // supports every version there is. Nothing to keep in sync.
                let version = req
                    .pointer("/params/protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("2025-06-18");
                rpc_result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "recompact", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": INSTRUCTIONS,
                    }),
                )
            }
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, tools_list()),
            "tools/call" => {
                let name = req
                    .pointer("/params/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if name == "recall" {
                    let args = req
                        .pointer("/params/arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    rpc_result(id, call_recall(&ctx, &args))
                } else {
                    rpc_error(id, -32602, &format!("unknown tool: {name}"))
                }
            }
            other => rpc_error(id, -32601, &format!("method not found: {other}")),
        };

        if writeln!(output, "{}", serde_json::to_string(&resp).unwrap_or_default()).is_err() {
            return 1;
        }
        // Explicit: stdout block-buffers when it is a pipe, which is exactly the MCP case, and a
        // response sitting in the buffer reads as a hung server.
        if output.flush().is_err() {
            return 1;
        }
    }
    0
}

/// The session a process runs inside, when Claude Code says so (it exports the id to MCP servers
/// and to Bash tool subprocesses alike).
pub fn current_session_id() -> Option<String> {
    std::env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn recall_ctx_from(args_dir: Option<&str>) -> Option<RecallCtx> {
    let dir = args_dir
        .map(PathBuf::from)
        .or_else(|| std::env::var("CLAUDE_PROJECT_DIR").ok().and_then(|d| project_dir_for(Path::new(&d))))
        .or_else(|| std::env::current_dir().ok().and_then(|d| project_dir_for(&d)))?;
    let session = current_session_id();
    // The session file decides the project dir when the two disagree (worktree resumes move it).
    let dir = session
        .as_deref()
        .and_then(|s| locate_session(Some(&session_path(&dir, s)), s))
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or(dir);
    Some(RecallCtx { dir, session })
}

/// Serve MCP on stdio. Started automatically by the plugin; not normally run by hand.
pub fn cmd_mcp(args: &[String]) -> i32 {
    let (pos, _opts) = parse_opts(args);
    let Some(ctx) = recall_ctx_from(pos.first().map(String::as_str)) else {
        eprintln!("error: cannot resolve a project transcript dir (set CLAUDE_PROJECT_DIR)");
        return 1;
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    mcp_serve(stdin.lock(), stdout.lock(), ctx)
}

/// `recall` from a shell: the same engine as the MCP tool, printed. Inside a Claude Code Bash
/// tool the session is known from the environment, exactly as the server knows it.
pub fn cmd_recall(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    let Some(mut ctx) = recall_ctx_from(opts.get("dir").and_then(|v| v.as_str())) else {
        eprintln!("error: cannot resolve a project transcript dir");
        return 1;
    };
    if let Some(s) = opts.get("session").and_then(|v| v.as_str()) {
        ctx.session = Some(s.to_string());
    }
    let mut call = json!({});
    if let Some(q) = opts.get("query").and_then(|v| v.as_str()) {
        call["query"] = json!(q);
    }
    if let Some(sel) = pos.first() {
        call["selector"] = json!(sel);
    }
    if let Some(c) = opts.get("chunk").and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()) {
        call["chunk"] = json!(c);
    }
    let res = call_recall(&ctx, &call);
    let mut rc = 0;
    if res["isError"].as_bool().unwrap_or(false) {
        rc = 1;
    }
    for c in res["content"].as_array().cloned().unwrap_or_default() {
        match c["type"].as_str() {
            Some("text") => println!("{}", c["text"].as_str().unwrap_or("")),
            Some("image") => println!("[image: {} base64 bytes, {}]", c["data"].as_str().map_or(0, str::len), c["mimeType"].as_str().unwrap_or("?")),
            _ => {}
        }
    }
    rc
}

/// `recompact hook <event>`: hook entry points. Reads the hook's JSON on stdin; prints hook
/// output JSON on stdout, or nothing. Never fails loudly — a broken hook must not block a resume.
pub fn cmd_hook(args: &[String]) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    // recompact's own headless summarizer and prewarm runs.
    if std::env::var_os("RECOMPACT_INTERNAL").is_some() {
        return 0;
    }
    let Ok(v) = serde_json::from_str::<Value>(&input) else {
        return 0;
    };
    let out = match args.first().map(String::as_str) {
        Some("session-start") => {
            on_session_start(&v);
            session_start_context(&v).map(|ctx| {
                json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": ctx}})
            })
        }
        Some("user-prompt-submit") => on_prompt(&v),
        Some("stop") => on_stop(&v),
        Some("post-tool-use") => on_post_tool_use(&v),
        _ => None,
    };
    if let Some(o) = out {
        println!("{o}");
    }
    0
}
