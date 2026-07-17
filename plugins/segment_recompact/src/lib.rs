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
recompact extract  <session.jsonl> [--out work/segments.json] [--keep K]\n  \
recompact assemble <session.jsonl> <summaries.json> [--keep K] [--out <path>]\n  \
recompact verify   <assembled.jsonl> [--source <session.jsonl>]\n\n\
  --keep K  number of most-recent segments kept verbatim (default 1)";

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

/// A genuine human-authored user turn: a segment boundary, always kept verbatim.
///
/// Fail-open on retention: any user record that is not a tool result, a meta record, or a
/// compaction summary counts as genuine — including content shapes this tool doesn't know
/// (image-first turns, future block types). Misclassifying a real prompt as agent activity would
/// let a collapse silently drop it; misclassifying activity as a prompt only costs compression.
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

/// One retention decision per segment, shared by extract and assemble so the two passes can never
/// disagree. The last `keep` segments stay verbatim. A segment whose activity carries a compaction
/// summary (isCompactSummary) is pinned verbatim regardless of age: that record is the only
/// surviving carrier of everything before its boundary, and a hand-written summary of a summary is
/// exactly the recursive loss this tool exists to avoid.
pub fn plan(records: &[Value], segs: &[Segment], keep: usize) -> Vec<SegPlan> {
    segs.iter()
        .enumerate()
        .map(|(s, seg)| {
            let tail = s + keep >= segs.len();
            let pinned = seg
                .activity
                .iter()
                .any(|&i| truthy(&records[i], "isCompactSummary"));
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
    let mut positional = Vec::new();
    let mut opts = Map::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(key) = a.strip_prefix("--") {
            let val = args.get(i + 1).cloned().unwrap_or_default();
            opts.insert(key.to_string(), Value::String(val));
            i += 2;
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

    let loaded = load_jsonl(&src);
    let total_in_file = loaded.len();
    let (records, off_path) = select_active(loaded);
    let (head, segs) = segment(&records);
    let plans = plan(&records, &segs, keep);

    let mut seg_json = Vec::new();
    for (s, seg) in segs.iter().enumerate() {
        // Render activity for Claude to read.
        let mut activity = Vec::new();
        let mut covered = vec![records[seg.user_idx]
            .get("uuid")
            .cloned()
            .unwrap_or(Value::Null)];
        for &i in &seg.activity {
            let r = &records[i];
            if let Some(u) = rec_uuid(r) {
                covered.push(Value::String(u.to_string()));
            }
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
                    } else if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
                        for b in blocks {
                            if b.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                                let text = tool_result_text(b);
                                activity.push(json!({
                                    "kind": "tool_result",
                                    "is_error": b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
                                    "result": truncate(&text, TOOL_RESULT_TRUNC)
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

        seg_json.push(json!({
            "index": s,
            "user_text": user_text(&records[seg.user_idx]),
            "has_agent_activity": has_agent_activity(&records, seg),
            "needs_summary": plans[s].needs_summary,
            "kept_verbatim": plans[s].kept_verbatim,
            "covered_uuids": covered,
            "approx_tokens": approx_tokens(
                &std::iter::once(seg.user_idx)
                    .chain(seg.activity.iter().copied())
                    .map(|i| records[i].clone())
                    .collect::<Vec<_>>()
            ),
            "activity": activity,
        }));
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

    let needs: Vec<usize> = seg_json
        .iter()
        .filter(|s| s["needs_summary"].as_bool() == Some(true))
        .map(|s| s["index"].as_u64().unwrap() as usize)
        .collect();

    let doc = json!({
        "source": src.canonicalize().unwrap_or(src.clone()).to_string_lossy(),
        "original_session_id": session_id,
        "leaf_uuid": leaf,
        "total_records": records.len(),
        "off_path_dropped": off_path,
        "head_record_count": head.len(),
        "approx_tokens_total": approx_tokens(&records),
        "keep_verbatim_last": keep,
        "segments_needing_summary": needs,
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
        "extract: {} records in file, {} on active path ({} off-path dropped) → {} segments ({} head, ~{} tokens). Summaries needed for segments {:?}. Worksheet: {}",
        total_in_file,
        records.len(),
        off_path,
        segs.len(),
        head.len(),
        approx_tokens(&records),
        needs,
        out.display()
    );
    0
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
    let (pos, opts) = parse_opts(args);
    if pos.len() < 2 {
        return usage();
    }
    let src = PathBuf::from(&pos[0]);
    let summaries_path = PathBuf::from(&pos[1]);
    let keep = keep_window(&opts);

    let loaded = load_jsonl(&src);
    let (records, _off_path) = select_active(loaded);
    let (head, segs) = segment(&records);
    let plans = plan(&records, &segs, keep);

    let summaries: Value = {
        let mut s = String::new();
        if let Err(e) = fs::File::open(&summaries_path).and_then(|mut f| f.read_to_string(&mut s)) {
            eprintln!("error: cannot read {}: {e}", summaries_path.display());
            return 1;
        }
        match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {} is not valid JSON: {e}", summaries_path.display());
                return 1;
            }
        }
    };
    let get_summary = |idx: usize| -> Option<String> {
        summaries
            .get(idx.to_string())
            .and_then(|v| v.as_str())
            .map(String::from)
    };

    // Validate: every segment that needs a summary has one.
    let mut missing = Vec::new();
    for (s, p) in plans.iter().enumerate() {
        if p.needs_summary && get_summary(s).is_none() {
            missing.push(s);
        }
    }
    if !missing.is_empty() {
        eprintln!(
            "error: missing summaries for segments {missing:?} in {}",
            summaries_path.display()
        );
        return 1;
    }

    let new_session = uuid_v4();

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

    let mut out: Vec<Value> = Vec::new();

    // Head: keep only records that carry a uuid (drop ephemeral scaffolding like queue-operation).
    for &i in &head {
        if rec_uuid(&records[i]).is_some() {
            out.push(records[i].clone());
        }
    }

    for (s, seg) in segs.iter().enumerate() {
        out.push(records[seg.user_idx].clone());
        if plans[s].kept_verbatim {
            for &i in &seg.activity {
                if rec_uuid(&records[i]).is_some() {
                    out.push(records[i].clone());
                }
            }
        } else if plans[s].needs_summary {
            let ts = records[seg.user_idx]
                .get("timestamp")
                .cloned()
                .unwrap_or(Value::Null);
            out.push(json!({
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
                "message": {
                    "id": format!("msg_recompact_{}", uuid_v4().replace('-', "")),
                    "role": "assistant",
                    "model": model,
                    "type": "message",
                    "stop_reason": "end_turn",
                    "content": [{ "type": "text", "text": get_summary(s).unwrap() }]
                }
            }));
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
        return 1;
    }
    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: cannot create {}: {e}", out_path.display());
            return 1;
        }
    };
    for r in &out {
        if let Err(e) = writeln!(f, "{}", serde_json::to_string(r).unwrap()) {
            eprintln!("error: write failed: {e}");
            return 1;
        }
    }

    eprintln!(
        "assemble: {} → {} records (~{} → ~{} tokens, keep last {} verbatim).\n  new sessionId: {}\n  wrote: {}",
        records.len(),
        out.len(),
        approx_tokens(&records),
        approx_tokens(&out),
        keep,
        new_session,
        out_path.display()
    );
    println!("{new_session}"); // stdout: the new sessionId, for scripting / `claude --resume`
    0
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
