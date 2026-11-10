//! `recompact eval`: does Claude carry on as well from a twin as from the full history?
//!
//! Hindsight makes this measurable offline. Cut one of the owner's long sessions at a past user
//! turn, build the twin of everything before the cut, and the session file already says what
//! really came next: the user's next message, the agent's first action, which details the work
//! used again. Every arm (the full prefix, the current twin, a candidate twin) runs on the same
//! cut points, so two versions compare point by point.
//!
//! - Tier 0, no model calls: which details the future used are still in the context.
//! - Tier 1, one call: the resumed model lists the files it changed and the last check's result.
//! - Tier 2, one call: the resumed model gets the real next message; its first action is recorded
//!   by a PreToolUse hook that denies it and stops the run, so nothing it proposes executes.
//!
//! Points use first compactions only: a source must be an original session, not a twin, because
//! re-deriving a twin needs its lineage's originals beside it.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::accounting::prompt_tokens;
use crate::brief::{commands_run, tool_result_texts};
use crate::{
    calibrate_lineage, content, continue_file, extract_json_object, human_words, is_genuine_user,
    last_check, last_reply_model, load_jsonl, parse_opts, parse_size, project_dir_for, rec_type,
    rec_uuid, select_active, truthy, uuid_v4, visible_identifiers, write_cache_merged, Mentions,
    DEFAULT_THRESHOLD,
};

pub const EVAL_USAGE: &str = "usage:\n  \
recompact eval points [project-dir ...] [--days 30] [--min 200k] [--max 850k] [--gap 150k]\n                        \
[--per-session 2] [--limit 40] [--out F] [--force]\n                        \
(choose cut points in long sessions that ran on uncompacted)\n  \
recompact eval run --arm NAME [--original] [--target 120k] [--summarize-with M | --mask]\n                        \
[--model opus] [--tier0] [--samples 1] [--probe] [--only id,id] [--jobs 2]\n                        \
[--points F] [--out F] [--save-to DIR]  (keep each point's prefix and context, to inspect)\n                        \
(score one arm on every point; nothing the resumed model proposes runs)\n  \
recompact eval report [results.jsonl ...]  (compare the arms on their shared points)";

/// What the Tier 2 hook answers every tool call with: not run, and stop the session.
const DENY_SETTINGS: &str = r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"printf '%s' '{\"continue\":false,\"stopReason\":\"recompact eval: proposal recorded\",\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"recompact eval: not run\"}}'"}]}]}}"#;

const PROBE_PROMPT: &str = "Pause the work for a moment and answer from what you know of this session. \
Do not use any tools. Reply with one JSON object and nothing else, in this shape: \
{\"files_changed\": [\"every file path you created or edited in this session\"], \
\"last_check\": {\"command\": \"the last test, build, lint or type-check command you ran\", \
\"result\": \"passed, failed or unknown\"}}";

const CALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

pub fn eval_home() -> PathBuf {
    crate::launcher::recompact_home().join("eval")
}

pub fn cmd_eval(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("points") => eval_points(&args[1..]),
        Some("run") => eval_run(&args[1..]),
        Some("report") => eval_report(&args[1..]),
        _ => {
            eprintln!("{EVAL_USAGE}");
            2
        }
    }
}

fn opt_size(opts: &Map<String, Value>, k: &str, default: usize) -> usize {
    opts.get(k)
        .and_then(|v| v.as_str())
        .and_then(|s| parse_size(s).or_else(|| s.parse().ok()))
        .unwrap_or(default)
}

fn opt_str<'a>(opts: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    opts.get(k).and_then(|v| v.as_str())
}

// ------------------------------------------------------------------------------- record helpers

fn user_text(r: &Value) -> String {
    match content(r) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tool_uses(r: &Value) -> Vec<(String, String, Value)> {
    if rec_type(r) != "assistant" {
        return Vec::new();
    }
    content(r)
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .map(|b| {
                    (
                        b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        b.get("input").cloned().unwrap_or(Value::Null),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn has_text(r: &Value) -> bool {
    rec_type(r) == "assistant"
        && content(r).and_then(|c| c.as_array()).is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("text")
                    && b.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.trim().is_empty())
            })
        })
}

/// The agent's first action after a human turn, from the records that follow it: its first tool
/// call, or `text` when it answered without one. None when no reply came before the next human
/// turn (an interrupted or abandoned turn).
fn first_action(after: &[Value]) -> Option<String> {
    let mut answered = false;
    for r in after {
        if is_genuine_user(r) {
            break;
        }
        if let Some((_, name, input)) = tool_uses(r).into_iter().next() {
            return Some(canonical(&name, &input));
        }
        answered |= has_text(r);
    }
    answered.then(|| "text".to_string())
}

/// An action as a short comparable key: the tool and what it acted on.
pub fn canonical(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "Read" | "Edit" | "Write" | "MultiEdit" => format!("{name} {}", s("file_path")),
        "NotebookEdit" => format!("{name} {}", s("notebook_path")),
        "Bash" => {
            let cmd = commands_run(s("command"))
                .into_iter()
                .find(|c| !c.starts_with("cd ") && c != "cd")
                .unwrap_or_default();
            // The program and its subcommand (`git commit`, `cargo test`, `gh pr`), not its
            // arguments: a link, a path or a redirection says nothing about the kind of action.
            let mut words = cmd.split_whitespace();
            let program = words.next().unwrap_or("");
            match words.next().filter(|w| w.chars().all(|c| c.is_ascii_lowercase() || c == '-') && !w.starts_with('-')) {
                Some(sub) => format!("Bash {program} {sub}"),
                None => format!("Bash {program}"),
            }
        }
        "Grep" | "Glob" => format!("{name} {}", s("pattern")),
        "Agent" | "Task" => format!("Agent {}", s("subagent_type")),
        "Skill" => format!("Skill {}", s("skill")),
        "WebFetch" => {
            let url = s("url");
            let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("");
            format!("WebFetch {host}")
        }
        _ => name.to_string(),
    }
}

fn tool_of(action: &str) -> &str {
    action.split(' ').next().unwrap_or(action)
}

/// Last two path components, lowercased: absolute and relative spellings of one file compare equal.
fn path_key(p: &str) -> String {
    let parts: Vec<&str> = p.trim().trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/").to_lowercase()
}

/// Files the prefix created or edited, by successful tool calls.
fn files_changed(records: &[Value]) -> HashSet<String> {
    let mut failed: HashSet<String> = HashSet::new();
    for r in records {
        for (id, _, err) in tool_result_texts(r) {
            if err {
                failed.insert(id);
            }
        }
    }
    let mut out = HashSet::new();
    for r in records {
        for (id, name, input) in tool_uses(r) {
            let key = match name.as_str() {
                "Edit" | "Write" | "MultiEdit" => input.get("file_path"),
                "NotebookEdit" => input.get("notebook_path"),
                _ => None,
            };
            if let Some(p) = key.and_then(|v| v.as_str()) {
                if !failed.contains(&id) {
                    out.insert(path_key(p));
                }
            }
        }
    }
    out
}

fn check_outcome(records: &[Value]) -> Option<&'static str> {
    let line = last_check(records)?;
    Some(if line.contains("FAILED") || line.contains("reports failures") {
        "failed"
    } else {
        "passed"
    })
}

// ------------------------------------------------------------------------------------- points

fn eval_points(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    let days = opt_str(&opts, "days").and_then(|s| s.parse::<u64>().ok()).unwrap_or(30);
    let min = opt_size(&opts, "min", 200_000);
    let max = opt_size(&opts, "max", 850_000);
    let gap = opt_size(&opts, "gap", 150_000);
    let per = opt_str(&opts, "per-session").and_then(|s| s.parse().ok()).unwrap_or(2usize);
    let limit = opt_str(&opts, "limit").and_then(|s| s.parse().ok()).unwrap_or(40usize);
    let home = eval_home();
    let out = opt_str(&opts, "out").map(PathBuf::from).unwrap_or_else(|| home.join("points.json"));
    if out.exists() && !opts.contains_key("force") {
        eprintln!(
            "eval: {} exists; the same points must serve every arm. Pass --force to replace it.",
            out.display()
        );
        return 1;
    }

    let dirs: Vec<PathBuf> = if pos.is_empty() {
        let root = crate::launcher::recompact_home()
            .parent()
            .map(|p| p.join("projects"))
            .unwrap_or_default();
        fs::read_dir(&root)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
            .unwrap_or_default()
    } else {
        pos.iter().map(PathBuf::from).collect()
    };
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(days * 86_400);
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for d in &dirs {
        // Our own scratch projects are not sources.
        if d.to_string_lossy().contains("recompact-eval-work") {
            continue;
        }
        let Ok(rd) = fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let Ok(mtime) = md.modified() else { continue };
            if md.len() >= 1_000_000 && mtime >= cutoff {
                files.push((mtime, p));
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));

    let sources = home.join("sources");
    if let Err(e) = fs::create_dir_all(&sources) {
        eprintln!("eval: cannot create {}: {e}", sources.display());
        return 1;
    }
    let mut points: Vec<Value> = Vec::new();
    for (_, path) in &files {
        if points.len() >= limit {
            break;
        }
        let Ok(raw) = fs::read_to_string(path) else { continue };
        if raw.contains("\"recompactPreamble\"") || raw.contains("\"recompactSynthetic\"") {
            continue;
        }
        drop(raw);
        let (active, _) = select_active(load_jsonl(path));
        let found = cut_points(&active, min, max, gap, per);
        if found.is_empty() {
            continue;
        }
        let sid = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        for (n, (i, live)) in found.into_iter().enumerate() {
            if points.len() >= limit {
                break;
            }
            let id = format!("{}-{}", &sid[..sid.len().min(8)], n + 1);
            // A frozen copy of the path through the cut: a live session keeps growing, Claude
            // Code's own compaction starts a new chain in the same file, and its 30-day cleanup
            // deletes it, so the point must not depend on the session file.
            let end = window_end(&active, i);
            let frozen = sources.join(format!("{id}.jsonl"));
            if let Err(e) = write_session(&frozen, &active[..end], &sid) {
                eprintln!("eval: {e}");
                continue;
            }
            let r = &active[i];
            points.push(json!({
                "id": id,
                "source": frozen.to_string_lossy(),
                "origin_dir": path.parent().map(|p| p.to_string_lossy().into_owned()),
                "cut": rec_uuid(r),
                "live": live,
                "cwd": r.get("cwd"),
                "model": last_reply_model(&active[..i]),
            }));
        }
    }
    let doc = json!({"version": 1, "created": crate::brief::format_utc(crate::brief::now_unix()), "points": points});
    if let Err(e) = fs::write(&out, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
        eprintln!("eval: cannot write {}: {e}", out.display());
        return 1;
    }
    println!("{} point(s) from {} session file(s) → {}", points.len(), files.len(), out.display());
    0
}

/// The end of the continuation a point keeps: up to its fourth human turn after the cut, at most
/// 400 records.
fn window_end(active: &[Value], cut: usize) -> usize {
    let mut humans = 0;
    for (k, r) in active.iter().enumerate().skip(cut) {
        if is_genuine_user(r) {
            humans += 1;
            if humans > 3 {
                return k;
            }
        }
        if k - cut >= 400 {
            return k;
        }
    }
    active.len()
}

/// Human turns where the live context first passes `min`, then every `gap` more, up to `per`
/// per session. A cut needs a real typed message, a reply to it, and no Claude Code compaction
/// before it.
fn cut_points(active: &[Value], min: usize, max: usize, gap: usize, per: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut live = 0usize;
    let mut level = min;
    for (i, r) in active.iter().enumerate() {
        if truthy(r, "isCompactSummary") {
            break;
        }
        if rec_type(r) == "assistant" {
            if let Some(t) = r.pointer("/message/usage").and_then(prompt_tokens) {
                live = t;
            }
            continue;
        }
        if live < level || live > max || !is_genuine_user(r) {
            continue;
        }
        let raw = user_text(r);
        let words = human_words(&raw);
        if words.trim().is_empty() || raw.contains("<command-name>") || words.trim_start().starts_with('/') {
            continue;
        }
        if first_action(&active[i + 1..]).is_none() {
            continue;
        }
        out.push((i, live));
        level = live + gap;
        if out.len() >= per {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------- run

struct Arm {
    name: String,
    original: bool,
    copts: crate::ContinueOpts,
    model: String,
    tier0: bool,
    /// `--save-to DIR`: keep each point's prefix and resumed context there, to see what a twin lost.
    save_to: Option<PathBuf>,
    /// `--samples K`: first-action samples per point (default 1).
    samples: usize,
    /// `--probe`: also ask for the files changed and the last check's result.
    probe: bool,
}

fn eval_run(args: &[String]) -> i32 {
    let (_, opts) = parse_opts(args);
    let Some(name) = opt_str(&opts, "arm").map(String::from) else {
        eprintln!("{EVAL_USAGE}");
        return 2;
    };
    let home = eval_home();
    let points_path = opt_str(&opts, "points").map(PathBuf::from).unwrap_or_else(|| home.join("points.json"));
    let doc: Value = match fs::read_to_string(&points_path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(v) => v,
        None => {
            eprintln!("eval: no points at {}; run `recompact eval points` first", points_path.display());
            return 1;
        }
    };
    let only: Option<HashSet<String>> =
        opt_str(&opts, "only").map(|s| s.split(',').map(|x| x.trim().to_string()).collect());
    let points: Vec<Value> = doc["points"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| only.as_ref().is_none_or(|o| o.contains(p["id"].as_str().unwrap_or(""))))
        .collect();
    let mut copts = crate::launcher::continue_opts(&opts, Some("haiku"));
    copts.threshold = 0;
    copts.force = true;
    copts.target = Some(opt_size(&opts, "target", DEFAULT_THRESHOLD));
    let arm = Arc::new(Arm {
        name: name.clone(),
        original: opts.contains_key("original"),
        copts,
        model: opt_str(&opts, "model").unwrap_or("opus").to_string(),
        tier0: opts.contains_key("tier0"),
        save_to: opt_str(&opts, "save-to").map(PathBuf::from),
        // One by default: a later sample reuses the first one's cached prompt only when both
        // reach the same account, and through an account pool most did not.
        samples: opt_str(&opts, "samples").and_then(|v| v.parse().ok()).unwrap_or(1usize).max(1),
        probe: opts.contains_key("probe"),
    });
    let out = opt_str(&opts, "out")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("results").join(format!("{name}.jsonl")));
    if let Some(dir) = out.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let done: HashSet<String> = fs::read_to_string(&out)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["ok"] == json!(true) && v["arm"].as_str() == Some(name.as_str()))
        .filter_map(|v| v["point"].as_str().map(String::from))
        .collect();
    let todo: Vec<Value> = points
        .into_iter()
        .filter(|p| !done.contains(p["id"].as_str().unwrap_or("")))
        .collect();
    let jobs = opt_str(&opts, "jobs").and_then(|s| s.parse().ok()).unwrap_or(2usize).max(1);
    eprintln!(
        "eval: arm {name}: {} point(s) to run ({} already scored) → {}",
        todo.len(),
        done.len(),
        out.display()
    );
    // A twin built for a past moment must not describe the repository as it is today.
    std::env::set_var("RECOMPACT_EVAL", "1");

    let queue = Arc::new(Mutex::new(todo.into_iter().collect::<std::collections::VecDeque<Value>>()));
    let sink = Arc::new(Mutex::new(out.clone()));
    let failures = Arc::new(Mutex::new(0usize));
    let handles: Vec<_> = (0..jobs)
        .map(|_| {
            let (queue, sink, arm, failures) = (queue.clone(), sink.clone(), arm.clone(), failures.clone());
            std::thread::spawn(move || loop {
                let Some(p) = queue.lock().unwrap().pop_front() else { break };
                let id = p["id"].as_str().unwrap_or("").to_string();
                let started = Instant::now();
                let mut row = match run_point(&p, &arm) {
                    Ok(v) => v,
                    Err(e) => {
                        *failures.lock().unwrap() += 1;
                        json!({"ok": false, "error": e})
                    }
                };
                row["point"] = json!(id);
                row["arm"] = json!(arm.name);
                row["version"] = json!(env!("CARGO_PKG_VERSION"));
                row["model"] = json!(arm.model);
                row["secs"] = json!(started.elapsed().as_secs());
                let line = serde_json::to_string(&row).unwrap_or_default();
                eprintln!("eval: {id}: {}", summary_line(&row));
                let path = sink.lock().unwrap();
                if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&*path) {
                    let _ = writeln!(f, "{line}");
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    let failed = *failures.lock().unwrap();
    eprintln!("eval: done; {failed} point(s) failed. `recompact eval report` compares the arms.");
    i32::from(failed > 0)
}

fn summary_line(row: &Value) -> String {
    if row["ok"] != json!(true) {
        return format!("FAILED: {}", row["error"].as_str().unwrap_or("?"));
    }
    let d = &row["details"];
    let mut s = format!(
        "{}k tokens, details {}/{}",
        row["context_tokens"].as_u64().unwrap_or(0) / 1000,
        d["kept"],
        d["total"]
    );
    if let Some(n) = row.get("next").filter(|n| !n.is_null()) {
        s.push_str(&format!(
            "; first action same {:.0}% (real: {}, proposed: {})",
            100.0 * share(&n["same"]),
            n["truth"].as_str().unwrap_or(""),
            n["proposals"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" / ")).unwrap_or_default()
        ));
    }
    s
}

/// A scratch project for one point: its own cwd, so its Claude Code project dir, lineage sidecar
/// and summary cache belong to nothing else. Removed whole when the point is scored.
struct Scratch {
    cwd: PathBuf,
    project: PathBuf,
}

impl Scratch {
    fn new(point: &str, arm: &str) -> Result<Scratch, String> {
        let cwd = eval_home()
            .join("recompact-eval-work")
            .join(format!("{arm}-{point}-{}", &uuid_v4()[..8]));
        fs::create_dir_all(&cwd).map_err(|e| format!("cannot create {}: {e}", cwd.display()))?;
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        let project = project_dir_for(&cwd).ok_or("no home directory")?;
        fs::create_dir_all(&project).map_err(|e| format!("cannot create {}: {e}", project.display()))?;
        Ok(Scratch { cwd, project })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Both paths were derived from our own work dir; never remove anything else.
        if self.project.to_string_lossy().contains("recompact-eval-work") {
            let _ = fs::remove_dir_all(&self.project);
        }
        if self.cwd.to_string_lossy().contains("recompact-eval-work") {
            let _ = fs::remove_dir_all(&self.cwd);
        }
    }
}

fn write_session(path: &Path, records: &[Value], session_id: &str) -> Result<(), String> {
    let mut f = fs::File::create(path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    for r in records {
        let mut r = r.clone();
        if r.get("sessionId").is_some() {
            r["sessionId"] = json!(session_id);
        }
        writeln!(f, "{}", serde_json::to_string(&r).unwrap_or_default()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The summary cache an arm reads and grows. Summaries depend on the summarizer, so each model
/// has its own; the default model starts from the source project's cache, which real
/// compactions filled with the same model.
fn arm_cache(arm: &Arm, origin_dir: Option<&str>) -> Option<PathBuf> {
    let model = arm.copts.summarize.as_ref()?.model.clone();
    let path = eval_home().join(format!("summary-cache-{model}.json"));
    if model == "haiku" {
        // Real compactions of this point's own session may have summarized the same units.
        let seed = origin_dir
            .map(|d| Path::new(d).join(".recompact-summary-cache.json"))
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Map<String, Value>>(&s).ok());
        if let Some(seed) = seed {
            let _ = write_cache_merged(&path, &seed);
        }
    }
    Some(path)
}

fn run_point(p: &Value, arm: &Arm) -> Result<Value, String> {
    let id = p["id"].as_str().unwrap_or("");
    let source = PathBuf::from(p["source"].as_str().ok_or("point has no source")?);
    let cut = p["cut"].as_str().ok_or("point has no cut")?;
    let (active, _) = select_active(load_jsonl(&source));
    let at = active
        .iter()
        .position(|r| rec_uuid(r) == Some(cut))
        .ok_or_else(|| format!("cut {cut} is no longer on the active path of {}", source.display()))?;
    let (prefix, future) = active.split_at(at);
    let truth = first_action(&future[1..]).ok_or("the cut turn has no reply")?;
    let message = human_words(&user_text(&future[0]));

    let scratch = Scratch::new(id, &arm.name)?;
    let prefix_id = uuid_v4();
    write_session(&scratch.project.join(format!("{prefix_id}.jsonl")), prefix, &prefix_id)?;

    let ctx_id = if arm.original {
        prefix_id.clone()
    } else {
        let cache = arm_cache(arm, p["origin_dir"].as_str());
        let local = scratch.project.join(".recompact-summary-cache.json");
        if let Some(c) = &cache {
            let _ = fs::copy(c, &local);
        }
        let (twin, rc) = continue_file(&scratch.project, &prefix_id, &arm.copts);
        if let (Some(c), Some(fresh)) = (
            &cache,
            fs::read_to_string(&local).ok().and_then(|s| serde_json::from_str::<Map<String, Value>>(&s).ok()),
        ) {
            let _ = write_cache_merged(c, &fresh);
        }
        if rc != 0 || twin == prefix_id {
            return Err(format!("no twin was built (exit {rc})"));
        }
        twin
    };
    let ctx_file = scratch.project.join(format!("{ctx_id}.jsonl"));
    if let Some(dir) = &arm.save_to {
        let _ = fs::create_dir_all(dir);
        let _ = fs::copy(scratch.project.join(format!("{prefix_id}.jsonl")), dir.join(format!("{id}.prefix.jsonl")));
        let _ = fs::copy(&ctx_file, dir.join(format!("{id}.context.jsonl")));
    }
    let (ctx, _) = select_active(load_jsonl(&ctx_file));
    let calib = calibrate_lineage(prefix);
    let mut row = json!({
        "ok": true,
        "context_tokens": calib.context_tokens(&ctx),
        "prefix_tokens": calib.context_tokens(prefix),
        "details": details_kept(prefix, future, &ctx),
    });
    if arm.tier0 {
        return Ok(row);
    }

    let prior: HashSet<String> = prefix
        .iter()
        .flat_map(tool_uses)
        .map(|(_, name, input)| canonical(&name, &input))
        .collect();
    // With several samples a point says how often, not just whether, the first action matches.
    let mut proposals: Vec<String> = Vec::new();
    let (mut tools, mut cost, mut usage) = (HashSet::new(), 0.0, Vec::new());
    for _ in 0..arm.samples {
        let next = resume_call(&scratch, &ctx_id, &arm.model, &message)?;
        proposals.push(next.action.clone().unwrap_or_else(|| "text".into()));
        if proposals.len() == 1 {
            row["next_text"] = json!(next.text.chars().take(600).collect::<String>());
        }
        tools.extend(next.tools);
        cost += next.cost;
        usage.push(next.usage);
    }
    let rate = |f: &dyn Fn(&String) -> bool| proposals.iter().filter(|p| f(p)).count() as f64 / proposals.len() as f64;
    row["next"] = json!({
        "truth": truth,
        "proposed": proposals[0],
        "proposals": proposals,
        "same": rate(&|p| *p == truth),
        "same_tool": rate(&|p| tool_of(p) == tool_of(&truth)),
        "repeat": rate(&|p| p != "text" && prior.contains(p)),
        "truth_repeat": truth != "text" && prior.contains(&truth),
        "recall": rate(&|p| p.contains("recall")),
        "unavailable": truth != "text" && !tools.is_empty() && !tools.contains(tool_of(&truth))
            && !(tool_of(&truth) == "Agent" && tools.contains("Task")),
    });

    if arm.probe {
        let probe = resume_call(&scratch, &ctx_id, &arm.model, PROBE_PROMPT)?;
        let answer = extract_json_object(&probe.text);
        let truth_files = files_changed(prefix);
        let said: HashSet<String> = answer
            .as_ref()
            .and_then(|a| a.get("files_changed"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(path_key).collect())
            .unwrap_or_default();
        let check_said = answer
            .as_ref()
            .and_then(|a| a.get("last_check"))
            .and_then(|c| c.get("result"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_lowercase());
        row["probe"] = json!({
            "answered": answer.is_some(),
            "action": probe.action,
            "text": probe.text.chars().take(1500).collect::<String>(),
            "files_truth": truth_files.len(),
            "files_said": said.len(),
            "files_hit": truth_files.intersection(&said).count(),
            "check_truth": check_outcome(prefix),
            "check_said": check_said,
        });
        cost += probe.cost;
        usage.push(probe.usage);
    }
    row["cost_usd"] = json!(cost);
    row["usage"] = json!(usage);
    Ok(row)
}

/// Tier 0: the identifiers the future window used again — paths, names, ids, errors the human
/// or the agent mentioned after the cut that the prefix already held — and how many of them the
/// resumed context still shows.
fn details_kept(prefix: &[Value], future: &[Value], ctx: &[Value]) -> Value {
    let mut humans = 0;
    let window: Vec<Value> = future
        .iter()
        .take_while(|r| {
            if is_genuine_user(r) {
                humans += 1;
            }
            humans <= 3
        })
        .take(400)
        .cloned()
        .collect();
    let joined: Vec<Value> = prefix.iter().chain(window.iter()).cloned().collect();
    let m = Mentions::build(&joined);
    let n = prefix.len();
    let cwd = prefix.iter().rev().find_map(|r| r.get("cwd").and_then(|v| v.as_str())).unwrap_or("");
    let mut wanted: Vec<(u32, String)> = Vec::new();
    for (ident, occ) in &m.by_ident {
        let before = occ.iter().any(|(i, _)| *i < n);
        // Tool output is noisy (an `ls` names every file): a later use must come from the human
        // or the agent.
        let later: u32 = occ.iter().filter(|(i, w)| *i >= n && *w >= 2).map(|(_, w)| *w as u32).sum();
        let base = ident.rsplit('/').next().unwrap_or(ident);
        let background = matches!(base, "CLAUDE.md" | "CLAUDE.local.md" | "AGENTS.md" | "MEMORY.md")
            || (!cwd.is_empty() && cwd.starts_with(ident.as_str()));
        if before && later > 0 && !background {
            wanted.push((later, ident.clone()));
        }
    }
    let visible = visible_identifiers(ctx, &m.vocab);
    let mut lost: Vec<(u32, String)> = wanted.iter().filter(|(_, i)| !visible.contains(i)).cloned().collect();
    lost.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    json!({
        "total": wanted.len(),
        "kept": wanted.len() - lost.len(),
        "lost": lost.into_iter().take(8).map(|(_, i)| i).collect::<Vec<_>>(),
    })
}

struct CallResult {
    action: Option<String>,
    text: String,
    cost: f64,
    usage: Value,
    /// The tools this environment offered; a real action with a tool missing here (a desktop
    /// app's own tools, an MCP server that is not configured) cannot be matched.
    tools: HashSet<String>,
}

/// One fork of the resumed context with `prompt` as the next user turn. Every tool call is denied
/// by the hook, which also stops the session, so the run ends at the first proposed action.
fn resume_call(s: &Scratch, session: &str, model: &str, prompt: &str) -> Result<CallResult, String> {
    let bin = std::env::var("RECOMPACT_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let mut child = Command::new(&bin)
        .current_dir(&s.cwd)
        .args(["-p", "--resume", session, "--fork-session", "--no-session-persistence"])
        .args(["--model", model, "--output-format", "stream-json", "--verbose"])
        .args(["--settings", DENY_SETTINGS])
        // A launcher, a background job or a desktop gateway around this process must not treat
        // the fork as one of its sessions.
        .env_remove("RECOMPACT_SHELL")
        .env_remove("RECOMPACT_PTY")
        .env_remove("CLAUDE_JOB_DIR")
        .env_remove("CLAUDE_PID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env("RECOMPACT_EVAL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot spawn {bin}: {e}"))?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(prompt.as_bytes()).map_err(|e| e.to_string())?;
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errors = std::thread::spawn(move || {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err);
        err
    });
    let reader = std::thread::spawn(move || {
        let mut action: Option<String> = None;
        let mut text = String::new();
        let mut usage = Value::Null;
        let mut tools: HashSet<String> = HashSet::new();
        let mut result: Option<String> = None;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            match v["type"].as_str() {
                Some("system") if v["subtype"] == json!("init") => {
                    tools.extend(v["tools"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).map(String::from));
                }
                Some("assistant") if action.is_none() => {
                    for b in v.pointer("/message/content").and_then(|c| c.as_array()).into_iter().flatten() {
                        match b["type"].as_str() {
                            Some("tool_use") if action.is_none() => {
                                action = Some(canonical(b["name"].as_str().unwrap_or(""), &b["input"]));
                            }
                            Some("text") => {
                                text.push_str(b["text"].as_str().unwrap_or(""));
                                text.push('\n');
                            }
                            _ => {}
                        }
                    }
                }
                Some("result") => {
                    // Claude Code's own total starts from the resumed session's past spend, so the
                    // price comes from this call's token counts instead.
                    usage = v["usage"].clone();
                    if v["is_error"] == json!(true) && action.is_none() && text.trim().is_empty() {
                        result = Some(v["result"].as_str().unwrap_or("error").to_string());
                    }
                }
                _ => {}
            }
        }
        (action, text, usage, tools, result)
    });
    let deadline = Instant::now() + CALL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let (action, text, usage, tools, error) = reader.join().map_err(|_| "reader panicked".to_string())?;
    let err = errors.join().unwrap_or_default();
    if status.is_none() {
        return Err(format!("claude did not finish within {} minutes", CALL_TIMEOUT.as_secs() / 60));
    }
    if let Some(e) = error {
        return Err(format!("claude reported an error: {}", e.chars().take(300).collect::<String>()));
    }
    if action.is_none() && text.trim().is_empty() {
        return Err(format!(
            "claude returned nothing ({}): {}",
            status.map(|s| s.to_string()).unwrap_or_default(),
            err.lines().last().unwrap_or("").chars().take(300).collect::<String>()
        ));
    }
    let cost = usage_cost(model, &usage);
    Ok(CallResult { action, text, cost, usage, tools })
}

/// List price in dollars of one call's usage. Per million tokens: input, output, cache read, and
/// cache writes with the one-hour and the five-minute lifetime.
pub fn usage_cost(model: &str, u: &Value) -> f64 {
    let m = model.to_lowercase();
    let (input, output, read) = if m.contains("haiku") {
        (0.10, 0.50, 0.01)
    } else if m.contains("sonnet") {
        (2.0, 10.0, 0.20)
    } else if m.contains("fable") {
        (10.0, 50.0, 1.0)
    } else {
        (4.0, 20.0, 0.20)
    };
    let n = |v: &Value| v.as_f64().unwrap_or(0.0);
    let created = n(&u["cache_creation_input_tokens"]);
    let hour = u.pointer("/cache_creation/ephemeral_1h_input_tokens").map_or(created, n);
    let five = u.pointer("/cache_creation/ephemeral_5m_input_tokens").map_or(0.0, n);
    (n(&u["input_tokens"]) * input
        + n(&u["output_tokens"]) * output
        + n(&u["cache_read_input_tokens"]) * read
        + hour * input * 2.0
        + five * input * 1.25)
        / 1e6
}

// ------------------------------------------------------------------------------------- report

#[derive(Default)]
struct Tally {
    n: usize,
    failed: usize,
    tokens: u64,
    kept: u64,
    total: u64,
    next_n: usize,
    same: f64,
    same_tool: f64,
    repeat: f64,
    truth_repeat: usize,
    recall: f64,
    unavailable: usize,
    files_truth: u64,
    files_hit: u64,
    files_said: u64,
    check_n: usize,
    check_ok: usize,
    cost: f64,
}

/// A per-point rate: a fraction of samples, or a boolean from single-sample rows.
fn share(v: &Value) -> f64 {
    v.as_f64().unwrap_or(if *v == json!(true) { 1.0 } else { 0.0 })
}

fn pctf(a: f64, b: usize) -> String {
    if b == 0 {
        "-".into()
    } else {
        format!("{:.0}%", 100.0 * a / b as f64)
    }
}

fn pct(a: usize, b: usize) -> String {
    if b == 0 {
        "-".into()
    } else {
        format!("{:.0}%", 100.0 * a as f64 / b as f64)
    }
}

fn pct64(a: u64, b: u64) -> String {
    pct(a as usize, b as usize)
}

fn eval_report(args: &[String]) -> i32 {
    let (pos, _) = parse_opts(args);
    let files: Vec<PathBuf> = if pos.is_empty() {
        let mut v: Vec<PathBuf> = fs::read_dir(eval_home().join("results"))
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
            .unwrap_or_default();
        v.sort();
        v
    } else {
        pos.iter().map(PathBuf::from).collect()
    };
    // arm -> point -> latest row
    let mut rows: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for f in &files {
        for line in fs::read_to_string(f).unwrap_or_default().lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let (Some(arm), Some(point)) = (v["arm"].as_str(), v["point"].as_str()) else { continue };
            let slot = rows.entry(arm.to_string()).or_default();
            // A later success replaces an earlier failure, never the other way round.
            if v["ok"] == json!(true) || !slot.contains_key(point) {
                slot.insert(point.to_string(), v);
            }
        }
    }
    if rows.is_empty() {
        eprintln!("eval: no results yet; run `recompact eval run --arm NAME`");
        return 1;
    }
    println!("{}", report_text(&rows));
    0
}

pub fn report_text(rows: &BTreeMap<String, BTreeMap<String, Value>>) -> String {
    let mut out = String::new();
    let mut tallies: Vec<(String, Tally)> = Vec::new();
    for (arm, points) in rows {
        let mut t = Tally::default();
        for v in points.values() {
            t.n += 1;
            if v["ok"] != json!(true) {
                t.failed += 1;
                continue;
            }
            t.tokens += v["context_tokens"].as_u64().unwrap_or(0);
            t.kept += v["details"]["kept"].as_u64().unwrap_or(0);
            t.total += v["details"]["total"].as_u64().unwrap_or(0);
            t.cost += v["cost_usd"].as_f64().unwrap_or(0.0);
            let n = &v["next"];
            if n["unavailable"] == json!(true) {
                t.unavailable += 1;
            } else if !n.is_null() {
                t.next_n += 1;
                t.same += share(&n["same"]);
                t.same_tool += share(&n["same_tool"]);
                t.repeat += share(&n["repeat"]);
                t.truth_repeat += usize::from(n["truth_repeat"] == json!(true));
                t.recall += share(&n["recall"]);
            }
            let p = &v["probe"];
            if !p.is_null() {
                t.files_truth += p["files_truth"].as_u64().unwrap_or(0);
                t.files_hit += p["files_hit"].as_u64().unwrap_or(0);
                t.files_said += p["files_said"].as_u64().unwrap_or(0);
                if let Some(truth) = p["check_truth"].as_str() {
                    t.check_n += 1;
                    t.check_ok += usize::from(p["check_said"].as_str() == Some(truth));
                }
            }
        }
        tallies.push((arm.clone(), t));
    }
    out.push_str(&format!(
        "{:<16} {:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}\n",
        "arm", "points", "size", "details", "next=", "tool=", "repeats", "files", "check", "failed", "cost"
    ));
    for (arm, t) in &tallies {
        let ok = t.n - t.failed;
        out.push_str(&format!(
            "{:<16} {:>6} {:>7}k {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}\n",
            arm,
            t.n,
            if ok == 0 { 0 } else { t.tokens / ok as u64 / 1000 },
            pct64(t.kept, t.total),
            pctf(t.same, t.next_n),
            pctf(t.same_tool, t.next_n),
            pctf(t.repeat, t.next_n),
            pct64(t.files_hit, t.files_truth),
            pct(t.check_ok, t.check_n),
            t.failed,
            format!("${:.2}", t.cost),
        ));
    }
    out.push_str(
        "\nsize: context on resume. details: details the next turns used that the context still shows.\n\
         next=: first action the same as the real one. tool=: same tool. repeats: first action repeats\n\
         an earlier call. files: changed files the model named. check: last check result right.\n",
    );
    let skipped: usize = tallies.iter().map(|(_, t)| t.unavailable).sum();
    if skipped > 0 {
        out.push_str(&format!(
            "{skipped} first action(s) used a tool this environment lacks and are left out of next= and tool=.\n"
        ));
    }

    // Paired differences against the first arm, on the points both scored.
    let arms: Vec<&String> = rows.keys().collect();
    if arms.len() >= 2 {
        let base = arms[0];
        out.push_str(&format!("\nPaired against {base} (difference, 95% interval, points):\n"));
        for other in &arms[1..] {
            for (label, f) in [
                ("next action same", metric_same as fn(&Value) -> Option<f64>),
                ("repeats a call", metric_repeat),
                ("details kept", metric_details),
                ("files named", metric_files),
            ] {
                let diffs: Vec<f64> = rows[base]
                    .iter()
                    .filter_map(|(pt, a)| {
                        let b = rows[*other].get(pt)?;
                        Some(f(b)? - f(a)?)
                    })
                    .collect();
                if diffs.is_empty() {
                    continue;
                }
                let (mean, half) = mean_ci(&diffs);
                out.push_str(&format!(
                    "  {other:<16} {label:<17} {:+.0} points ({:+.0} to {:+.0}), n={}\n",
                    100.0 * mean,
                    100.0 * (mean - half),
                    100.0 * (mean + half),
                    diffs.len()
                ));
            }
        }
    }
    out
}

fn ok_row(v: &Value) -> Option<&Value> {
    (v["ok"] == json!(true)).then_some(v)
}

fn metric_same(v: &Value) -> Option<f64> {
    let n = &ok_row(v)?["next"];
    (!n.is_null() && n["unavailable"] != json!(true)).then(|| share(&n["same"]))
}

fn metric_repeat(v: &Value) -> Option<f64> {
    let n = &ok_row(v)?["next"];
    (!n.is_null() && n["unavailable"] != json!(true)).then(|| share(&n["repeat"]))
}

fn metric_details(v: &Value) -> Option<f64> {
    let d = &ok_row(v)?["details"];
    let total = d["total"].as_f64()?;
    (total > 0.0).then(|| d["kept"].as_f64().unwrap_or(0.0) / total)
}

fn metric_files(v: &Value) -> Option<f64> {
    let p = &ok_row(v)?["probe"];
    let truth = p["files_truth"].as_f64()?;
    (truth > 0.0).then(|| p["files_hit"].as_f64().unwrap_or(0.0) / truth)
}

/// Mean and the half-width of its 95% interval (normal approximation).
pub fn mean_ci(xs: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    if xs.len() < 2 {
        return (mean, f64::NAN);
    }
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    (mean, 1.96 * (var / n).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_keys_compare_what_an_action_touches() {
        assert_eq!(canonical("Read", &json!({"file_path": "/a/b/c.rs"})), "Read /a/b/c.rs");
        assert_eq!(
            canonical("Bash", &json!({"command": "cd /repo && FOO=1 cargo test --release -q"})),
            "Bash cargo test"
        );
        assert_eq!(canonical("Bash", &json!({"command": "git commit -m \"x y\""})), "Bash git commit");
        assert_eq!(
            canonical("Bash", &json!({"command": "slack-read https://x.slack.com/archives/C1/p2?a=1&b=2 2>&1"})),
            "Bash slack-read"
        );
        assert_eq!(canonical("Task", &json!({"subagent_type": "Explore"})), "Agent Explore");
        assert_eq!(canonical("WebFetch", &json!({"url": "https://arxiv.org/abs/1"})), "WebFetch arxiv.org");
        assert_eq!(canonical("mcp__x__y", &json!({})), "mcp__x__y");
    }

    #[test]
    fn path_keys_ignore_how_a_path_was_spelled() {
        assert_eq!(path_key("/Users/x/repo/src/lib.rs"), "src/lib.rs");
        assert_eq!(path_key("src/lib.rs"), "src/lib.rs");
        assert_eq!(path_key("SRC/Lib.rs"), "src/lib.rs");
    }

    #[test]
    fn cost_comes_from_this_calls_tokens() {
        let u = json!({"input_tokens": 2, "output_tokens": 1000, "cache_read_input_tokens": 100_000,
            "cache_creation_input_tokens": 200_000,
            "cache_creation": {"ephemeral_1h_input_tokens": 200_000, "ephemeral_5m_input_tokens": 0}});
        let c = usage_cost("opus", &u);
        assert!((c - (0.000008 + 0.02 + 0.02 + 1.6)).abs() < 1e-6, "{c}");
    }

    #[test]
    fn mean_ci_of_identical_values_is_exact() {
        let (m, h) = mean_ci(&[0.5, 0.5, 0.5]);
        assert_eq!(m, 0.5);
        assert_eq!(h, 0.0);
    }
}
