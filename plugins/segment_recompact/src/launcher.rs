//! Handoff in place: compaction that happens in the terminal you are already working in.
//!
//! `recompact shell` runs `claude` as its child and otherwise stays out of the way. Hooks inside
//! that claude report which session is live and ask for a handoff: the user typing a bare
//! `/recompact`, a turn ending with the context over the threshold, or the agent queueing one
//! itself (`recompact handoff`). The launcher then stops claude with SIGTERM (Claude Code exits
//! cleanly on it: terminal restored, transcript flushed, exit 143), compacts the session with
//! progress on the terminal, and resumes the twin in the same terminal with the same flags.
//!
//! The launcher exports `RECOMPACT_SHELL=<state dir>`. Each file there has one writer, so no
//! process read-modify-writes another's state:
//! - `config.json`: the launcher (its child's pid, thresholds, the re-arm floor)
//! - `session.json`: the SessionStart hook of the launcher's own claude (live session, cwd)
//! - `request.json`: hooks and `recompact handoff` (which session, why, whether to continue)
//! - `nudged.json`: the PostToolUse hook (the last checkpoint request)
//! - `prewarm.json`: the Stop hook (the last background prewarm)

use std::fs;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::{
    calibrate_lineage, continue_session, has_active_goal, lineage_latest, load_jsonl,
    locate_session, parse_opts, prompt_tokens, rec_type, resume_command, resume_flags,
    select_active, short_id, summarize_for_plan, truthy, ContinueOpts, SummarizeCfg,
    DEFAULT_THRESHOLD,
};

// ------------------------------------------------------------------------------------ signals

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
    fn kill(pid: i32, sig: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn setsid() -> i32;
}

const SIGINT: i32 = 2;
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;
const SIGTSTP_MAC: i32 = 18;
const SIGTSTP_LINUX: i32 = 20;
const WNOHANG: i32 = 1;
const WUNTRACED: i32 = 2;

fn sigtstp() -> i32 {
    if cfg!(target_os = "linux") {
        SIGTSTP_LINUX
    } else {
        SIGTSTP_MAC
    }
}

/// Ctrl-C reaches the whole foreground group when claude is not in raw mode. The launcher must
/// survive it (or the terminal goes back to the shell with claude still running), and during a
/// compaction it means "stop, resume what I had". A caught signal resets to the default in
/// children at exec, so claude still gets ordinary Ctrl-C handling.
fn catch_interrupts() {
    unsafe {
        signal(SIGINT, on_interrupt as *const () as usize);
    }
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    pid > 0 && unsafe { kill(pid as i32, 0) } == 0
}

fn send_signal(pid: u32, sig: i32) -> bool {
    pid > 0 && unsafe { kill(pid as i32, sig) } == 0
}

enum ChildState {
    Running,
    Exited(i32),
    Stopped,
}

fn poll_child(pid: u32) -> ChildState {
    let mut status: i32 = 0;
    let r = unsafe { waitpid(pid as i32, &mut status, WNOHANG | WUNTRACED) };
    if r == 0 {
        return ChildState::Running;
    }
    if r < 0 {
        return ChildState::Exited(1);
    }
    if status & 0xff == 0x7f {
        return ChildState::Stopped;
    }
    if status & 0x7f == 0 {
        ChildState::Exited((status >> 8) & 0xff)
    } else {
        ChildState::Exited(128 + (status & 0x7f))
    }
}

// ------------------------------------------------------------------------------------ state

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn state_root() -> PathBuf {
    home().join(".claude").join("recompact").join("shell")
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
}

/// Write-then-rename, so a reader never sees half a file.
fn write_json(p: &Path, v: &Value) {
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    if fs::write(&tmp, v.to_string()).is_ok() {
        let _ = fs::rename(&tmp, p);
    }
}

fn get_u(v: &Value, k: &str) -> Option<usize> {
    v.get(k).and_then(|x| x.as_u64()).map(|x| x as usize)
}

fn get_s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

/// The launcher this process runs under, if any, and still alive.
pub struct Shell {
    pub dir: PathBuf,
    pub config: Value,
}

impl Shell {
    pub fn from_env() -> Option<Shell> {
        let dir = PathBuf::from(std::env::var("RECOMPACT_SHELL").ok()?);
        let config = read_json(&dir.join("config.json"))?;
        let launcher = get_u(&config, "launcher")? as u32;
        if !pid_alive(launcher) {
            return None;
        }
        Some(Shell { dir, config })
    }

    fn child(&self) -> u32 {
        get_u(&self.config, "child").unwrap_or(0) as u32
    }

    pub fn session(&self) -> Option<Value> {
        read_json(&self.dir.join("session.json"))
    }

    fn tracks(&self, session: &str) -> bool {
        self.session()
            .and_then(|s| get_s(&s, "session").map(|x| x == session))
            .unwrap_or(false)
    }

    /// Is the calling process (a hook, or a command the agent ran) inside the launcher's own
    /// claude, rather than a claude started from within it that inherited the variable?
    fn is_own_claude(&self) -> bool {
        let child = self.child();
        if child == 0 {
            return false;
        }
        if let Some(pid) = std::env::var("CLAUDE_PID")
            .ok()
            .and_then(|p| p.parse::<u32>().ok())
        {
            return pid == child;
        }
        ancestors(4).contains(&child)
    }
}

fn ancestors(depth: usize) -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..depth {
        if pid <= 1 {
            break;
        }
        out.push(pid);
        let next = Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            });
        match next {
            Some(p) => pid = p,
            None => break,
        }
    }
    out
}

// ------------------------------------------------------------------------------------ sizing

/// Prompt tokens of the session's latest main-thread response, read from the transcript's tail:
/// what the context holds right now, preserved thinking included. Cheap enough for every hook.
pub fn live_tokens(path: &Path) -> Option<usize> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    for window in [2u64 << 20, 32u64 << 20] {
        let start = len.saturating_sub(window);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf);
        for line in text.lines().rev() {
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(r) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if rec_type(&r) != "assistant" || truthy(&r, "isSidechain") {
                continue;
            }
            if let Some(t) = r.pointer("/message/usage").and_then(prompt_tokens) {
                return Some(t);
            }
        }
        if start == 0 {
            break;
        }
    }
    None
}

/// Where automatic handoff fires, when not configured. A 1M-token window gets room to work;
/// a 200k one hands off well before Claude Code's own compaction would.
pub fn default_at(model: &str, live: usize) -> usize {
    if model.contains("[1m]") || live > 200_000 {
        400_000
    } else {
        140_000
    }
}

fn default_checkpoint(at: usize) -> usize {
    if at >= 300_000 {
        at + 150_000
    } else {
        at + 30_000
    }
}

fn fmt_k(t: usize) -> String {
    if t >= 10_000 {
        format!("{}k", (t + 500) / 1000)
    } else {
        t.to_string()
    }
}

// ------------------------------------------------------------------------------------ hooks

fn hook_session(input: &Value) -> Option<&str> {
    get_s(input, "session_id")
}

/// SessionStart: the launcher learns which session its claude is in (startup, resume, /clear,
/// a /resume inside the TUI).
pub fn on_session_start(input: &Value) {
    let Some(shell) = Shell::from_env() else {
        return;
    };
    // The hook can outrun the launcher's write of the child pid by a few milliseconds.
    let mut shell = shell;
    for _ in 0..5 {
        if shell.child() != 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        match Shell::from_env() {
            Some(s) => shell = s,
            None => return,
        }
    }
    if !shell.is_own_claude() {
        return;
    }
    let Some(session) = hook_session(input) else {
        return;
    };
    write_json(
        &shell.dir.join("session.json"),
        &json!({
            "session": session,
            "transcript": get_s(input, "transcript_path"),
            "cwd": get_s(input, "cwd"),
            "model": get_s(input, "model"),
            "source": get_s(input, "source"),
        }),
    );
}

fn is_bare_recompact(prompt: &str) -> bool {
    matches!(prompt.trim(), "/recompact" | "/segment-recompact:recompact")
}

fn request(shell: &Shell, input: &Value, reason: &str, kick: bool, force: bool, ready: bool) {
    write_json(
        &shell.dir.join("request.json"),
        &json!({
            "session": hook_session(input),
            "transcript": get_s(input, "transcript_path"),
            "cwd": get_s(input, "cwd"),
            "reason": reason,
            "kick": kick,
            "force": force,
            "ready": ready,
        }),
    );
}

/// UserPromptSubmit: a bare `/recompact` under the launcher is handled without the model. The
/// prompt is blocked (never reaches the context) and the launcher takes over within ~100 ms.
pub fn on_prompt(input: &Value) -> Option<Value> {
    on_prompt_in(Shell::from_env(), input)
}

pub fn on_prompt_in(shell: Option<Shell>, input: &Value) -> Option<Value> {
    let prompt = get_s(input, "prompt")?;
    if !is_bare_recompact(prompt) {
        return None;
    }
    let shell = shell?;
    let session = hook_session(input)?;
    if !shell.tracks(session) && !shell.is_own_claude() {
        return None;
    }
    request(&shell, input, "manual", false, true, true);
    Some(json!({
        "decision": "block",
        "reason": "recompact: compacting this session; it resumes here in a moment."
    }))
}

fn nudged_at(shell: &Shell, session: &str) -> Option<usize> {
    let n = read_json(&shell.dir.join("nudged.json"))?;
    (get_s(&n, "session") == Some(session)).then(|| get_u(&n, "tokens"))?
}

fn thresholds(shell: &Shell, model: &str, live: usize) -> (usize, usize) {
    let at = get_u(&shell.config, "at").unwrap_or_else(|| default_at(model, live));
    let checkpoint =
        get_u(&shell.config, "checkpoint_at").unwrap_or_else(|| default_checkpoint(at));
    // After a handoff that could not get far below the threshold, wait for real growth before
    // the next one instead of compacting every turn.
    let rearm = get_u(&shell.config, "rearm").unwrap_or(0);
    (at.max(rearm), checkpoint.max(rearm))
}

fn session_model(shell: &Shell) -> String {
    shell
        .session()
        .and_then(|s| get_s(&s, "model").map(String::from))
        .unwrap_or_default()
}

/// Stop: the turn is over and the transcript complete, the safe moment to hand off.
pub fn on_stop(input: &Value) -> Option<Value> {
    on_stop_in(Shell::from_env(), input)
}

pub fn on_stop_in(shell: Option<Shell>, input: &Value) -> Option<Value> {
    if truthy(input, "stop_hook_active") {
        return None;
    }
    let session = hook_session(input)?;
    let transcript = PathBuf::from(get_s(input, "transcript_path")?);
    let Some(shell) = shell else {
        return suggest(session, &transcript);
    };
    if !shell.tracks(session) {
        return None;
    }
    // A handoff the agent queued during the turn goes now.
    if let Some(mut req) = read_json(&shell.dir.join("request.json")) {
        if get_s(&req, "session") == Some(session) && req.get("ready") != Some(&json!(true)) {
            req["ready"] = json!(true);
            write_json(&shell.dir.join("request.json"), &req);
            return Some(
                json!({"systemMessage": "recompact: compacting this session; it resumes here in a moment."}),
            );
        }
    }
    if shell.config.get("auto") == Some(&json!(false)) {
        return None;
    }
    let live = live_tokens(&transcript)?;
    let (at, _) = thresholds(&shell, &session_model(&shell), live);
    if live < at {
        // From halfway on, keep the summary cache warm in the background, so the handoff (or a
        // manual /recompact) finds almost every summary already written.
        let target = get_u(&shell.config, "target").unwrap_or(DEFAULT_THRESHOLD);
        if shell.config.get("summarize") == Some(&json!(true)) && live * 2 >= at && live > target {
            maybe_prewarm(&shell, session, &transcript, live);
        }
        return None;
    }
    let busy = input
        .get("background_tasks")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());
    let crons = input
        .get("session_crons")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());
    if busy || crons {
        // Stopping claude would kill its background work or drop its scheduled wakeups.
        let key = format!("deferred-{}", short_id(session));
        if read_json(&shell.dir.join(format!("{key}.json"))).is_none() {
            write_json(
                &shell.dir.join(format!("{key}.json")),
                &json!({"tokens": live}),
            );
            return Some(json!({"systemMessage": format!(
                "recompact: context is {} (≥ {}), but {} {}; compaction waits. Type /recompact to do it now.",
                fmt_k(live), fmt_k(at),
                if busy { "background tasks are running" } else { "this session has scheduled wakeups" },
                if busy { "and would be killed" } else { "that would be lost" })}));
        }
        return None;
    }
    let kick = nudged_at(&shell, session).is_some();
    request(&shell, input, "auto", kick, false, true);
    Some(json!({"systemMessage": format!(
        "recompact: context is {} (≥ {}); compacting and resuming here.", fmt_k(live), fmt_k(at))}))
}

/// PostToolUse: deep into a single long turn, ask the agent to reach a checkpoint so the handoff
/// lands on a clean boundary instead of Claude Code compacting mid-step.
pub fn on_post_tool_use(input: &Value) -> Option<Value> {
    on_post_tool_use_in(Shell::from_env(), input)
}

pub fn on_post_tool_use_in(shell: Option<Shell>, input: &Value) -> Option<Value> {
    let shell = shell?;
    if shell.config.get("auto") == Some(&json!(false)) {
        return None;
    }
    let session = hook_session(input)?;
    if !shell.tracks(session) {
        return None;
    }
    let transcript = PathBuf::from(get_s(input, "transcript_path")?);
    let live = live_tokens(&transcript)?;
    let (_, checkpoint) = thresholds(&shell, &session_model(&shell), live);
    if live < checkpoint {
        return None;
    }
    if nudged_at(&shell, session).is_some_and(|t| live < t + 50_000) {
        return None;
    }
    write_json(
        &shell.dir.join("nudged.json"),
        &json!({"session": session, "tokens": live}),
    );
    Some(
        json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": format!(
        "recompact: the context is at {} tokens. Reach a clean checkpoint: finish the step in progress \
(do not start a new one), then end your turn with a short status: done, in progress, next. The session \
is then compacted and resumed automatically, and you will be told to continue.", fmt_k(live))}}),
    )
}

/// Outside the launcher, say once per 100k of growth that the session is large.
fn suggest(session: &str, transcript: &Path) -> Option<Value> {
    let live = live_tokens(transcript)?;
    let at = default_at("", live);
    if live < at {
        return None;
    }
    let dir = home().join(".claude").join("recompact").join("suggest");
    let mark = dir.join(format!("{session}.json"));
    if read_json(&mark)
        .and_then(|v| get_u(&v, "tokens"))
        .is_some_and(|t| live < t + 100_000)
    {
        return None;
    }
    let _ = fs::create_dir_all(&dir);
    write_json(&mark, &json!({"tokens": live}));
    Some(json!({"systemMessage": format!(
        "recompact: this session is at {} tokens. Type /recompact to compact it, or start claude with \
`recompact shell` so it happens by itself.", fmt_k(live))}))
}

fn maybe_prewarm(shell: &Shell, session: &str, transcript: &Path, live: usize) {
    let marker = shell.dir.join("prewarm.json");
    if let Some(p) = read_json(&marker) {
        let running = get_u(&p, "pid").is_some_and(|pid| pid_alive(pid as u32));
        let recent = get_s(&p, "session") == Some(session)
            && get_u(&p, "tokens").is_some_and(|t| live < t + 60_000);
        if running || recent {
            return;
        }
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("prewarm").arg(transcript);
    for key in ["target", "summarize_with"] {
        if let Some(v) = shell.config.get(key).and_then(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        }) {
            cmd.arg(format!("--{}", key.replace('_', "-"))).arg(v);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("RECOMPACT_INTERNAL", "1")
        .env_remove("RECOMPACT_SHELL");
    // Its own session, so it outlives the hook and never shares claude's process group.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    if let Ok(child) = cmd.spawn() {
        write_json(
            &marker,
            &json!({"session": session, "tokens": live, "pid": child.id()}),
        );
    }
}

// ------------------------------------------------------------------------------------ prewarm

/// `recompact prewarm <session>`: summarize, into the cache, what compacting now would
/// summarize, so the handoff itself is mostly cache hits.
pub fn cmd_prewarm(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    let Some(arg) = pos.first() else {
        eprintln!(
            "usage: recompact prewarm <session.jsonl | id> [--target N] [--summarize-with M]"
        );
        return 2;
    };
    let path = if Path::new(arg).is_file() {
        PathBuf::from(arg)
    } else {
        match locate_session(None, arg) {
            Some(p) => p,
            None => {
                eprintln!("prewarm: no session {arg}");
                return 1;
            }
        }
    };
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let latest = lineage_latest(&dir, &id);
    let lock = dir.join(format!(".recompact-prewarm-{}.lock", short_id(&latest)));
    if let Some(pid) = fs::read_to_string(&lock)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        if pid_alive(pid) {
            return 0;
        }
    }
    let _ = fs::write(&lock, std::process::id().to_string());
    let mut o = continue_opts(&opts, Some("haiku"));
    o.force = true;
    let (all, _) = select_active(load_jsonl(&dir.join(format!("{latest}.jsonl"))));
    let calib = calibrate_lineage(&all);
    let active: Vec<Value> = all
        .into_iter()
        .filter(|r| !truthy(r, "recompactPreamble"))
        .collect();
    if let Some(cfg) = o.summarize.clone() {
        let failed = summarize_for_plan(
            &active,
            &calib,
            &cfg,
            &o,
            &dir.join(".recompact-summary-cache.json"),
        );
        eprintln!("prewarm: done ({} unit(s) not summarized)", failed.len());
    }
    let _ = fs::remove_file(&lock);
    0
}

/// ContinueOpts for a handoff: `--summarize-with` (default haiku; `mask` or `--mask` for none),
/// `--target` (default 120k), threshold from `--at`.
fn continue_opts(
    opts: &serde_json::Map<String, Value>,
    default_model: Option<&str>,
) -> ContinueOpts {
    let mut o = crate::continue_opts_from(opts);
    let model = opts
        .get("summarize-with")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| std::env::var("RECOMPACT_SUMMARIZE_WITH").ok())
        .or_else(|| default_model.map(String::from));
    let mask = opts.get("mask").is_some() || model.as_deref() == Some("mask");
    o.summarize = if mask {
        None
    } else {
        model.map(|m| SummarizeCfg {
            bin: opts
                .get("claude-bin")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| std::env::var("RECOMPACT_CLAUDE_BIN").ok())
                .unwrap_or_else(|| "claude".into()),
            model: m,
            escalate_with: None,
            escalate_above: 0.4,
        })
    };
    if o.target.is_none() {
        o.target = std::env::var("RECOMPACT_TARGET")
            .ok()
            .and_then(|s| s.parse().ok())
            .or(Some(DEFAULT_THRESHOLD));
    }
    o
}

// ------------------------------------------------------------------------------------ handoff

/// `recompact handoff [session]`: compact the session this command runs in.
///
/// Under the launcher it only queues the request: when the turn ends, the launcher compacts and
/// resumes in the same terminal (`--continue-after` makes the resumed session carry on working).
/// Without it, it compacts now and prints the resume command (also copied to the clipboard).
pub fn cmd_handoff(args: &[String]) -> i32 {
    let (pos, mut opts) = parse_opts(args);
    let Some(session) = pos
        .first()
        .cloned()
        .or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())
    else {
        eprintln!("handoff: no session given and CLAUDE_CODE_SESSION_ID is not set");
        return 2;
    };
    let kick = opts.contains_key("continue-after");
    if let Some(shell) = Shell::from_env() {
        if shell.is_own_claude() {
            let transcript = shell
                .session()
                .filter(|s| get_s(s, "session") == Some(session.as_str()))
                .and_then(|s| get_s(&s, "transcript").map(String::from));
            write_json(
                &shell.dir.join("request.json"),
                &json!({"session": session, "transcript": transcript, "reason": "agent",
                        "kick": kick, "force": true, "ready": false}),
            );
            println!(
                "Handoff queued. When this turn ends, recompact compacts session {} and resumes it in this terminal{}.",
                short_id(&session),
                if kick { ", and the resumed session continues the work" } else { "" }
            );
            return 0;
        }
    }
    let Some(path) = locate_session(None, &session) else {
        eprintln!("handoff: no session {session} under ~/.claude/projects");
        return 1;
    };
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    opts.insert("force".into(), json!(true));
    let o = continue_opts(&opts, Some("haiku"));
    let (next, rc) = continue_session(&dir, &session, &o);
    if next == session {
        eprintln!("handoff: nothing was compacted (rc {rc})");
        return if rc == 0 { 1 } else { rc };
    }
    let flags = resume_flags(&load_jsonl(&dir.join(format!("{next}.jsonl"))));
    let cmd = resume_command(&next, &flags);
    let copied = copy_to_clipboard(&cmd);
    println!("{cmd}");
    eprintln!(
        "handoff: exit this session (/exit) and run the command above{}. \
Run claude as `recompact shell` next time and this happens in place.",
        if copied {
            " (it is on the clipboard)"
        } else {
            ""
        }
    );
    rc
}

fn copy_to_clipboard(text: &str) -> bool {
    let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

// ------------------------------------------------------------------------------------ launcher

/// Claude flags that take one value, several (until the next flag), or an optional one.
const VALUE_FLAGS: &[&str] = &[
    "--agent",
    "--agents",
    "--append-system-prompt",
    "--autocompact",
    "--debug-file",
    "--effort",
    "--environment",
    "--fallback-model",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--model",
    "-n",
    "--name",
    "--output-format",
    "--permission-mode",
    "--permission-prompts",
    "--plugin-dir",
    "--plugin-url",
    "--remote-control-session-name-prefix",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-snapshot",
];
const VARIADIC_FLAGS: &[&str] = &[
    "--add-dir",
    "--allowedTools",
    "--allowed-tools",
    "--betas",
    "--disallowedTools",
    "--disallowed-tools",
    "--file",
    "--mcp-config",
    "--tools",
];
const OPTIONAL_FLAGS: &[&str] = &[
    "--cloud",
    "-d",
    "--debug",
    "--from-pr",
    "--prompt-suggestions",
    "--remote-control",
    "-r",
    "--resume",
    "--teleport",
    "-w",
    "--worktree",
];
/// Dropped when relaunching into a twin: which session to open is the launcher's call now, and
/// the model and effort come from the session itself (they may have changed mid-session).
const SESSION_FLAGS: &[&str] = &[
    "-r",
    "--resume",
    "-c",
    "--continue",
    "--session-id",
    "--fork-session",
    "--from-pr",
    "--teleport",
    "-w",
    "--worktree",
    "--model",
    "--effort",
];
/// Invocations that are not an interactive session: run claude directly.
const PASSTHROUGH_FLAGS: &[&str] = &[
    "-p",
    "--print",
    "-h",
    "--help",
    "-v",
    "--version",
    "--bg",
    "--background",
    "--cloud",
    "--teleport",
];
const SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "doctor",
    "gateway",
    "import",
    "install",
    "logs",
    "mcp",
    "plugin",
    "plugins",
    "project",
    "respawn",
    "rm",
    "setup-token",
    "stop",
    "kill",
    "ultrareview",
    "update",
    "upgrade",
];

/// One parsed claude argument: the flag (or positional) and its values.
#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    pub flag: Option<String>,
    pub values: Vec<String>,
}

pub fn parse_claude_args(args: &[String]) -> Vec<Arg> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            out.extend(args[i + 1..].iter().map(|v| Arg {
                flag: None,
                values: vec![v.clone()],
            }));
            break;
        }
        if !a.starts_with('-') || a == "-" {
            out.push(Arg {
                flag: None,
                values: vec![a.clone()],
            });
            i += 1;
            continue;
        }
        if let Some((f, v)) = a.split_once('=') {
            out.push(Arg {
                flag: Some(f.to_string()),
                values: vec![v.to_string()],
            });
            i += 1;
            continue;
        }
        let flag = a.clone();
        let mut values = Vec::new();
        i += 1;
        let takes_next = |j: usize| args.get(j).is_some_and(|n| !n.starts_with('-'));
        if VALUE_FLAGS.contains(&flag.as_str()) {
            if let Some(v) = args.get(i) {
                values.push(v.clone());
                i += 1;
            }
        } else if VARIADIC_FLAGS.contains(&flag.as_str()) {
            while takes_next(i) {
                values.push(args[i].clone());
                i += 1;
            }
        } else if OPTIONAL_FLAGS.contains(&flag.as_str()) && takes_next(i) {
            values.push(args[i].clone());
            i += 1;
        }
        out.push(Arg {
            flag: Some(flag),
            values,
        });
    }
    out
}

fn flatten(args: &[Arg]) -> Vec<String> {
    let mut out = Vec::new();
    for a in args {
        if let Some(f) = &a.flag {
            out.push(f.clone());
        }
        out.extend(a.values.iter().cloned());
    }
    out
}

/// What carries over to a relaunch: every flag except the session-selecting ones; no prompt.
pub fn carry_args(args: &[Arg]) -> Vec<String> {
    flatten(
        &args
            .iter()
            .filter(|a| {
                a.flag
                    .as_deref()
                    .is_some_and(|f| !SESSION_FLAGS.contains(&f))
            })
            .cloned()
            .collect::<Vec<_>>(),
    )
}

fn flag_value<'a>(args: &'a [Arg], names: &[&str]) -> Option<&'a str> {
    args.iter()
        .rev()
        .find(|a| a.flag.as_deref().is_some_and(|f| names.contains(&f)))
        .and_then(|a| a.values.first().map(String::as_str))
}

pub fn is_passthrough(args: &[Arg]) -> bool {
    args.iter().any(|a| {
        a.flag
            .as_deref()
            .is_some_and(|f| PASSTHROUGH_FLAGS.contains(&f))
    }) || args
        .first()
        .is_some_and(|a| a.flag.is_none() && SUBCOMMANDS.contains(&a.values[0].as_str()))
}

struct Launch {
    state_root: PathBuf,
    bin: String,
    at: Option<usize>,
    checkpoint_at: Option<usize>,
    auto: bool,
    kick: String,
    max_cycles: usize,
    dir: Option<PathBuf>,
    interactive: bool,
    copts_raw: serde_json::Map<String, Value>,
}

/// Split `recompact shell` arguments into the launcher's own options and claude's.
fn split_launcher_args(args: &[String]) -> (Launch, Vec<String>) {
    let env_usize = |k: &str| std::env::var(k).ok().and_then(|s| s.parse().ok());
    let mut l = Launch {
        state_root: state_root(),
        bin: std::env::var("RECOMPACT_CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
        at: env_usize("RECOMPACT_AT"),
        checkpoint_at: env_usize("RECOMPACT_CHECKPOINT_AT"),
        auto: std::env::var("RECOMPACT_AUTO")
            .map(|v| v != "0")
            .unwrap_or(true),
        kick:
            "Continue the work from where you left off: the session was compacted at a checkpoint, \
and your last message says what was done and what is next."
                .into(),
        max_cycles: 0,
        dir: None,
        interactive: false,
        copts_raw: serde_json::Map::new(),
    };
    let mut claude = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let val = || args.get(i + 1).cloned().unwrap_or_default();
        match a {
            "--" => {
                claude.extend(args[i + 1..].iter().cloned());
                break;
            }
            "--at" | "--threshold" => {
                l.at = val().parse().ok();
                i += 2;
            }
            "--checkpoint-at" => {
                l.checkpoint_at = val().parse().ok();
                i += 2;
            }
            "--kick" => {
                l.kick = val();
                i += 2;
            }
            "--claude-bin" => {
                l.bin = val();
                l.copts_raw.insert("claude-bin".into(), json!(val()));
                i += 2;
            }
            "--max-cycles" => {
                l.max_cycles = val().parse().unwrap_or(0);
                i += 2;
            }
            "--dir" => {
                l.dir = Some(PathBuf::from(val()));
                i += 2;
            }
            "--state-root" => {
                l.state_root = PathBuf::from(val());
                i += 2;
            }
            "--target" | "--summarize-with" | "--keep" | "--split" | "--tail-budget"
            | "--overhead" => {
                l.copts_raw
                    .insert(a.trim_start_matches("--").into(), json!(val()));
                i += 2;
            }
            "--mask" | "--error-floor" => {
                l.copts_raw
                    .insert(a.trim_start_matches("--").into(), json!(true));
                i += 1;
            }
            "--no-auto" => {
                l.auto = false;
                i += 1;
            }
            "--auto" => {
                l.auto = true;
                i += 1;
            }
            "--interactive" => {
                l.interactive = true;
                i += 1;
            }
            _ => {
                claude.push(args[i].clone());
                i += 1;
            }
        }
    }
    (l, claude)
}

fn color(code: &str, s: &str) -> String {
    if std::io::stderr().is_terminal() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn say(s: &str) {
    eprintln!("{}", color("36", &format!("recompact · {s}")));
}

fn exec_claude(bin: &str, args: &[String]) -> i32 {
    use std::os::unix::process::CommandExt;
    let err = Command::new(bin).args(args).exec();
    eprintln!("recompact shell: cannot run {bin}: {err}");
    127
}

/// `recompact shell [options] [claude arguments]`: run claude with compaction handled in place.
pub fn cmd_shell(args: &[String]) -> i32 {
    let (l, claude_args) = split_launcher_args(args);
    let parsed = parse_claude_args(&claude_args);
    if is_passthrough(&parsed) || !(l.interactive || std::io::stdin().is_terminal()) {
        return exec_claude(&l.bin, &claude_args);
    }
    catch_interrupts();
    let state = l.state_root.join(std::process::id().to_string());
    prune_stale_states(&l.state_root);
    if fs::create_dir_all(&state).is_err() {
        return exec_claude(&l.bin, &claude_args);
    }
    let copts = continue_opts(&l.copts_raw, Some("haiku"));
    let carry = carry_args(&parsed);
    let fallback_model = flag_value(&parsed, &["--model"]).map(String::from);
    let fallback_effort = flag_value(&parsed, &["--effort"]).map(String::from);
    let mut rearm = 0usize;

    // Resuming a session that is already over the threshold: compact it before opening it.
    let mut next_args: Vec<String> = claude_args.clone();
    let mut cwd: Option<PathBuf> = None;
    if let Some(id) = flag_value(&parsed, &["-r", "--resume"]) {
        if l.auto {
            let path = match &l.dir {
                Some(d) => Some(d.join(format!("{id}.jsonl"))),
                None => locate_session(None, id),
            };
            if let Some(path) = path.filter(|p| p.exists()) {
                let live = live_tokens(&path).unwrap_or(0);
                let at = l.at.unwrap_or_else(|| default_at("", live));
                if live >= at {
                    say(&format!(
                        "{} is at {} (≥ {}); compacting before resuming it",
                        short_id(id),
                        fmt_k(live),
                        fmt_k(at)
                    ));
                    let req = json!({"session": id, "transcript": path, "reason": "auto", "kick": false, "force": false});
                    if let Some((twin, est)) = run_handoff(&req, &copts, at) {
                        rearm = est;
                        next_args = relaunch_args(
                            &carry,
                            &twin,
                            &path,
                            &fallback_model,
                            &fallback_effort,
                            None,
                        );
                    }
                }
            }
        }
    }

    let mut cycles = 0usize;
    loop {
        cycles += 1;
        let _ = fs::remove_file(state.join("request.json"));
        let _ = fs::remove_file(state.join("nudged.json"));
        let mut cmd = Command::new(&l.bin);
        cmd.args(&next_args).env("RECOMPACT_SHELL", &state);
        if let Some(d) = cwd.as_ref().filter(|d| d.is_dir()) {
            cmd.current_dir(d);
        }
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("recompact shell: cannot run {}: {e}", l.bin);
                let _ = fs::remove_dir_all(&state);
                return 127;
            }
        };
        let pid = child.id();
        write_json(
            &state.join("config.json"),
            &json!({
                "launcher": std::process::id(), "child": pid, "at": l.at,
                "checkpoint_at": l.checkpoint_at, "auto": l.auto, "rearm": rearm,
                "summarize": copts.summarize.is_some(),
                "summarize_with": copts.summarize.as_ref().map(|c| c.model.clone()),
                "target": copts.target,
            }),
        );
        let (code, req) = supervise(pid, &state);
        let session = read_json(&state.join("session.json"));
        // SIGTERM from anyone else (an agent running `kill -TERM $CLAUDE_PID`) is a handoff too.
        let req = req.or_else(|| {
            (code == 143).then(|| {
                let s = session.clone().unwrap_or(json!({}));
                json!({"session": s.get("session"), "transcript": s.get("transcript"),
                       "cwd": s.get("cwd"), "reason": "signal", "kick": false, "force": true})
            })
        });
        let Some(req) = req else {
            let _ = fs::remove_dir_all(&state);
            return code;
        };
        if l.max_cycles > 0 && cycles >= l.max_cycles {
            let _ = fs::remove_dir_all(&state);
            return code;
        }
        if let Some(p) = read_json(&state.join("prewarm.json")).and_then(|p| get_u(&p, "pid")) {
            send_signal(p as u32, SIGTERM);
        }
        cwd = get_s(&req, "cwd")
            .or_else(|| session.as_ref().and_then(|s| get_s(s, "cwd")))
            .map(PathBuf::from)
            .or(cwd);
        let Some(transcript) = request_transcript(&req, &l) else {
            say("could not find the session to compact; starting claude again");
            next_args = carry.clone();
            next_args.push("--continue".into());
            continue;
        };
        let at = l.at.unwrap_or_else(|| {
            let model = session
                .as_ref()
                .and_then(|s| get_s(s, "model"))
                .unwrap_or("");
            default_at(model, live_tokens(&transcript).unwrap_or(0))
        });
        let (twin, est) = match run_handoff(&req, &copts, at) {
            Some(v) => v,
            None => {
                let id = get_s(&req, "session").unwrap_or("").to_string();
                (id, 0)
            }
        };
        rearm = if est > 0 {
            (est + (at / 4).max(40_000)).max(at)
        } else {
            rearm
        };
        let kick = req.get("kick") == Some(&json!(true));
        next_args = relaunch_args(
            &carry,
            &twin,
            &transcript,
            &fallback_model,
            &fallback_effort,
            kick.then_some(l.kick.as_str()),
        );
    }
}

fn request_transcript(req: &Value, l: &Launch) -> Option<PathBuf> {
    if let Some(t) = get_s(req, "transcript")
        .map(PathBuf::from)
        .filter(|p| p.exists())
    {
        return Some(t);
    }
    let id = get_s(req, "session")?;
    match &l.dir {
        Some(d) => Some(d.join(format!("{id}.jsonl"))).filter(|p| p.exists()),
        None => locate_session(None, id),
    }
}

fn relaunch_args(
    carry: &[String],
    twin: &str,
    transcript: &Path,
    fallback_model: &Option<String>,
    fallback_effort: &Option<String>,
    kick: Option<&str>,
) -> Vec<String> {
    let dir = transcript.parent().unwrap_or(Path::new("."));
    let records = load_jsonl(&dir.join(format!("{twin}.jsonl")));
    let mut flags = resume_flags(&records);
    for (flag, fallback) in [("--model", fallback_model), ("--effort", fallback_effort)] {
        if !flags.iter().any(|f| f == flag) {
            if let Some(v) = fallback {
                flags.push(flag.into());
                flags.push(v.clone());
            }
        }
    }
    let mut out: Vec<String> = carry.to_vec();
    out.push("--resume".into());
    out.push(twin.to_string());
    out.extend(flags);
    if let Some(k) = kick.or_else(|| has_active_goal(&records).then_some("continue")) {
        out.push(k.to_string());
    }
    out
}

/// Wait for claude to exit, stopping it when a ready handoff request appears. Returns claude's
/// exit code and the request that ended it, if any.
fn supervise(pid: u32, state: &Path) -> (i32, Option<Value>) {
    let req_path = state.join("request.json");
    loop {
        match poll_child(pid) {
            ChildState::Exited(code) => {
                let req = read_json(&req_path).filter(|r| r.get("ready") == Some(&json!(true)));
                return (code, req);
            }
            // Ctrl-Z inside claude stops only claude; stop the launcher with it so the shell gets
            // the terminal back, and `fg` resumes both.
            ChildState::Stopped => unsafe {
                kill(0, sigtstp());
            },
            ChildState::Running => {}
        }
        INTERRUPTED.store(false, Ordering::SeqCst);
        if let Some(req) = read_json(&req_path).filter(|r| r.get("ready") == Some(&json!(true))) {
            send_signal(pid, SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let ChildState::Exited(code) = poll_child(pid) {
                    return (code, Some(req));
                }
                if Instant::now() > deadline {
                    send_signal(pid, SIGKILL);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Compact the requested session. Returns the id to resume (the original when nothing came of
/// it or the user pressed Ctrl-C) and the twin's estimated size.
fn run_handoff(req: &Value, copts: &ContinueOpts, at: usize) -> Option<(String, usize)> {
    let id = get_s(req, "session")?.to_string();
    let transcript = PathBuf::from(get_s(req, "transcript")?);
    let dir = transcript.parent()?.to_path_buf();
    let live = live_tokens(&transcript).unwrap_or(0);
    let mut o = copts.clone();
    o.threshold = at;
    o.force = req.get("force") == Some(&json!(true));
    let why = match get_s(req, "reason") {
        Some("auto") => format!("context {} ≥ {}", fmt_k(live), fmt_k(at)),
        Some("agent") => "the agent asked for a checkpoint".into(),
        _ => "requested".into(),
    };
    say(&format!(
        "compacting {} ({why}) with {}; Ctrl-C resumes it as it is",
        short_id(&id),
        o.summarize
            .as_ref()
            .map(|c| c.model.as_str())
            .unwrap_or("masking only")
    ));
    INTERRUPTED.store(false, Ordering::SeqCst);
    let t0 = Instant::now();
    let (next, rc) = continue_session(&dir, &id, &o);
    if INTERRUPTED.swap(false, Ordering::SeqCst) {
        say(&format!(
            "interrupted; resuming {} unchanged",
            short_id(&id)
        ));
        return Some((lineage_latest(&dir, &id), 0));
    }
    let twin_path = dir.join(format!("{next}.jsonl"));
    let (active, _) = select_active(load_jsonl(&twin_path));
    let est = calibrate_lineage(&active).context_tokens(&active);
    if next == id || rc != 0 {
        say(&format!(
            "no compaction (exit {rc}); resuming {}",
            short_id(&next)
        ));
    } else {
        say(&format!(
            "{} → ~{} in {:.0}s; resuming {}",
            fmt_k(live),
            fmt_k(est),
            t0.elapsed().as_secs_f32(),
            short_id(&next)
        ));
    }
    Some((next, est))
}

fn prune_stale_states(root: &Path) {
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let pid = e.file_name().to_string_lossy().parse::<u32>().unwrap_or(0);
        if pid != 0 && pid != std::process::id() && !pid_alive(pid) {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}
