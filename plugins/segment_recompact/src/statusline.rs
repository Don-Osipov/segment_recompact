//! Compaction progress in claude's status line.
//!
//! `recompact statusline [-- '<command>']` is a status line command. It runs the user's own
//! status line command (if any) on the same input and prints what it prints. While the launcher
//! compacts in place, it adds one row: a 0-100% bar for the compaction, then the result for a few
//! seconds. Claude Code re-runs it every second for that (`refreshInterval: 1`), so the user's
//! command is re-run only when Claude Code's input to it changes; a timer tick reuses its output.

use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::now_unix;

const ACCENT: &str = "\x1b[38;2;215;119;87m";
const DIM: &str = "\x1b[38;5;246m";
const TRACK: &str = "\x1b[38;5;238m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

/// How long the result stays in the status line after a switch.
const SHOW_DONE_SECS: i64 = 10;

fn fmt_k(t: u64) -> String {
    if t >= 10_000 {
        format!("{}k", (t + 500) / 1000)
    } else {
        t.to_string()
    }
}

/// The progress row for `p` (the launcher's `progress.json`) at `now`, `cols` wide; `None` when
/// there is nothing to show.
pub fn render_progress(p: &Value, now: i64, cols: usize) -> Option<String> {
    let phase = p.get("phase")?.as_str()?;
    let at = p.get("at").and_then(Value::as_i64).unwrap_or(now);
    let num = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (live, est) = (num("live"), num("est"));
    let sizes = match (live, est) {
        (0, _) => String::new(),
        (l, 0) => format!(" · {}", fmt_k(l)),
        (l, e) => format!(" · {} → ~{}", fmt_k(l), fmt_k(e)),
    };
    match phase {
        "done" => (now - at <= SHOW_DONE_SECS)
            .then(|| format!("{GREEN}✓{RESET} {DIM}recompact · compacted in place{sizes}{RESET}")),
        "noop" => (now - at <= SHOW_DONE_SECS)
            .then(|| format!("{DIM}recompact · nothing to compact{RESET}")),
        _ => {
            let (done, total) = (num("done"), num("total"));
            let what = match phase {
                "reading" => "reading the session".to_string(),
                "summarizing" if total > 0 => format!("summarizing {done} of {total}"),
                "summarizing" => "summarizing".into(),
                "assembling" => "building the compacted copy".into(),
                "verifying" => "checking it".into(),
                "waiting" => match p.get("wait").and_then(Value::as_str) {
                    Some("turn") => "ready · switches when this turn ends".into(),
                    Some("typing") => "ready · switches once the input box is empty".into(),
                    Some("answer") => "ready · switches after you answer claude".into(),
                    _ => "ready · switching at the next pause".into(),
                },
                "switching" => "switching".into(),
                _ => return None,
            };
            let mut pct = num("pct").min(100) as usize;
            // While a wave of summaries runs, ease toward where it lands (never reaching it): the
            // real count only moves when a batch comes back.
            let next = num("next");
            if phase == "summarizing" && next > done && total > 0 {
                let from = crate::progress_pct(phase, done as usize, total as usize);
                let to = crate::progress_pct(phase, next as usize, total as usize);
                let since = p.get("step_at").and_then(Value::as_i64).unwrap_or(at);
                let eased = 1.0 - (-((now - since).max(0) as f64) / 8.0).exp();
                pct = pct.max(from + ((to - from) as f64 * 0.9 * eased) as usize);
            }
            let width = cols.saturating_sub(70).clamp(10, 30);
            let filled = width * pct / 100;
            let spin = ["◐", "◓", "◑", "◒"][now.rem_euclid(4) as usize];
            let secs = p
                .get("started")
                .and_then(Value::as_i64)
                .map(|s| format!(" · {}s", (now - s).max(0)))
                .unwrap_or_default();
            Some(format!(
                "{ACCENT}{spin}{RESET} {DIM}recompact{RESET} {ACCENT}{}{TRACK}{}{RESET} {pct:>3}% {DIM}{what}{sizes}{secs}{RESET}",
                "━".repeat(filled),
                "─".repeat(width - filled),
            ))
        }
    }
}

/// Claude Code's input, without what changes on every timer tick.
fn input_key(input: &str, command: &str) -> String {
    let normalized = match serde_json::from_str::<Value>(input) {
        Ok(mut v) => {
            if let Some(cost) = v.get_mut("cost").and_then(Value::as_object_mut) {
                cost.remove("total_duration_ms");
            }
            v.to_string()
        }
        Err(_) => input.to_string(),
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (normalized, command).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The user's status line command's output for `input`: its last output when the input is the
/// same and recent, else a fresh run.
fn inner_output(command: &str, input: &str, cache_dir: &Path) -> String {
    let session = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| {
            v.get("session_id")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or_else(|| "none".into());
    let cache = cache_dir.join(format!("{session}.json"));
    let key = input_key(input, command);
    let now = now_unix();
    if let Some(c) = fs::read_to_string(&cache)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        let fresh = c
            .get("at")
            .and_then(Value::as_i64)
            .is_some_and(|at| now - at < 30);
        if fresh && c.get("key").and_then(Value::as_str) == Some(key.as_str()) {
            return c
                .get("out")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
    }
    let Ok(mut child) = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let _ = fs::create_dir_all(cache_dir);
    let tmp = cache.with_extension(format!("tmp{}", std::process::id()));
    if fs::write(&tmp, json!({"key": key, "out": out, "at": now}).to_string()).is_ok() {
        let _ = fs::rename(&tmp, &cache);
    }
    out
}

/// What the status line prints: the user's command's output for `input` (cached in
/// `cache_dir`), then the progress row when there is one.
pub fn status_line(
    command: Option<&str>,
    input: &str,
    cache_dir: &Path,
    progress: Option<&Value>,
    now: i64,
    cols: usize,
) -> String {
    let mut out = command
        .filter(|c| !c.trim().is_empty())
        .map(|c| inner_output(c, input, cache_dir))
        .unwrap_or_default();
    if let Some(row) = progress.and_then(|p| render_progress(p, now, cols)) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&row);
        out.push('\n');
    }
    out
}

/// `recompact statusline [-- '<command>']`.
pub fn cmd_statusline(args: &[String]) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let command = args
        .iter()
        .position(|a| a == "--")
        .map(|i| args[i + 1..].join(" "));
    let progress = std::env::var_os("RECOMPACT_SHELL")
        .map(|d| PathBuf::from(d).join("progress.json"))
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(100);
    print!(
        "{}",
        status_line(
            command.as_deref(),
            &input,
            &crate::recompact_home().join("statusline"),
            progress.as_ref(),
            now_unix(),
            cols,
        )
    );
    0
}

// ------------------------------------------------------------------------------------ settings

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The status line command that wraps `original` (falls back to it when recompact is gone).
pub fn wrapped_command(bin: &str, original: &str) -> String {
    format!(
        "if [ -x \"{bin}\" ]; then \"{bin}\" statusline -- {}; else {original}; fi",
        sh_quote(original)
    )
}

fn is_wrapped(status_line: &Value) -> bool {
    status_line
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("\" statusline -- "))
}

/// The byte span of the value of the top-level key `key` in a JSON object's text.
fn value_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let quoted = format!("\"{key}\"");
    let at = text.find(&quoted)? + quoted.len();
    let rest = &text[at..];
    let colon = rest.find(':')?;
    if !rest[..colon].trim().is_empty() {
        return None;
    }
    let start = at + colon + 1 + (rest[colon + 1..].len() - rest[colon + 1..].trim_start().len());
    let bytes = text.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some((start, i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

/// Replace the top-level `statusLine` value in a settings file's text, changing nothing else.
fn replace_status_line(text: &str, new: &Value) -> Result<String, String> {
    let before: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let (s, e) = value_span(text, "statusLine").ok_or("statusLine not found")?;
    let out = format!("{}{}{}", &text[..s], new, &text[e..]);
    let mut expected = before;
    expected["statusLine"] = new.clone();
    let after: Value =
        serde_json::from_str(&out).map_err(|e| format!("edit broke the JSON: {e}"))?;
    if after != expected {
        return Err("edit changed more than statusLine".into());
    }
    Ok(out)
}

/// Settings text with the user's status line wrapped, and the original status line; `None` when
/// there is none to wrap or it is wrapped already.
pub fn with_statusline(text: &str, bin: &str) -> Result<Option<(String, Value)>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let Some(sl) = v.get("statusLine").filter(|s| s.is_object()) else {
        return Ok(None);
    };
    if is_wrapped(sl) {
        return Ok(None);
    }
    let original = sl
        .get("command")
        .and_then(Value::as_str)
        .ok_or("the status line has no command")?;
    let mut new = sl.clone();
    new["command"] = json!(wrapped_command(bin, original));
    new["refreshInterval"] = json!(1);
    Ok(Some((replace_status_line(text, &new)?, sl.clone())))
}

/// Settings text with the wrapped status line put back to `original`.
pub fn without_statusline(text: &str, original: &Value) -> Result<Option<String>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    if !v.get("statusLine").is_some_and(is_wrapped) {
        return Ok(None);
    }
    replace_status_line(text, original).map(Some)
}

fn original_path() -> PathBuf {
    crate::recompact_home().join("statusline-original.json")
}

fn write_settings(settings: &Path, old: &str, new: &str) -> bool {
    let _ = fs::write(settings.with_extension("json.recompact-backup"), old);
    let tmp = settings.with_extension("json.recompact-tmp");
    fs::write(&tmp, new).is_ok() && fs::rename(&tmp, settings).is_ok()
}

/// Add compaction progress to the user's status line (`install`).
pub fn wrap_statusline(settings: &Path, bin: &str) -> String {
    let Ok(text) = fs::read_to_string(settings) else {
        return "status line: no user settings file".into();
    };
    match with_statusline(&text, bin) {
        Ok(Some((updated, original))) => {
            let _ = fs::create_dir_all(crate::recompact_home());
            let _ = fs::write(original_path(), original.to_string());
            if write_settings(settings, &text, &updated) {
                "status line: shows compaction progress under your own".into()
            } else {
                "status line: could not write the settings file".into()
            }
        }
        Ok(None) if text.contains("\"statusLine\"") => "status line: already shows progress".into(),
        Ok(None) => "status line: none set, so compaction progress has nowhere to show (set one \
up with /statusline, then run `recompact install` again)"
            .into(),
        Err(e) => format!("status line: left as is ({e})"),
    }
}

/// Put the user's status line back (`uninstall`).
pub fn unwrap_statusline(settings: &Path) -> Option<String> {
    let text = fs::read_to_string(settings).ok()?;
    let original: Value = serde_json::from_str(&fs::read_to_string(original_path()).ok()?).ok()?;
    match without_statusline(&text, &original) {
        Ok(Some(updated)) if write_settings(settings, &text, &updated) => {
            let _ = fs::remove_file(original_path());
            Some("status line: restored".into())
        }
        Ok(_) => None,
        Err(e) => Some(format!("status line: left as is ({e})")),
    }
}
