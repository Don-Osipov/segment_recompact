//! Orientation: what a session resumed into a twin needs in its first minute.
//!
//! In a month of real use (56 resumed twins), users opened a third of them with "where did we
//! leave off / reorient yourself", and the model then spent dozens of tool calls rebuilding the
//! state of branches, PRs and files from live systems. None of it used `recall`. The brief below
//! is the part of that rebuild the transcript already knows — derived mechanically from the raw
//! records, so it cannot hallucinate — and the SessionStart hook adds the part only the present
//! knows: how long ago the compaction was, and what changed in the repo since.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

use crate::{
    content, human_queued_text, is_genuine_user, is_human_queued, rec_type, truncate, user_text,
};

#[derive(Default)]
struct FileTouch {
    roles: Vec<&'static str>,
    count: usize,
    order: usize,
}

/// Last observed state per PR number, with the order it was observed in.
#[derive(Default)]
struct PrState {
    state: String,
    order: usize,
}

fn tool_result_texts(r: &Value) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
        for b in blocks {
            if b.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                continue;
            }
            let id = b
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let err = b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            let text = match b.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            out.push((id, text, err));
        }
    }
    out
}

fn pr_number_after(cmd: &str, sub: &str) -> Option<String> {
    let i = cmd.find(sub)?;
    cmd[i + sub.len()..]
        .split_whitespace()
        .find(|t| !t.starts_with('-'))
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_digit()))
        .filter(|t| !t.is_empty() && t.len() <= 7 && t.chars().all(|c| c.is_ascii_digit()))
        .map(|t| t.to_string())
}

fn pr_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("/pull/") {
        let n: String = rest[i + 6..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !n.is_empty() && rest[..i].contains("github.com/") {
            out.push(n);
        }
        rest = &rest[i + 6..];
    }
    out
}

/// A `git commit` success line: "[branch abc1234] message".
fn commit_lines(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        let Some(inner) = l.strip_prefix('[') else {
            continue;
        };
        let Some((head, msg)) = inner.split_once("] ") else {
            continue;
        };
        let mut parts = head.split_whitespace();
        let (Some(_branch), Some(sha)) = (parts.next(), parts.last()) else {
            continue;
        };
        if (7..=12).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit()) {
            out.push((sha.to_string(), truncate(msg.trim(), 60)));
        }
    }
    out
}

fn branch_created(cmd: &str) -> Option<String> {
    for flag in ["checkout -b ", "switch -c ", "checkout -B ", " -b "] {
        if let Some(i) = cmd.find(flag) {
            if flag == " -b " && !cmd.contains("worktree add") {
                continue;
            }
            let name = cmd[i + flag.len()..]
                .split_whitespace()
                .next()?
                .trim_matches(|c: char| c == '"' || c == '\'' || c == ';' || c == '&');
            if !name.is_empty() && !name.starts_with('-') {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Mechanical state brief of a session's raw active path. Empty lines are omitted.
pub fn state_brief(records: &[Value]) -> Vec<String> {
    let mut files: BTreeMap<String, FileTouch> = BTreeMap::new();
    let mut prs: BTreeMap<String, PrState> = BTreeMap::new();
    let mut commits: Vec<(String, String)> = Vec::new();
    let mut branches: Vec<String> = Vec::new();
    let mut pending_cmd: HashMap<String, String> = HashMap::new();
    let mut order = 0usize;
    for r in records {
        order += 1;
        if rec_type(r) == "assistant" {
            if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
                for b in blocks {
                    if b.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                        continue;
                    }
                    let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let input = b.get("input").cloned().unwrap_or(Value::Null);
                    let id = b
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let path = input
                        .get("file_path")
                        .or_else(|| input.get("notebook_path"))
                        .and_then(|v| v.as_str());
                    let role = match name {
                        "Edit" | "MultiEdit" | "NotebookEdit" => Some("edited"),
                        "Write" => Some("written"),
                        _ => None,
                    };
                    if let (Some(p), Some(role)) = (path, role) {
                        let f = files.entry(p.to_string()).or_default();
                        if !f.roles.contains(&role) {
                            f.roles.push(role);
                        }
                        f.count += 1;
                        f.order = order;
                    }
                    if name == "EnterWorktree" {
                        if let Some(n) = input.get("name").and_then(|v| v.as_str()) {
                            branches.push(format!("worktree {n}"));
                        }
                    }
                    if name == "Bash" {
                        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                            if let Some(b) = branch_created(cmd) {
                                branches.push(b);
                            }
                            pending_cmd.insert(id, cmd.to_string());
                        }
                    }
                }
            }
        }
        for (id, text, err) in tool_result_texts(r) {
            let cmd = pending_cmd.remove(&id).unwrap_or_default();
            if !err {
                commits.extend(commit_lines(&text));
            }
            if cmd.contains("gh pr create") && !err {
                for n in pr_urls(&text) {
                    prs.insert(
                        n,
                        PrState {
                            state: "opened".into(),
                            order,
                        },
                    );
                }
            }
            if let Some(n) = pr_number_after(&cmd, "gh pr merge") {
                if !err {
                    let state = if text.contains("auto-merge") || cmd.contains("--auto") {
                        "auto-merge set"
                    } else {
                        "merged"
                    };
                    prs.insert(
                        n,
                        PrState {
                            state: state.into(),
                            order,
                        },
                    );
                }
            }
            for sub in ["gh pr view", "gh pr checks", "gh pr status"] {
                if let Some(n) = pr_number_after(&cmd, sub) {
                    let state = if text.contains("MERGED") || text.contains("\"merged\"") {
                        Some("merged")
                    } else if text.contains("CLOSED") {
                        Some("closed")
                    } else if text.contains("OPEN") {
                        Some("open")
                    } else {
                        None
                    };
                    if let (Some(s), false) = (state, err) {
                        prs.insert(
                            n,
                            PrState {
                                state: s.into(),
                                order,
                            },
                        );
                    }
                }
            }
        }
    }

    let mut lines = Vec::new();
    if !files.is_empty() {
        let mut v: Vec<(&String, &FileTouch)> = files.iter().collect();
        v.sort_by_key(|(_, f)| std::cmp::Reverse(f.order));
        let root = common_dir(files.keys().map(String::as_str));
        let shown: Vec<String> = v
            .iter()
            .take(12)
            .map(|(p, f)| {
                let rel = root
                    .as_deref()
                    .and_then(|r| p.strip_prefix(r))
                    .map(|x| x.trim_start_matches('/'))
                    .filter(|x| !x.is_empty())
                    .unwrap_or(p);
                let role = f.roles.join("+");
                if f.count > 1 {
                    format!("`{rel}` ({role} ×{})", f.count)
                } else {
                    format!("`{rel}` ({role})")
                }
            })
            .collect();
        let more = v.len().saturating_sub(12);
        lines.push(format!(
            "Files changed this session, newest first{}: {}{}",
            root.as_deref()
                .map(|r| format!(" (under `{r}`)"))
                .unwrap_or_default(),
            shown.join(", "),
            if more > 0 {
                format!(", +{more} more")
            } else {
                String::new()
            }
        ));
    }
    let mut git_bits = Vec::new();
    if !commits.is_empty() {
        let recent: Vec<String> = commits
            .iter()
            .rev()
            .take(6)
            .map(|(sha, msg)| format!("{sha} \"{msg}\""))
            .collect();
        git_bits.push(format!("commits (newest first) {}", recent.join(", ")));
    }
    branches.dedup();
    if !branches.is_empty() {
        let recent: Vec<String> = branches
            .iter()
            .rev()
            .take(6)
            .map(|b| format!("`{b}`"))
            .collect();
        git_bits.push(format!("branches/worktrees created {}", recent.join(", ")));
    }
    if !git_bits.is_empty() {
        lines.push(format!("Git: {}", git_bits.join("; ")));
    }
    if !prs.is_empty() {
        let mut v: Vec<(&String, &PrState)> = prs.iter().collect();
        v.sort_by_key(|(_, p)| std::cmp::Reverse(p.order));
        let shown: Vec<String> = v
            .iter()
            .take(10)
            .map(|(n, s)| format!("#{n} {}", s.state))
            .collect();
        lines.push(format!(
            "PRs, last state observed in this session: {}",
            shown.join(" · ")
        ));
    }
    let asks: Vec<String> = records
        .iter()
        .filter(|r| is_genuine_user(r) || is_human_queued(r))
        .map(|r| {
            if is_human_queued(r) {
                human_queued_text(r)
            } else {
                user_text(r)
            }
        })
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| {
            !t.is_empty() && !t.starts_with("<command-name>") && !t.starts_with("<local-command")
        })
        .collect();
    if !asks.is_empty() {
        let recent: Vec<String> = asks
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|t| format!("\"{}\"", truncate(t, 220)))
            .collect();
        lines.push(format!(
            "Most recent asks, oldest first: {}",
            recent.join(" → ")
        ));
    }
    lines
}

/// The directory most of a session's changed files share — where relative paths in the brief
/// and in carried lines are rooted.
pub fn session_path_root(records: &[Value]) -> Option<String> {
    let mut paths: Vec<String> = Vec::new();
    for r in records {
        if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                    && matches!(
                        b.get("name").and_then(|v| v.as_str()),
                        Some("Edit" | "MultiEdit" | "Write" | "NotebookEdit")
                    )
                {
                    if let Some(p) = b
                        .pointer("/input/file_path")
                        .or_else(|| b.pointer("/input/notebook_path"))
                        .and_then(|v| v.as_str())
                    {
                        if !paths.iter().any(|x| x == p) {
                            paths.push(p.to_string());
                        }
                    }
                }
            }
        }
    }
    common_dir(paths.iter().map(String::as_str))
}

/// Longest directory prefix shared by most of the paths (every path but at most one outlier),
/// when it saves real space.
fn common_dir<'a>(paths: impl Iterator<Item = &'a str>) -> Option<String> {
    let paths: Vec<&str> = paths.filter(|p| p.starts_with('/')).collect();
    if paths.len() < 2 {
        return None;
    }
    let dirs: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| {
            let mut parts: Vec<&str> = p.split('/').collect();
            parts.pop();
            parts
        })
        .collect();
    let mut best: Option<String> = None;
    let longest = dirs.iter().map(Vec::len).max().unwrap_or(0);
    for depth in 2..=longest {
        let mut counts: HashMap<Vec<&str>, usize> = HashMap::new();
        for d in &dirs {
            if d.len() >= depth {
                *counts.entry(d[..depth].to_vec()).or_default() += 1;
            }
        }
        match counts.into_iter().max_by_key(|(_, n)| *n) {
            Some((prefix, n)) if n + 1 >= paths.len() => best = Some(prefix.join("/")),
            _ => break,
        }
    }
    best.filter(|b| b.len() >= 12)
}

fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Live repo state for `cwd`, when it is a git checkout: branch, short HEAD, uncommitted count.
pub fn git_snapshot(cwd: &str) -> Option<Value> {
    if cwd.is_empty() || !Path::new(cwd).is_dir() {
        return None;
    }
    let head = git(cwd, &["rev-parse", "--short", "HEAD"])?;
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let dirty = git(cwd, &["status", "--porcelain"])
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    Some(json!({"cwd": cwd, "branch": branch, "head": head, "dirty": dirty}))
}

pub fn describe_snapshot(s: &Value) -> String {
    format!(
        "`{}` on branch `{}` at {}, {} uncommitted file(s)",
        s["cwd"].as_str().unwrap_or("?"),
        s["branch"].as_str().unwrap_or("?"),
        s["head"].as_str().unwrap_or("?"),
        s["dirty"].as_u64().unwrap_or(0)
    )
}

/// Model and effort to resume with. A resume starts on the saved default model, and effort set
/// with "(this session only)" does not survive it: in the sample, 9 of 56 resumed twins opened
/// with the user re-running `/model` or `/effort` by hand.
pub fn resume_flags(records: &[Value]) -> Vec<String> {
    let mut flags = Vec::new();
    let model = records
        .iter()
        .rev()
        .filter(|r| rec_type(r) == "assistant" && !crate::truthy(r, "recompactSynthetic"))
        .find_map(|r| r.pointer("/message/model").and_then(|v| v.as_str()))
        .filter(|m| m.starts_with("claude-"));
    let mut long_context = false;
    let mut effort: Option<String> = None;
    for r in records {
        if let Some(u) = r.pointer("/message/usage") {
            let get = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            if get("input_tokens")
                + get("cache_creation_input_tokens")
                + get("cache_read_input_tokens")
                > 200_000
            {
                long_context = true;
            }
        }
        if rec_type(r) == "user" {
            let t = user_text(r);
            if let Some(i) = t.find("Set effort level to ") {
                let e: String = t[i + 20..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if !e.is_empty() {
                    effort = Some(e);
                }
            }
            if t.contains("Set model to") && t.contains("1M context") {
                long_context = true;
            }
        }
        if let Some(m) = r.pointer("/attachment/model").and_then(|v| v.as_str()) {
            if m.ends_with("[1m]") {
                long_context = true;
            }
        }
    }
    if let Some(m) = model {
        flags.push("--model".into());
        flags.push(if long_context {
            format!("{m}[1m]")
        } else {
            m.to_string()
        });
    }
    if let Some(e) = effort {
        flags.push("--effort".into());
        flags.push(e);
    }
    flags
}

/// Shell-safe rendering of a resume command.
pub fn resume_command(id: &str, flags: &[String]) -> String {
    let mut s = format!("claude --resume {id}");
    for f in flags {
        if f.contains('[') || f.contains(' ') {
            s.push_str(&format!(" '{f}'"));
        } else {
            s.push_str(&format!(" {f}"));
        }
    }
    s
}

fn human_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 90 {
        format!("{s}s")
    } else if s < 90 * 60 {
        format!("{}m", s / 60)
    } else if s < 48 * 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d", s / 86400)
    }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm): parses the transcript's
/// RFC 3339 timestamps without a date crate.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn parse_ts(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Inverse of days_from_civil.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// The newest twin in `transcript`'s project dir whose custom title is `title`.
fn twin_by_title(transcript: &str, title: &str) -> Option<String> {
    if !title.contains("(recompact") {
        return None;
    }
    let dir = Path::new(transcript).parent()?;
    let needle = serde_json::to_string(&json!({ "customTitle": title })).ok()?;
    let needle = needle.trim_start_matches('{').trim_end_matches('}');
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(t) = e.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_some_and(|(bt, _)| *bt >= t) {
            continue;
        }
        if let Ok(raw) = std::fs::read_to_string(&p) {
            if raw.contains(needle) && raw.contains("\"recompactPreamble\"") {
                best = Some((t, raw));
            }
        }
    }
    best.map(|(_, raw)| raw)
}

/// SessionStart hook body. Silent (no output) unless the session being resumed is a twin.
pub fn session_start_context(input: &Value) -> Option<String> {
    let source = input.get("source").and_then(|v| v.as_str()).unwrap_or("");
    // A fork of a twin (`--fork-session`, `/branch`) wakes up inside the same compacted history.
    if source != "resume" && source != "fork" {
        return None;
    }
    let path = input.get("transcript_path").and_then(|v| v.as_str())?;
    let raw = match std::fs::read_to_string(path) {
        Ok(r) if r.contains("\"recompactPreamble\"") => r,
        // A fork's transcript is not written until its first turn, after this hook runs; the
        // session title (a twin's carries "(recompact N)") names the twin it was forked from.
        _ => twin_by_title(path, input.get("session_title").and_then(|v| v.as_str())?)?,
    };
    let records: Vec<Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect();
    let pidx = records
        .iter()
        .rposition(|r| crate::truthy(r, "recompactPreamble"))?;
    let preamble = &records[pidx];
    let assembled_at = preamble
        .get("recompactAssembledAt")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            preamble
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(parse_ts)
        });
    let since: Vec<&Value> = records[pidx + 1..]
        .iter()
        .filter(|r| is_genuine_user(r))
        .collect();
    let generation = preamble
        .get("recompactGeneration")
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    let source_id = preamble
        .pointer("/recompactSource/sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let idle = input
        .get("seconds_since_last_response")
        .and_then(|v| v.as_i64())
        .filter(|s| *s > 3600)
        .map(|s| format!(" The work itself last moved {} ago.", human_age(s)))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "{} You are resuming a transcript that segment_recompact compacted{}{} (generation {generation}){}.{idle}",
        crate::ORIENT_MARKER,
        assembled_at
            .map(|t| format!(" {} ago", human_age(now_unix() - t)))
            .unwrap_or_default(),
        if source_id.is_empty() { String::new() } else { format!(" from session {}", short_id(source_id)) },
        if since.is_empty() {
            String::new()
        } else {
            format!("; {} user turn(s) have happened since", since.len())
        }
    )];

    // What changed in the repo since the snapshot the assembly took.
    if let Some(snap) = preamble.get("recompactSnapshot") {
        let cwd = snap.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
        match git_snapshot(cwd) {
            Some(now) => {
                let (h0, h1) = (snap["head"].as_str().unwrap_or(""), now["head"].as_str().unwrap_or(""));
                let (b0, b1) = (snap["branch"].as_str().unwrap_or(""), now["branch"].as_str().unwrap_or(""));
                let mut delta = Vec::new();
                if b0 != b1 {
                    delta.push(format!("branch changed `{b0}` → `{b1}`"));
                }
                if h0 != h1 {
                    let log = git(cwd, &["log", "--oneline", "--no-decorate", "-5", &format!("{h0}..{h1}")])
                        .filter(|s| !s.is_empty())
                        .map(|s| format!(": {}", s.lines().map(|l| truncate(l, 70)).collect::<Vec<_>>().join(" | ")))
                        .unwrap_or_default();
                    delta.push(format!("HEAD moved {h0} → {h1}{log}"));
                }
                let (d0, d1) = (snap["dirty"].as_u64().unwrap_or(0), now["dirty"].as_u64().unwrap_or(0));
                if d0 != d1 {
                    delta.push(format!("uncommitted files {d0} → {d1}"));
                }
                if delta.is_empty() {
                    lines.push(format!("Repo `{cwd}` is unchanged since compaction (branch `{b1}` at {h1})."));
                } else {
                    lines.push(format!("Since compaction, in `{cwd}`: {}.", delta.join("; ")));
                }
            }
            None if !cwd.is_empty() => lines.push(format!("The compaction-time working directory `{cwd}` is no longer a readable git checkout.")),
            None => {}
        }
    }
    if since.is_empty() {
        lines.push(
            "The final assistant message is the orientation note: read its \"State when compacted\" list before acting. \
             Summaries and that list are a snapshot: re-check anything external (PRs, deploys, database rows, running jobs) \
             before relying on it. For an exact detail a summary dropped, call the recall tool (query= to search, selector= for a marker) \
             instead of re-running the work."
                .to_string(),
        );
        let last_pre = records[..pidx]
            .iter()
            .rev()
            .find(|r| is_genuine_user(r))
            .map(user_text)
            .unwrap_or_default();
        if last_pre.contains("recompact") {
            lines.push(
                "The /recompact request near the end of this transcript is the one that produced this session; it has already run, so do not run it again."
                    .to_string(),
            );
        }
    }
    Some(lines.join("\n"))
}

pub fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Openings that make a sentence an instruction (imperative mood), after filler is stripped.
const CONSTRAINT_OPENINGS: &[&str] = &[
    "don't",
    "dont ",
    "do not",
    "never",
    "always",
    "make sure",
    "be sure",
    "stop ",
    "avoid",
    "keep ",
    "lets not",
    "let's not",
    "no need to",
    "only use",
    "only do",
    "only run",
    "use ",
    "remember",
    "from now on",
    "going forward",
    "you must",
    "you should",
    "you need to",
    "i don't want",
    "i dont want",
    "we don't",
    "we dont",
    "no more",
];

/// Phrases that bind wherever they appear.
const CONSTRAINT_ANYWHERE: &[&str] = &[
    "from now on",
    "going forward",
    "without asking",
    "without my",
    "no need to",
    "not yet",
    "should never",
    "must not",
    "do not ",
    "don't ever",
    "dont ever",
    "never ever",
];

const FILLER: &[&str] = &[
    "ok ",
    "okay ",
    "ok, ",
    "so ",
    "and ",
    "also ",
    "but ",
    "please ",
    "yes ",
    "yeah ",
    "alright ",
    "no ",
    "no, ",
    "and also ",
    "also, ",
    "then ",
    "actually ",
    "btw ",
    "oh ",
];

fn is_instruction(sentence: &str) -> bool {
    let mut lower = sentence.trim().to_lowercase();
    loop {
        let before = lower.len();
        for f in FILLER {
            if let Some(rest) = lower.strip_prefix(f) {
                lower = rest.trim_start().to_string();
            }
        }
        if lower.len() == before {
            break;
        }
    }
    CONSTRAINT_OPENINGS.iter().any(|o| lower.starts_with(o))
        || CONSTRAINT_ANYWHERE.iter().any(|c| lower.contains(c))
}

/// Sentences with the character that ended them, so questions can be told from instructions.
fn sentences(text: &str) -> Vec<(String, char)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if matches!(c, '\n' | '.' | '!' | '?') {
            let t = cur.split_whitespace().collect::<Vec<_>>().join(" ");
            if !t.is_empty() {
                out.push((t, c));
            }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    let t = cur.split_whitespace().collect::<Vec<_>>().join(" ");
    if !t.is_empty() {
        out.push((t, '\n'));
    }
    out
}

fn strip_tagged(text: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(&open) {
        out.push_str(&rest[..i]);
        match rest[i..].find(&close) {
            Some(j) => rest = &rest[i + j + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The user's own words that bind future work, verbatim and in order. Constraint retention is the
/// best-measured failure of compaction (one round of summarization took violations from 0% to
/// 30%, four rounds to 78%; pinning the constraints restored 0%). Human turns already survive here
/// verbatim, but scattered through a long transcript; this lane restates the normative ones at the
/// end, where attention is strongest, without paraphrasing a word of them.
pub fn constraint_lane(records: &[Value]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for r in records {
        let raw = if is_human_queued(r) {
            human_queued_text(r)
        } else if is_genuine_user(r) {
            user_text(r)
        } else {
            continue;
        };
        if raw.starts_with("<command-name>")
            || raw.starts_with("<local-command")
            || raw.starts_with("[Request interrupted")
        {
            continue;
        }
        let mut text = raw;
        for tag in [
            "pasted_content",
            "local-command-stdout",
            "bash-stdout",
            "bash-stderr",
            "command-message",
            "command-name",
            "command-args",
            "system-reminder",
        ] {
            text = strip_tagged(&text, tag);
        }
        let text: String = text.chars().take(6000).collect();
        for (sentence, end) in sentences(&text) {
            if end == '?' || !(12..=400).contains(&sentence.len()) || !is_instruction(&sentence) {
                continue;
            }
            let q = format!("\"{}\"", truncate(&sentence, 200));
            found.retain(|f| f != &q);
            found.push(q);
        }
    }
    // Newest win the space: keep the most recent, up to ~1.8k chars, in chronological order.
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    for q in found.into_iter().rev() {
        if kept.len() >= 14 || used + q.len() > 1800 {
            break;
        }
        used += q.len();
        kept.push(q);
    }
    kept.reverse();
    kept
}

const CHECK_PATTERNS: &[&str] = &[
    "cargo test",
    "cargo build",
    "cargo check",
    "cargo clippy",
    "npm test",
    "npm run test",
    "npm run build",
    "npm run lint",
    "npm run typecheck",
    "pnpm test",
    "pnpm run test",
    "pnpm build",
    "pnpm run build",
    "pnpm lint",
    "pnpm run lint",
    "pnpm typecheck",
    "pnpm run typecheck",
    "pnpm tsc",
    "npx tsc",
    "yarn test",
    "vitest",
    "jest",
    "pytest",
    "go test",
    "tsc --noEmit",
    "eslint",
    "make test",
    "make check",
    "ruff",
    "mypy",
    "swift test",
    "xcodebuild test",
    "bun test",
];

/// The most recent validation command and how it ended — the one line a successor needs to know
/// whether the tree was green, and the one that handoff notes most often dropped.
pub fn last_check(records: &[Value]) -> Option<String> {
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut last: Option<String> = None;
    for r in records {
        if rec_type(r) == "assistant" {
            if let Some(blocks) = content(r).and_then(|c| c.as_array()) {
                for b in blocks {
                    if b.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                        && b.get("name").and_then(|v| v.as_str()) == Some("Bash")
                    {
                        let cmd = b
                            .pointer("/input/command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if CHECK_PATTERNS.iter().any(|p| cmd.contains(p)) {
                            let id = b
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            pending.insert(id, cmd.to_string());
                        }
                    }
                }
            }
        }
        for (id, text, err) in tool_result_texts(r) {
            if let Some(cmd) = pending.remove(&id) {
                let clean = strip_ansi(&text);
                let tail_lines: Vec<&str> = clean
                    .lines()
                    .rev()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .take(15)
                    .collect();
                let tail = tail_lines
                    .first()
                    .map(|l| truncate(l, 140))
                    .unwrap_or_default();
                // A pipe (`| tail`) swallows the exit code; the output still says what happened.
                let reported_failure = tail_lines.iter().any(|l| {
                    let lo = l.to_ascii_lowercase();
                    l.starts_with("FAIL")
                        || lo.contains(" failed")
                        || lo.starts_with("error")
                        || lo.contains("error:")
                        || l.contains('✗')
                        || l.contains('×')
                        || lo.contains("panicked")
                });
                let status = if err {
                    "FAILED (non-zero exit)"
                } else if reported_failure {
                    "exit 0, but the output reports failures"
                } else {
                    "exit 0"
                };
                let one_line = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
                last = Some(format!(
                    "`{}` → {status}{}",
                    truncate(&one_line, 110),
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!("; last line: \"{tail}\"")
                    }
                ));
            }
        }
    }
    last
}

/// Terminal color codes, which test runners emit even into pipes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

pub struct PreambleInput<'a> {
    pub path_root: Option<&'a str>,
    pub new_session: &'a str,
    pub source_session: &'a str,
    pub generation: u64,
    pub assembled_at: i64,
    pub snapshot: Option<&'a Value>,
    pub brief: Vec<String>,
    pub constraints: Vec<String>,
    pub last_check: Option<String>,
}

/// The orientation note that closes every twin. Reading rules first (they never change), then
/// the state brief, then the user's standing instructions — the last thing before the resumed
/// model's own turn.
pub fn preamble_text(p: &PreambleInput) -> String {
    let when = format_utc(p.assembled_at);
    let generation = p.generation;
    let from = if p.source_session.is_empty() {
        String::new()
    } else {
        format!(", from session {}", p.source_session)
    };
    let rooted = p
        .path_root
        .map(|r| format!(" (relative paths are under `{r}`)"))
        .unwrap_or_default();
    let mut s = format!(
        "This transcript was compacted by segment_recompact on {when} (generation {generation}{from}). You may be a fresh session resumed into it.\n\n\
         How to read it:\n\
         - Past-tense assistant recaps are SUMMARIES of real turns; each ends \"[recompact summary <key> · recall <id>]\". \
         Lines beginning \"⟨carried⟩\" beneath a summary were copied mechanically from the original: files it changed, its errors verbatim, and identifiers later turns rely on{rooted}.\n\
         - Markers like \"[recompact: elided …; rehydrate <id>]\" stand for removed payloads (bulky tool output, old screenshots). User turns are verbatim.\n\
         - Nothing is lost. The `recall` tool reads originals back: recall(query=\"words\") searches everything compaction removed; \
         recall(selector=\"<id>\") returns one item verbatim. Use it before re-running a search or guessing at a detail. \
         Without the tool, in a shell: `recompact recall --query \"words\"` or `recompact recall <id>`.\n"
    );
    s.push_str(
        "\nState when compacted (a snapshot; re-check anything external, such as PRs, deploys, database rows, or running jobs, before relying on it):\n",
    );
    if let Some(snap) = p.snapshot {
        s.push_str(&format!(
            "- Working directory: {}\n",
            describe_snapshot(snap)
        ));
    }
    for line in &p.brief {
        s.push_str(&format!("- {line}\n"));
    }
    if let Some(c) = &p.last_check {
        s.push_str(&format!("- Last check run: {c}\n"));
    }
    if !p.constraints.is_empty() {
        s.push_str(
            "\nStanding instructions from the user, verbatim, oldest first (a later one overrides an earlier one):\n",
        );
        for c in &p.constraints {
            s.push_str(&format!("- {c}\n"));
        }
    }
    s.push_str(&format!(
        "\nThis session: {}. If it grows large again: `recompact continue {} --summarize-with haiku`.",
        p.new_session, p.new_session
    ));
    s
}
