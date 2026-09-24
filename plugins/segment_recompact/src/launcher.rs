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
//! - `session.json`: the SessionStart hook of the launcher's own claude (live session)
//! - `start.json`: the first SessionStart of this launcher (the directory claude started in)
//! - `request.json`: hooks and `recompact handoff` (which session, why, whether to continue)
//! - `nudged.json`: the PostToolUse hook (the last checkpoint request)
//! - `prewarm.json`: the Stop hook (the running background prewarm)

use std::fs;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::{
    calibrate_lineage, continue_session, has_active_goal, lineage_latest, lineage_remove,
    load_jsonl, locate_session, parse_opts, prompt_tokens, rec_type, resume_command, resume_flags,
    select_active, short_id, summarize_for_plan, truthy, ContinueOpts, SummarizeCfg, CANCEL,
    DEFAULT_THRESHOLD,
};

// ------------------------------------------------------------------------------------ signals

extern "C" fn on_interrupt(_: i32) {
    CANCEL.store(true, Ordering::SeqCst);
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
const WNOHANG: i32 = 1;
const WUNTRACED: i32 = 2;

fn sigtstp() -> i32 {
    if cfg!(target_os = "linux") {
        20
    } else {
        18
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
    /// claude, rather than a claude started from within it that inherited the variable? Claude
    /// Code sets `CLAUDE_PID` to its own pid for hooks and tools; a nested claude sets its own.
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

/// The session's latest main-thread response: its prompt tokens (what the context holds right
/// now, preserved thinking included) and its model. Read from the transcript's tail, cheap
/// enough for every hook.
pub fn live_status(path: &Path) -> Option<(usize, String)> {
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
                let model = r
                    .pointer("/message/model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                return Some((t, model));
            }
        }
        if start == 0 {
            break;
        }
    }
    None
}

pub fn live_tokens(path: &Path) -> Option<usize> {
    live_status(path).map(|(t, _)| t)
}

/// The context window a session runs in. Transcripts do not record it (a 1M Opus session logs
/// plain `claude-opus-5-5`, and the `opus` alias gives 1M on current plans), so: an explicit
/// `RECOMPACT_WINDOW`, usage already past 200k, else Haiku at 200k and everything else at 1M.
pub fn window_for(model: &str, live: usize) -> usize {
    if let Ok(w) = std::env::var("RECOMPACT_WINDOW") {
        let w = w.trim().to_lowercase();
        let n = if let Some(m) = w.strip_suffix('m') {
            m.parse::<usize>().ok().map(|x| x * 1_000_000)
        } else if let Some(k) = w.strip_suffix('k') {
            k.parse::<usize>().ok().map(|x| x * 1000)
        } else {
            w.parse().ok()
        };
        if let Some(n) = n {
            return n;
        }
    }
    if live > 200_000 {
        return 1_000_000;
    }
    if model.to_lowercase().contains("haiku") {
        200_000
    } else {
        1_000_000
    }
}

/// Where automatic handoff fires: with room to work on a 1M window, and on a 200k one well
/// before Claude Code's own compaction (~167k).
pub fn default_at(window: usize) -> usize {
    if window >= 1_000_000 {
        400_000
    } else {
        140_000
    }
}

pub fn default_at_for(model: &str, live: usize) -> usize {
    default_at(window_for(model, live))
}

fn default_checkpoint(at: usize, window: usize) -> usize {
    if window >= 1_000_000 {
        at + 150_000
    } else {
        (at + 30_000).min(window.saturating_sub(40_000)).max(at)
    }
}

/// Twin size to aim for: far enough below the trigger that handoffs are not back to back.
pub fn default_target(at: usize) -> usize {
    DEFAULT_THRESHOLD.min(at / 2)
}

/// A twin must get this far under the trigger before the next automatic handoff can fire.
fn rearm_for(twin_estimate: usize, at: usize) -> usize {
    (twin_estimate + (at / 4).max(40_000)).max(at)
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
    let Some(mut shell) = Shell::from_env() else {
        return;
    };
    // The hook can outrun the launcher's write of the child pid by a few milliseconds.
    for _ in 0..10 {
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
            "model": get_s(input, "model"),
            "source": get_s(input, "source"),
            "start_tokens": get_s(input, "transcript_path")
                .and_then(|t| live_status(Path::new(t)))
                .map(|(t, _)| t),
        }),
    );
    let start = shell.dir.join("start.json");
    if !start.exists() {
        write_json(&start, &json!({"cwd": get_s(input, "cwd")}));
    }
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
    if let Some((cmd, arg)) = switch_command(prompt) {
        let transcript = get_s(input, "transcript_path").map(PathBuf::from);
        let managed = shell.as_ref().is_some_and(|sh| {
            hook_session(input).is_some_and(|s| sh.tracks(s)) || sh.is_own_claude()
        });
        let text = switch(
            cmd,
            arg,
            hook_session(input),
            shell.as_ref(),
            transcript.as_deref(),
            managed,
        );
        return Some(json!({"decision": "block", "reason": text}));
    }
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

/// (trigger, checkpoint, target) for the session's current model and size.
fn thresholds(shell: &Shell, session: &str, model: &str, live: usize) -> (usize, usize, usize) {
    let model = if model.is_empty() {
        get_s(&shell.config, "launch_model").unwrap_or("")
    } else {
        model
    };
    let window = window_for(model, live);
    let at = user_at(Some(session))
        .or_else(|| get_u(&shell.config, "at"))
        .unwrap_or_else(|| default_at(window));
    let checkpoint =
        get_u(&shell.config, "checkpoint_at").unwrap_or_else(|| default_checkpoint(at, window));
    let target = get_u(&shell.config, "target").unwrap_or_else(|| default_target(at));
    // After a handoff that could not get far below the trigger, wait for real growth before the
    // next one instead of compacting every turn. Likewise a session that was already over the
    // size when it opened: the user chose to resume it as it is.
    let rearm = get_u(&shell.config, "rearm").unwrap_or(0);
    let opened = shell
        .session()
        .and_then(|s| get_u(&s, "start_tokens"))
        .filter(|&t| t >= at)
        .map(|t| t + (at / 4).max(100_000))
        .unwrap_or(0);
    let floor = rearm.max(opened);
    (at.max(floor), checkpoint.max(floor), target)
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
    let (on, source) = auto_for(&shell.config, Some(session));
    if !on {
        return None;
    }
    let (live, model) = live_status(&transcript)?;
    let (at, _, target) = thresholds(&shell, session, &model, live);
    if live < at {
        // From halfway on, keep the summary cache warm in the background, so the handoff (or a
        // manual /recompact) finds almost every summary already written.
        if shell.config.get("summarize") == Some(&json!(true)) && live * 2 >= at && live > target {
            maybe_prewarm(&shell, session, &transcript, live, target);
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
        let mark = shell
            .dir
            .join(format!("deferred-{}.json", short_id(session)));
        if read_json(&mark).is_none() {
            write_json(&mark, &json!({"tokens": live}));
            return Some(json!({"systemMessage": format!(
                "recompact: context is {} (≥ {}), but {}; compaction waits. Type /recompact to do it now.",
                fmt_k(live), fmt_k(at),
                if busy { "background tasks are running and would be stopped" } else { "this session's scheduled wakeups would be lost" })}));
        }
        return None;
    }
    let kick = nudged_at(&shell, session).is_some();
    // Never a surprise: when it is on only by default, the first turn over the size says what
    // will happen and how to stop it, and the next turn's end compacts. A session switched on
    // for itself (or started with --auto) asked for this, and an agent past its checkpoint
    // request is mid-task with no one typing: those go now.
    let warned = shell.dir.join("warned.json");
    if !kick
        && source == AutoSource::Default
        && read_json(&warned).and_then(|w| get_s(&w, "session").map(|s| s == session)) != Some(true)
    {
        write_json(&warned, &json!({"session": session, "tokens": live}));
        return Some(json!({"systemMessage": format!(
            "recompact: this session is at {} (≥ {}). It compacts in place when your next turn ends. \
/recompact off prevents that; /recompact does it now.",
            fmt_k(live), fmt_k(at))}));
    }
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
    // A subagent's tool call carries the parent's session id; the request is for the main agent.
    if get_s(input, "agent_id").is_some_and(|a| !a.is_empty()) {
        return None;
    }
    let shell = shell?;
    let session = hook_session(input)?;
    if !shell.tracks(session) || !auto_on(&shell.config, Some(session)) {
        return None;
    }
    let transcript = PathBuf::from(get_s(input, "transcript_path")?);
    let (live, model) = live_status(&transcript)?;
    let (_, checkpoint, _) = thresholds(&shell, session, &model, live);
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
    if !auto_on(&json!({}), Some(session)) {
        return None;
    }
    let (live, model) = live_status(transcript)?;
    if live < user_at(Some(session)).unwrap_or_else(|| default_at_for(&model, live)) {
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
        "recompact: this session is at {} tokens. Type /recompact to compact it; run `/recompact setup` \
once to have it happen by itself.", fmt_k(live))}))
}

fn prewarm_running(marker: &Path) -> Option<u32> {
    let pid = get_u(&read_json(marker)?, "pid")? as u32;
    if !pid_alive(pid) {
        return None;
    }
    // The pid may have been reused since; only a prewarm is ours to stop.
    let cmd = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&cmd.stdout)
        .contains(" prewarm ")
        .then_some(pid)
}

fn maybe_prewarm(shell: &Shell, session: &str, transcript: &Path, live: usize, target: usize) {
    let marker = shell.dir.join("prewarm.json");
    if prewarm_running(&marker).is_some() {
        return;
    }
    if let Some(p) = read_json(&marker) {
        if get_s(&p, "session") == Some(session)
            && get_u(&p, "tokens").is_some_and(|t| live < t + 60_000)
        {
            return;
        }
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("prewarm")
        .arg(transcript)
        .arg("--target")
        .arg(target.to_string());
    if let Some(m) = get_s(&shell.config, "summarize_with") {
        cmd.arg("--summarize-with").arg(m);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("RECOMPACT_INTERNAL", "1")
        .env_remove("RECOMPACT_SHELL");
    // Its own session and process group: it outlives the hook, and the launcher can stop it and
    // its summarizer calls together.
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

/// Stop a background prewarm and its summarizer calls (its process group).
fn stop_prewarm(state: &Path) {
    let marker = state.join("prewarm.json");
    if let Some(pid) = prewarm_running(&marker) {
        unsafe {
            kill(-(pid as i32), SIGTERM);
        }
    }
    let _ = fs::remove_file(marker);
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
/// `--target`, and the rest of continue's options.
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
            .and_then(|s| s.parse().ok());
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
    let mut o = continue_opts(&opts, Some("haiku"));
    if o.target.is_none() {
        let (live, model) = live_status(&path).unwrap_or((0, String::new()));
        o.target = Some(forced_target(
            default_target(default_at_for(&model, live)),
            &dir,
            &session,
        ));
    }
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
Run `recompact install` once and this happens in place from then on.",
        if copied {
            " (it is on the clipboard)"
        } else {
            ""
        }
    );
    rc
}

/// An explicit request must shrink the session even when it is already near the target: aim
/// well below its current estimated size.
fn forced_target(target: usize, dir: &Path, session: &str) -> usize {
    let latest = lineage_latest(dir, session);
    let (active, _) = select_active(load_jsonl(&dir.join(format!("{latest}.jsonl"))));
    let est = calibrate_lineage(&active).context_tokens(&active);
    target.min(est * 55 / 100)
}

fn copy_to_clipboard(text: &str) -> bool {
    let Ok(mut child) = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

// ------------------------------------------------------------------------------------ launcher

/// Claude flags (2.1.281, hidden ones included) by arity. An argument the launcher cannot
/// classify means claude runs directly, unwrapped: a relaunch must never mangle a flag.
const VALUE_FLAGS: &[&str] = &[
    "--agent",
    "--agents",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--autocompact",
    "--debug-file",
    "--effort",
    "--environment",
    "--fallback-model",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--max-turns",
    "--model",
    "-n",
    "--name",
    "--output-format",
    "--permission-mode",
    "--permission-prompt-tool",
    "--permission-prompts",
    "--plugin-dir",
    "--plugin-url",
    "--remote-control-session-name-prefix",
    "--resume-session-at",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-file",
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
const BOOL_FLAGS: &[&str] = &[
    "--allow-dangerously-skip-permissions",
    "--ax-screen-reader",
    "--bare",
    "--brief",
    "--chrome",
    "-c",
    "--continue",
    "--dangerously-skip-permissions",
    "--disable-slash-commands",
    "--exclude-dynamic-system-prompt-sections",
    "--fork-session",
    "--forward-subagent-text",
    "--ide",
    "--include-hook-events",
    "--include-partial-messages",
    "--mcp-debug",
    "--no-chrome",
    "--no-session-persistence",
    "-p",
    "--print",
    "--replay-user-messages",
    "--restricted",
    "--safe-mode",
    "--strict-mcp-config",
    "--tmux",
    "--verbose",
    "-h",
    "--help",
    "-v",
    "--version",
    "--bg",
    "--background",
];
/// Dropped when relaunching into a twin: which session to open is the launcher's call; the
/// worktree already exists; the session restores its own permission mode (a flag would override
/// a mode the user changed mid-session).
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
    "--tmux",
    "--resume-session-at",
    "--model",
    "--effort",
    "--permission-mode",
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

fn known_flag(f: &str) -> bool {
    [VALUE_FLAGS, VARIADIC_FLAGS, OPTIONAL_FLAGS, BOOL_FLAGS]
        .iter()
        .any(|t| t.contains(&f))
}

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
                values: vec![format!("={v}")],
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
        match (&a.flag, a.values.first()) {
            // `--flag=value` stays one token.
            (Some(f), Some(v)) if v.starts_with('=') && a.values.len() == 1 => {
                out.push(format!("{f}{v}"));
            }
            _ => {
                if let Some(f) = &a.flag {
                    out.push(f.clone());
                }
                out.extend(a.values.iter().cloned());
            }
        }
    }
    out
}

/// What carries over to a relaunch: every flag except the session-selecting ones; no prompt.
/// Bypass stays available without being forced back on.
pub fn carry_args(args: &[Arg]) -> Vec<String> {
    let kept: Vec<Arg> = args
        .iter()
        .filter(|a| {
            a.flag
                .as_deref()
                .is_some_and(|f| !SESSION_FLAGS.contains(&f))
        })
        .map(|a| {
            if a.flag.as_deref() == Some("--dangerously-skip-permissions") {
                Arg {
                    flag: Some("--allow-dangerously-skip-permissions".into()),
                    values: vec![],
                }
            } else {
                a.clone()
            }
        })
        .collect();
    flatten(&kept)
}

fn flag_value<'a>(args: &'a [Arg], names: &[&str]) -> Option<&'a str> {
    args.iter()
        .rev()
        .find(|a| a.flag.as_deref().is_some_and(|f| names.contains(&f)))
        .and_then(|a| a.values.first().map(|v| v.trim_start_matches('=')))
}

/// Run claude directly: print mode, help, subcommands, and anything the launcher cannot parse.
pub fn is_passthrough(args: &[Arg]) -> bool {
    args.iter().any(|a| {
        a.flag
            .as_deref()
            .is_some_and(|f| PASSTHROUGH_FLAGS.contains(&f) || !known_flag(f))
    }) || args
        .first()
        .is_some_and(|a| a.flag.is_none() && SUBCOMMANDS.contains(&a.values[0].as_str()))
}

/// The model family, for telling a mid-session model switch from the same model spelled
/// differently (`opus` vs `claude-opus-5-5`).
fn family(model: &str) -> Option<&'static str> {
    let m = model.to_lowercase();
    ["opus", "fable", "sonnet", "haiku"]
        .into_iter()
        .find(|f| m.contains(f))
}

/// The model claude starts with when none is passed: the settings' `model`.
fn settings_model(cwd: &Path) -> Option<String> {
    let mut model = None;
    for p in [
        home().join(".claude").join("settings.json"),
        cwd.join(".claude").join("settings.json"),
        cwd.join(".claude").join("settings.local.json"),
    ] {
        if let Some(m) = read_json(&p).and_then(|v| get_s(&v, "model").map(String::from)) {
            model = Some(m);
        }
    }
    model
}

struct Launch {
    state_root: PathBuf,
    bin: String,
    at: Option<usize>,
    checkpoint_at: Option<usize>,
    auto: Option<bool>,
    kick: String,
    max_cycles: usize,
    dir: Option<PathBuf>,
    interactive: bool,
    copts_raw: serde_json::Map<String, Value>,
}

/// Split `recompact shell` arguments into the launcher's own options and claude's. None of the
/// launcher's option names exists in claude.
fn split_launcher_args(args: &[String]) -> (Launch, Vec<String>) {
    let env_usize = |k: &str| std::env::var(k).ok().and_then(|s| s.parse().ok());
    let mut l = Launch {
        state_root: state_root(),
        bin: std::env::var("RECOMPACT_CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
        at: env_usize("RECOMPACT_AT"),
        checkpoint_at: env_usize("RECOMPACT_CHECKPOINT_AT"),
        auto: std::env::var("RECOMPACT_AUTO").ok().map(|v| v != "0"),
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
                claude.extend(args[i..].iter().cloned());
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
                l.auto = Some(false);
                i += 1;
            }
            "--auto" => {
                l.auto = Some(true);
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

/// What the relaunch needs to know about how claude was first started.
struct Origin {
    carry: Vec<String>,
    /// The user's explicit `--model`, else the settings' default model.
    launch_model: Option<String>,
    explicit_model: bool,
    effort: Option<String>,
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
    let here = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let explicit = flag_value(&parsed, &["--model"]).map(String::from);
    let origin = Origin {
        carry: carry_args(&parsed),
        launch_model: explicit.clone().or_else(|| settings_model(&here)),
        explicit_model: explicit.is_some(),
        effort: flag_value(&parsed, &["--effort"]).map(String::from),
    };
    let mut rearm = 0usize;
    let mut next_args: Vec<String> = claude_args.clone();

    // A resumed session opens as it is, however big: compaction only ever follows a warning.
    let (auto, _) = auto_for(
        &json!({"auto": l.auto}),
        flag_value(&parsed, &["-r", "--resume"]),
    );
    say(if auto {
        "auto-compaction on for this session · /recompact off to turn it off"
    } else {
        "auto-compaction off · /recompact on turns it on for this session"
    });

    let mut cycles = 0usize;
    loop {
        cycles += 1;
        for f in ["request.json", "nudged.json", "session.json", "warned.json"] {
            let _ = fs::remove_file(state.join(f));
        }
        let config = |child: u32| {
            json!({
                "launcher": std::process::id(), "child": child, "at": l.at,
                "checkpoint_at": l.checkpoint_at, "auto": l.auto, "rearm": rearm,
                "summarize": copts.summarize.is_some(),
                "summarize_with": copts.summarize.as_ref().map(|c| c.model.clone()),
                "target": copts.target, "launch_model": origin.launch_model,
            })
        };
        write_json(&state.join("config.json"), &config(0));
        let mut cmd = Command::new(&l.bin);
        cmd.args(&next_args).env("RECOMPACT_SHELL", &state);
        if let Some(d) = read_json(&state.join("start.json"))
            .and_then(|s| get_s(&s, "cwd").map(PathBuf::from))
            .filter(|d| d.is_dir())
        {
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
        write_json(&state.join("config.json"), &config(pid));
        let (code, req) = supervise(pid, &state);
        let session = read_json(&state.join("session.json"));
        // SIGTERM from anyone else (an agent running `kill -TERM $CLAUDE_PID`) is a handoff too.
        let req = req.or_else(|| {
            (code == 143).then(|| {
                let s = session.clone().unwrap_or(json!({}));
                json!({"session": s.get("session"), "transcript": s.get("transcript"),
                       "reason": "signal", "kick": false, "force": true})
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
        stop_prewarm(&state);
        let Some(transcript) = request_transcript(&req, &l) else {
            let known = get_s(&req, "session")
                .or_else(|| session.as_ref().and_then(|s| get_s(s, "session")))
                .map(String::from);
            next_args = origin.carry.clone();
            match known {
                Some(id) => {
                    say("could not find the session file; resuming it as it is");
                    next_args.extend(["--resume".to_string(), id]);
                }
                None => say("no session to compact yet; starting claude again"),
            }
            continue;
        };
        let at = user_at(get_s(&req, "session")).or(l.at).unwrap_or_else(|| {
            let (live, model) = live_status(&transcript).unwrap_or((0, String::new()));
            let model = if model.is_empty() {
                origin.launch_model.clone().unwrap_or_default()
            } else {
                model
            };
            default_at_for(&model, live)
        });
        let (twin, est) = match run_handoff(&req, &copts, at) {
            Some(v) => v,
            None => (get_s(&req, "session").unwrap_or("").to_string(), 0),
        };
        if est > 0 {
            rearm = rearm_for(est, at);
        }
        if let Some(from) = get_s(&req, "session") {
            carry_session_setting(from, &twin);
        }
        let kick = req.get("kick") == Some(&json!(true));
        next_args = relaunch_args(&origin, &twin, &transcript, kick.then_some(l.kick.as_str()));
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

/// `claude <carried flags> --resume <twin> <model> <effort> [kick]`. The model stays the one the
/// user launched with (an alias like `opus` keeps its context window) unless the session
/// switched to another model family mid-way; effort follows the session, falling back to the
/// launch flag.
fn relaunch_args(
    origin: &Origin,
    twin: &str,
    transcript: &Path,
    kick: Option<&str>,
) -> Vec<String> {
    let dir = transcript.parent().unwrap_or(Path::new("."));
    let records = load_jsonl(&dir.join(format!("{twin}.jsonl")));
    let flags = resume_flags(&records);
    let value_of = |name: &str| {
        flags
            .iter()
            .position(|f| f == name)
            .and_then(|i| flags.get(i + 1).cloned())
    };
    let session_model = value_of("--model");
    let switched = match (&session_model, &origin.launch_model) {
        (Some(s), Some(l)) => family(s).is_some() && family(l).is_some() && family(s) != family(l),
        _ => false,
    };
    let model = if switched {
        session_model
    } else if origin.explicit_model {
        origin.launch_model.clone()
    } else {
        None
    };
    let effort = value_of("--effort").or_else(|| origin.effort.clone());
    let mut out: Vec<String> = origin.carry.clone();
    out.push("--resume".into());
    out.push(twin.to_string());
    if let Some(m) = model {
        out.push("--model".into());
        out.push(m);
    }
    if let Some(e) = effort {
        out.push("--effort".into());
        out.push(e);
    }
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
        // A Ctrl-C that reached the launcher while claude ran is claude's business.
        CANCEL.store(false, Ordering::SeqCst);
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
/// it or the user pressed Ctrl-C) and the twin's estimated size (0 when there is no new twin).
fn run_handoff(req: &Value, copts: &ContinueOpts, at: usize) -> Option<(String, usize)> {
    let id = get_s(req, "session")?.to_string();
    let transcript = PathBuf::from(get_s(req, "transcript")?);
    let dir = transcript.parent()?.to_path_buf();
    let live = live_tokens(&transcript).unwrap_or(0);
    let mut o = copts.clone();
    o.threshold = at;
    o.force = req.get("force") == Some(&json!(true));
    let target = o.target.unwrap_or_else(|| default_target(at));
    o.target = Some(if o.force {
        forced_target(target, &dir, &id)
    } else {
        target
    });
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
    let before = lineage_latest(&dir, &id);
    CANCEL.store(false, Ordering::SeqCst);
    let t0 = Instant::now();
    let (next, rc) = continue_session(&dir, &id, &o);
    if CANCEL.swap(false, Ordering::SeqCst) {
        // Whatever this run produced is discarded; the session resumes exactly as it was.
        let after = lineage_latest(&dir, &id);
        if after != before {
            let _ = fs::remove_file(dir.join(format!("{after}.jsonl")));
            lineage_remove(&dir, &after);
        }
        say(&format!(
            "cancelled; resuming {} as it was",
            short_id(&before)
        ));
        return Some((before, 0));
    }
    if next == before || rc != 0 {
        say(&format!(
            "no compaction (exit {rc}); resuming {}",
            short_id(&next)
        ));
        return Some((next, 0));
    }
    let (active, _) = select_active(load_jsonl(&dir.join(format!("{next}.jsonl"))));
    let est = calibrate_lineage(&active).context_tokens(&active);
    say(&format!(
        "{} → ~{} in {:.0}s; resuming {}",
        fmt_k(live),
        fmt_k(est),
        t0.elapsed().as_secs_f32(),
        short_id(&next)
    ));
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

// ------------------------------------------------------------------------------------ setup

const BLOCK_START: &str = "# >>> recompact >>>";
const BLOCK_END: &str = "# <<< recompact <<<";

fn recompact_home() -> PathBuf {
    std::env::var("RECOMPACT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".claude").join("recompact"))
}

/// The shell startup file `install` writes to, from `$SHELL`.
fn rc_file() -> Result<PathBuf, String> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let name = Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    match name {
        "zsh" => Ok(home().join(".zshrc")),
        "bash" => {
            // macOS terminals start login shells, which read .bash_profile.
            let profile = home().join(".bash_profile");
            if cfg!(target_os = "macos") && profile.exists() {
                Ok(profile)
            } else {
                Ok(home().join(".bashrc"))
            }
        }
        other => Err(format!(
            "shell `{other}` is not supported by `recompact install`; define `claude` to run \
`{} shell \"$@\"` yourself",
            recompact_home().join("bin").join("recompact").display()
        )),
    }
}

/// The shell block `install` writes: interactive `claude` runs through the launcher, and falls
/// back to plain claude whenever the launcher is missing.
pub fn shell_block(launcher: &Path) -> String {
    // `$HOME/...` keeps the block valid for a dotfiles repo shared across machines.
    let l = match launcher.strip_prefix(home()) {
        Ok(rel) => format!("$HOME/{}", rel.display()),
        Err(_) => launcher.display().to_string(),
    };
    [
        BLOCK_START,
        "# Interactive claude runs through recompact: /recompact and large contexts compact in place.",
        "# Remove this block (or run `recompact uninstall`) to undo.",
        "claude() {",
        &format!("  if [ -x \"{l}\" ]; then"),
        &format!("    \"{l}\" shell \"$@\""),
        "  else",
        "    command claude \"$@\"",
        "  fi",
        "}",
        BLOCK_END,
        "",
    ]
    .join("\n")
}

/// Remove the managed block; `None` when there is none.
pub fn strip_block(text: &str) -> Option<String> {
    let start = text.find(BLOCK_START)?;
    let end = text[start..].find(BLOCK_END)? + start + BLOCK_END.len();
    let end = if text[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };
    let mut out = text[..start].trim_end_matches('\n').to_string();
    let rest = text[end..].trim_start_matches('\n');
    if !rest.is_empty() {
        out.push_str("\n\n");
        out.push_str(rest);
    }
    out.push('\n');
    Some(out)
}

/// Add or replace the managed block. Refuses when the file defines `claude` some other way.
pub fn with_block(text: &str, block: &str) -> Result<String, String> {
    let base = strip_block(text).unwrap_or_else(|| text.to_string());
    let defines_claude = base.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("claude()")
            || t.starts_with("claude ()")
            || t.starts_with("function claude")
            || t.starts_with("alias claude=")
    });
    if defines_claude {
        return Err("it already defines `claude`; remove that definition first".into());
    }
    let mut out = base.trim_end_matches('\n').to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    Ok(out)
}

/// `recompact install [--rc <file>]`: make interactive `claude` run through the launcher.
pub fn cmd_install(args: &[String]) -> i32 {
    let (_, opts) = parse_opts(args);
    let rc = match opts.get("rc").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => match rc_file() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("install: {e}");
                return 1;
            }
        },
    };
    let launcher = recompact_home().join("bin").join("recompact");
    let text = fs::read_to_string(&rc).unwrap_or_default();
    let updated = match with_block(&text, &shell_block(&launcher)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("install: not changing {}: {e}", rc.display());
            return 1;
        }
    };
    if updated == text {
        println!("recompact is already set up in {}.", rc.display());
        return 0;
    }
    if !text.is_empty() {
        let _ = fs::write(rc.with_extension("recompact-backup"), &text);
    }
    if let Err(e) = fs::write(&rc, &updated) {
        eprintln!("install: cannot write {}: {e}", rc.display());
        return 1;
    }
    println!(
        "Set up in {}. Open a new terminal (or `source {}`), then start claude as usual: \
/recompact and large contexts now compact in place. Undo with `recompact uninstall`.",
        rc.display(),
        rc.display()
    );
    0
}

/// `recompact uninstall [--rc <file>]`: remove what `install` added.
pub fn cmd_uninstall(args: &[String]) -> i32 {
    let (_, opts) = parse_opts(args);
    let rc = match opts.get("rc").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        None => match rc_file() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("uninstall: {e}");
                return 1;
            }
        },
    };
    let text = fs::read_to_string(&rc).unwrap_or_default();
    match strip_block(&text) {
        Some(t) => {
            if let Err(e) = fs::write(&rc, t) {
                eprintln!("uninstall: cannot write {}: {e}", rc.display());
                return 1;
            }
            println!(
                "Removed from {}. New terminals run plain claude.",
                rc.display()
            );
        }
        None => println!("Nothing to remove in {}.", rc.display()),
    }
    0
}

// ------------------------------------------------------------------------------------ switch

/// Where a session's auto-compaction setting comes from, strongest first.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AutoSource {
    /// `/recompact on|off` in that session (or an earlier session of its lineage).
    Session,
    /// `claude --auto` / `--no-auto` for this launcher.
    Launch,
    /// `/recompact default on|off`; off unless set.
    Default,
}

fn settings_path() -> PathBuf {
    recompact_home().join("settings.json")
}

/// Defaults for new sessions, `~/.claude/recompact/settings.json`: `auto` (off unless set) and
/// optionally `at`.
pub fn user_settings() -> Value {
    read_json(&settings_path()).unwrap_or(json!({}))
}

fn session_setting_path(id: &str) -> PathBuf {
    recompact_home().join("sessions").join(format!("{id}.json"))
}

fn session_setting(id: &str) -> Value {
    read_json(&session_setting_path(id)).unwrap_or(json!({}))
}

/// Is auto-compaction on for this session, and on whose say-so.
pub fn auto_for(config: &Value, session: Option<&str>) -> (bool, AutoSource) {
    if let Some(b) =
        session.and_then(|id| session_setting(id).get("auto").and_then(|v| v.as_bool()))
    {
        return (b, AutoSource::Session);
    }
    if let Some(b) = config.get("auto").and_then(|v| v.as_bool()) {
        return (b, AutoSource::Launch);
    }
    (
        user_settings()
            .get("auto")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        AutoSource::Default,
    )
}

fn auto_on(config: &Value, session: Option<&str>) -> bool {
    auto_for(config, session).0
}

fn user_at(session: Option<&str>) -> Option<usize> {
    session
        .and_then(|id| get_u(&session_setting(id), "at"))
        .or_else(|| get_u(&user_settings(), "at"))
}

/// A handoff's twin inherits its parent's setting: a session switched on stays on.
fn carry_session_setting(from: &str, to: &str) {
    if from != to {
        if let Some(v) = read_json(&session_setting_path(from)) {
            let _ = fs::create_dir_all(recompact_home().join("sessions"));
            write_json(&session_setting_path(to), &v);
        }
    }
}

/// `300k`, `1m`, `400000`.
pub fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim().to_lowercase().replace('_', "");
    let (num, mult) = if let Some(n) = s.strip_suffix('k') {
        (n, 1_000.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1_000_000.0)
    } else {
        (s.as_str(), 1.0)
    };
    let v = num.parse::<f64>().ok()? * mult;
    (v >= 10_000.0).then_some(v as usize)
}

/// `/recompact on [size]`, `/recompact off`, `/recompact status`, `/recompact default on|off`.
fn switch_command(prompt: &str) -> Option<(&str, Option<&str>)> {
    let p = prompt.trim();
    let rest = p
        .strip_prefix("/segment-recompact:recompact")
        .or_else(|| p.strip_prefix("/recompact"))?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut words = rest.split_whitespace();
    let cmd = words.next()?;
    let arg = words.next();
    if words.next().is_some() {
        return None;
    }
    let ok = match (cmd, arg) {
        ("on" | "off" | "status", None) => true,
        ("on", Some(a)) => parse_size(a).is_some(),
        ("default", Some("on" | "off")) => true,
        _ => false,
    };
    ok.then_some((cmd, arg))
}

/// Apply a switch command for `session` and describe the result in one line.
pub fn switch(
    cmd: &str,
    arg: Option<&str>,
    session: Option<&str>,
    shell: Option<&Shell>,
    transcript: Option<&Path>,
    managed: bool,
) -> String {
    match (cmd, session) {
        ("on" | "off", Some(id)) => {
            let mut v = session_setting(id);
            v["auto"] = json!(cmd == "on");
            if let Some(at) = arg.and_then(parse_size) {
                v["at"] = json!(at);
            }
            let _ = fs::create_dir_all(recompact_home().join("sessions"));
            write_json(&session_setting_path(id), &v);
        }
        ("default", _) => {
            let mut v = user_settings();
            v["auto"] = json!(arg == Some("on"));
            let _ = fs::create_dir_all(recompact_home());
            write_json(&settings_path(), &v);
        }
        _ => {}
    }
    let config = shell.map(|s| s.config.clone()).unwrap_or(json!({}));
    let (on, source) = auto_for(&config, session);
    let default_on = user_settings()
        .get("auto")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let live = transcript.and_then(live_status);
    let at = match (shell, session, &live) {
        (Some(sh), Some(id), Some((t, m))) => thresholds(sh, id, m, *t).0,
        (_, _, Some((t, m))) => user_at(session).unwrap_or_else(|| default_at_for(m, *t)),
        _ => user_at(session).unwrap_or(400_000),
    };
    let now = live
        .as_ref()
        .map(|(t, _)| format!(", now {}", fmt_k(*t)))
        .unwrap_or_default();
    let new_sessions = format!(
        "New sessions start {} (/recompact default {} changes that).",
        if default_on { "ON" } else { "OFF" },
        if default_on { "off" } else { "on" }
    );
    let mut text = match (cmd, session.is_some()) {
        ("default", _) => format!(
            "recompact: new sessions now start with auto-compaction {}. {}",
            if default_on { "ON" } else { "OFF" },
            if session.is_some() {
                format!("This session is {}.", if on { "ON" } else { "OFF" })
            } else {
                String::new()
            }
        ),
        (_, false) => format!(
            "recompact: no session here; `recompact auto on --session <id>` names one. {new_sessions}"
        ),
        ("on", true) => format!(
            "recompact: auto-compaction is ON for this session and its continuations (at {}{now}). \
When a turn ends past that size it compacts in place and carries on; long autonomous turns are asked \
to checkpoint first. /recompact off turns it off.",
            fmt_k(at)
        ),
        ("off", true) => format!(
            "recompact: auto-compaction is OFF for this session{now}. /recompact still compacts by hand; \
/recompact on turns it back on."
        ),
        _ => format!(
            "recompact: this session is {}{} (at {}{now}). {new_sessions}",
            if on { "ON" } else { "OFF" },
            match source {
                AutoSource::Session => "",
                AutoSource::Launch => ", from `claude --auto`/`--no-auto`",
                AutoSource::Default => ", the default",
            },
            fmt_k(at)
        ),
    };
    if on && !managed {
        text.push_str(
            " This claude was not started through recompact, so it cannot compact in place: run \
/recompact setup once, then open a new terminal.",
        );
    }
    text.trim_end().to_string()
}

/// `recompact auto [on [size] | off | status] [--session <id>]`, `recompact auto default on|off`:
/// the switch from a shell. Inside claude the session is the current one.
pub fn cmd_auto(args: &[String]) -> i32 {
    let (pos, opts) = parse_opts(args);
    let cmd = pos.first().map(String::as_str).unwrap_or("status");
    let arg = pos.get(1).map(String::as_str);
    let valid = match (cmd, arg) {
        ("on" | "off" | "status", None) => true,
        ("on", Some(a)) => parse_size(a).is_some(),
        ("default", Some("on" | "off")) => true,
        _ => false,
    };
    if !valid {
        eprintln!(
            "usage: recompact auto [on [300k] | off | status] [--session <id>]\n       recompact auto default on|off"
        );
        return 2;
    }
    let shell = Shell::from_env();
    let session = opts
        .get("session")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok());
    let inside = std::env::var("CLAUDE_CODE_SESSION_ID").is_ok();
    let managed = !inside || shell.as_ref().is_some_and(|s| s.is_own_claude());
    let transcript = session.as_deref().and_then(|id| locate_session(None, id));
    println!(
        "{}",
        switch(
            cmd,
            arg,
            session.as_deref(),
            shell.as_ref(),
            transcript.as_deref(),
            managed
        )
    );
    0
}
