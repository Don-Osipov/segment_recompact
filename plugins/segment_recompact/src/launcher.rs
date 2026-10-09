//! Handoff in place: compaction that happens in the terminal you are already working in.
//!
//! `recompact shell` runs `claude` as its child and otherwise stays out of the way. Hooks inside
//! that claude report which session is live and ask for a handoff: the user typing a bare
//! `/recompact`, a turn ending with the context over the threshold, or the agent queueing one
//! itself (`recompact handoff`).
//!
//! In a terminal, claude runs on a pseudo-terminal the launcher owns (`term`). A handoff then
//! happens in place: the launcher compacts in the background while claude keeps running, waits
//! for a pause (turn over, input box empty), and types `/resume <twin>` into it. Claude Code
//! switches sessions in the same process, so background shells, monitors, agents and scheduled
//! prompts keep running and their notices reach the twin (measured, CLI 2.1.286).
//!
//! Without that terminal, or when the switch does not take, the launcher stops claude with
//! SIGTERM (Claude Code exits cleanly on it: terminal restored, transcript flushed, exit 143),
//! compacts with progress on the terminal, and resumes the twin with the same flags.
//!
//! With `RECOMPACT_JOBS=1` (off by default) the launcher starts claude as a background job
//! instead and attaches this terminal to it with `claude attach`; the job path below then handles
//! compaction, and the terminal's environment reaches the job through its `--settings` file.
//!
//! A background job (`claude --bg`, agent view) never runs through the launcher: Claude Code's
//! daemon starts it on a terminal of its own. Its hooks find it from `CLAUDE_JOB_DIR` and keep
//! the same files in `~/.claude/recompact/jobs/<short id>`, and start `recompact job-handoff`,
//! which compacts, waits for the job to pause, opens it with `claude attach` on a terminal of its
//! own, and types `/resume <twin>` there (measured, CLI 2.1.293: the job switches in place and
//! keeps its short id and background work). When that does not take, the twin starts as a new
//! job (`claude --bg --resume`) and the old one is stopped.
//!
//! The launcher exports `RECOMPACT_SHELL=<state dir>`. Each file there has one writer, so no
//! process read-modify-writes another's state:
//! - `config.json`: the launcher (its child's pid, thresholds, the re-arm floor, in-place mode)
//! - `session.json`: the SessionStart hook of the launcher's own claude (live session)
//! - `start.json`: the first SessionStart of this launcher (the directory claude started in)
//! - `request.json`: hooks and `recompact handoff` (which session, why, whether to continue)
//! - `handoff.json`: the launcher (an in-place handoff is under way)
//! - `switch.json`: the launcher (the twin it is typing `/resume` for; the SessionStart hook
//!   consumes it)
//! - `child.json`: `recompact pty-leader` (claude's pid, when claude runs under it)
//! - `nudged.json`: the PostToolUse hook (the last checkpoint request)
//! - `prewarm.json`: the Stop hook (the running background prewarm)
//! - `worker.json`: `recompact job-handoff` (a background job's handoff worker)

use std::fs;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::term::{InputBox, Proxy};
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

/// The launcher this process runs under (if any, and still alive), or the background job it
/// runs in.
pub struct Shell {
    pub dir: PathBuf,
    pub config: Value,
}

impl Shell {
    pub fn from_env() -> Option<Shell> {
        let launcher = Shell::launcher_from_env();
        if launcher.as_ref().is_some_and(|s| s.is_own_claude()) {
            return launcher;
        }
        Shell::job_from_env().or(launcher)
    }

    fn launcher_from_env() -> Option<Shell> {
        let dir = PathBuf::from(std::env::var("RECOMPACT_SHELL").ok()?);
        let config = read_json(&dir.join("config.json"))?;
        let launcher = get_u(&config, "launcher")? as u32;
        if !pid_alive(launcher) {
            return None;
        }
        Some(Shell { dir, config })
    }

    /// The background job this process runs in, when it runs inside the job's own claude.
    fn job_from_env() -> Option<Shell> {
        let job = Job::from_env()?;
        let pid = std::env::var("CLAUDE_PID").ok()?.parse::<u32>().ok()?;
        runs_job(&home().join(".claude").join("sessions"), pid, &job.short)
            .then(|| job_shell(&job, pid))
    }

    /// The short id of the background job this is the state of.
    pub fn job(&self) -> Option<&str> {
        get_s(&self.config, "job")
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

/// The session this process runs inside. Under the launcher that is the one its claude has open
/// now: CLAUDE_CODE_SESSION_ID keeps the id claude started with, even after a switch in place.
fn own_session() -> Option<String> {
    Shell::from_env()
        .filter(|s| s.is_own_claude())
        .and_then(|s| s.session())
        .and_then(|s| get_s(&s, "session").map(String::from))
        .or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())
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

// ------------------------------------------------------------------------------------ jobs

/// A Claude Code background job (`claude --bg`, agent view). Its folder (`CLAUDE_JOB_DIR`) holds
/// the daemon's `state.json`: the short id, whether a turn runs (`tempo`), queued prompts, the
/// directory, and the flags it restarts with (measured, CLI 2.1.293).
pub struct Job {
    pub dir: PathBuf,
    pub short: String,
}

impl Job {
    pub fn at(dir: &Path) -> Option<Job> {
        let state = read_json(&dir.join("state.json"))?;
        if get_s(&state, "backend") != Some("daemon") {
            return None;
        }
        let short = get_s(&state, "daemonShort")
            .map(String::from)
            .or_else(|| dir.file_name()?.to_str().map(String::from))?;
        // It names a folder and goes on command lines.
        (!short.is_empty() && short.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(
            || Job {
                dir: dir.to_path_buf(),
                short,
            },
        )
    }

    pub fn from_env() -> Option<Job> {
        Job::at(Path::new(&std::env::var_os("CLAUDE_JOB_DIR")?))
    }

    pub fn state(&self) -> Value {
        read_json(&self.dir.join("state.json")).unwrap_or(json!({}))
    }
}

/// Is `pid` the job's own claude, rather than a claude started inside it that inherited
/// `CLAUDE_JOB_DIR`? Claude Code records each running session in `sessions/<pid>.json`, with the
/// job's short id only for the job itself (measured: a nested `claude -p` has none).
pub fn runs_job(sessions: &Path, pid: u32, short: &str) -> bool {
    read_json(&sessions.join(format!("{pid}.json")))
        .is_some_and(|s| get_s(&s, "jobId") == Some(short))
}

pub fn job_state_dir(short: &str) -> PathBuf {
    recompact_home().join("jobs").join(short)
}

/// The state a job's hooks and its handoff worker share, as a `Shell` for the job's claude
/// `pid`. Only the re-arm floor persists (`config.json`, written by the worker); the rest is the
/// launcher's defaults.
pub fn job_shell(job: &Job, pid: u32) -> Shell {
    let dir = job_state_dir(&job.short);
    if !dir.exists() {
        prune_job_states(job);
        let _ = fs::create_dir_all(&dir);
    }
    let env_usize = |k: &str| std::env::var(k).ok().and_then(|s| s.parse::<usize>().ok());
    let copts = continue_opts(&serde_json::Map::new(), Some("haiku"));
    let mut config = json!({
        "job": job.short, "job_dir": job.dir, "child": pid, "inplace": true,
        "at": env_usize("RECOMPACT_AT"), "checkpoint_at": env_usize("RECOMPACT_CHECKPOINT_AT"),
        "auto": std::env::var("RECOMPACT_AUTO").ok().map(|v| v != "0"),
        "summarize": copts.summarize.is_some(),
        "summarize_with": copts.summarize.as_ref().map(|c| c.model.clone()),
        "target": copts.target,
        "launch_model": job_origin(job).launch_model,
    });
    if let Some(Value::Object(saved)) = read_json(&dir.join("config.json")) {
        for (k, v) in saved {
            config[k] = v;
        }
    }
    Shell { dir, config }
}

/// Dropped from a job's restart flags: the restart picks the session, model and effort itself,
/// and the job's directory already exists.
const JOB_SESSION_FLAGS: &[&str] = &[
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
    "--bg",
    "--background",
    "-p",
    "--print",
];

/// How a job restarts: the flags the daemon restarts it with (`respawnFlags`), less the
/// session-selecting ones and its first prompt. Unlike a restart under the launcher, permission
/// flags stay as they were: a background job may have no one to answer a permission prompt.
/// Flags this cannot classify are all left out rather than risk mangling them.
fn job_origin(job: &Job) -> Origin {
    let state = job.state();
    let flags: Vec<String> = state
        .get("respawnFlags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let parsed = parse_claude_args(&flags);
    let known = parsed
        .iter()
        .all(|a| a.flag.as_deref().is_none_or(known_flag));
    let kept: Vec<Arg> = parsed
        .iter()
        .filter(|a| {
            known
                && a.flag
                    .as_deref()
                    .is_some_and(|f| !JOB_SESSION_FLAGS.contains(&f))
        })
        .cloned()
        .collect();
    let explicit = flag_value(&parsed, &["--model"]).map(String::from);
    let cwd = get_s(&state, "cwd").map(PathBuf::from).unwrap_or_default();
    Origin {
        carry: flatten(&kept),
        launch_model: explicit.clone().or_else(|| settings_model(&cwd)),
        explicit_model: explicit.is_some(),
        effort: flag_value(&parsed, &["--effort"]).map(String::from),
    }
}

/// A claude the job's worker runs is a client of its own, not part of the job's session: it
/// must not look like the job's claude or a child of it.
fn plain_claude(cmd: &mut Command) {
    for k in [
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ATTENDED",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_EXECPATH",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_PID",
        "CLAUDE_JOB_DIR",
        "RECOMPACT_SHELL",
    ] {
        cmd.env_remove(k);
    }
}

/// A one-time line for whoever next opens or finishes a turn in `session` (SessionStart or Stop).
fn notice_path(session: &str) -> PathBuf {
    let name: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    recompact_home()
        .join("notices")
        .join(format!("{name}.json"))
}

fn leave_notice(session: &str, message: &str) {
    let path = notice_path(session);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    write_json(&path, &json!({"session": session, "message": message}));
}

pub fn take_notice(session: &str) -> Option<String> {
    let path = notice_path(session);
    let n = read_json(&path)?;
    if get_s(&n, "session") != Some(session) {
        return None;
    }
    let _ = fs::remove_file(&path);
    get_s(&n, "message").map(String::from)
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
/// `RECOMPACT_WINDOW`, usage already past 200k, else Haiku 4.5 and older at 200k and everything
/// else at 1M (Claude Code gives Haiku 5.5 a 1M window).
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
    if crate::is_haiku_4_or_older(model) {
        200_000
    } else {
        1_000_000
    }
}

/// Where automatic handoff fires: with room to work on a 1M window, and on a 200k one well
/// before Claude Code's own compaction (~167k).
pub fn default_at(window: usize) -> usize {
    if window >= 1_000_000 {
        500_000
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

/// The session's transcript. The hook's `transcript_path` can name a file that does not exist:
/// `claude --worktree <name> --resume <id>` reports the worktree's project folder while the
/// session is still written in the folder it started in.
fn hook_transcript(input: &Value) -> Option<PathBuf> {
    let given = get_s(input, "transcript_path").map(PathBuf::from);
    if given.as_ref().is_some_and(|p| p.exists()) {
        return given;
    }
    hook_session(input)
        .and_then(|id| locate_session(given.as_deref(), id))
        .or(given)
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
    write_session(&shell, input);
    let start = shell.dir.join("start.json");
    if !start.exists() {
        write_json(&start, &json!({"cwd": get_s(input, "cwd")}));
    }
}

fn write_session(shell: &Shell, input: &Value) {
    let Some(session) = hook_session(input) else {
        return;
    };
    write_json(
        &shell.dir.join("session.json"),
        &json!({
            "session": session,
            "transcript": hook_transcript(input),
            "model": get_s(input, "model"),
            "source": get_s(input, "source"),
            "start_tokens": hook_transcript(input)
                .and_then(|t| live_status(&t))
                .map(|(t, _)| t),
        }),
    );
}

/// A job whose claude started before recompact's hooks were on has no record of its session
/// yet; the session its hooks report is the one it has open.
fn track_job(shell: &Shell, input: &Value) {
    if shell.job().is_some() && shell.session().is_none() {
        write_session(shell, input);
    }
}

fn file_len(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// A prompt, a task notice or a reply in the main conversation. Hook output, queue bookkeeping,
/// titles and subagent records are not.
fn turn_record(r: &Value) -> bool {
    matches!(rec_type(r), "user" | "assistant") && !truthy(r, "isSidechain")
}

/// The last `max` bytes of a file, from the first whole line in them.
fn read_tail(path: &Path, max: u64) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Some(match (start, text.find('\n')) {
        (0, _) | (_, None) => text,
        (_, Some(i)) => text[i + 1..].to_string(),
    })
}

/// Is the session between turns? After a turn, Claude Code writes its bookkeeping
/// (`turn_duration`, `stop_hook_summary`), and it writes the turn's own last records after the
/// Stop hooks run, so only the transcript can tell, not a hook.
pub fn turn_ended(transcript: &Path) -> bool {
    // Read often while a switch waits: a small tail answers almost always.
    for window in [64u64 << 10, 4 << 20] {
        let Some(tail) = read_tail(transcript, window) else {
            return true;
        };
        for line in tail.lines().rev() {
            let Ok(r) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if rec_type(&r) == "system"
                && matches!(
                    get_s(&r, "subtype"),
                    Some("turn_duration" | "stop_hook_summary")
                )
            {
                return true;
            }
            if turn_record(&r) {
                return false;
            }
        }
        if file_len(transcript) <= window {
            break;
        }
    }
    true
}

/// A tool call in the current turn has no result yet: claude is running it, or a dialog for it
/// (a permission prompt, a question) is waiting for the user.
pub fn tool_call_waiting(transcript: &Path) -> bool {
    let Some(tail) = read_tail(transcript, 4 << 20) else {
        return false;
    };
    let mut open: Vec<String> = Vec::new();
    for line in tail.lines() {
        let Ok(r) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if rec_type(&r) == "system"
            && matches!(
                get_s(&r, "subtype"),
                Some("turn_duration" | "stop_hook_summary")
            )
        {
            open.clear();
            continue;
        }
        if !turn_record(&r) {
            continue;
        }
        for b in r
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match get_s(b, "type") {
                Some("tool_use") => open.extend(get_s(b, "id").map(String::from)),
                Some("tool_result") => {
                    let id = get_s(b, "tool_use_id");
                    open.retain(|o| Some(o.as_str()) != id);
                }
                _ => {}
            }
        }
    }
    !open.is_empty()
}

enum Idle {
    Yes,
    Busy,
    /// Between steps of a turn, waiting for the user to answer a dialog.
    Asking,
}

/// Claude Code keeps the spinner in its title while a background agent runs, also after the turn
/// has ended and the prompt is free (measured, CLI 2.1.294). A turn writes its prompt to the
/// transcript as it starts, so a title busy this long over an ended turn, in a transcript this
/// quiet, is a pause.
const BUSY_TITLE_PAUSE: Duration = Duration::from_secs(10);

fn paused_under_busy_title(p: &Proxy, transcript: &Path) -> bool {
    let quiet = fs::metadata(transcript)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or(Duration::ZERO);
    p.title_busy_for().is_some_and(|d| d >= BUSY_TITLE_PAUSE)
        && quiet >= BUSY_TITLE_PAUSE
        && turn_ended(transcript)
}

/// Is claude between turns, with nothing waiting on the user? Claude Code's terminal title says
/// whether it is working, except while background agents run (see `BUSY_TITLE_PAUSE`); a dialog
/// shows the idle title too, so a tool call without a result means it is asking. A prompt whose
/// query was dropped (a hook blocked its batch) leaves a turn open in the transcript, so the
/// transcript alone is only the fallback, for when Claude Code sets no title.
fn claude_idle(p: &Proxy, transcript: &Path, r: &mut Ready) -> Idle {
    match p.title_idle_for() {
        Some(d) if d < SETTLE && !paused_under_busy_title(p, transcript) => Idle::Busy,
        Some(_)
            if *r
                .waiting
                .get_or_insert_with(|| tool_call_waiting(transcript)) =>
        {
            Idle::Asking
        }
        Some(_) => Idle::Yes,
        None if turn_ended(transcript) => Idle::Yes,
        None => Idle::Busy,
    }
}

/// Is a background job at a pause? Its `state.json` says whether a turn runs: `tempo` is
/// `active` during one, `idle` after it, and `blocked` after a turn that ended asking the user
/// something. `state` stays `working` while a background task runs after the turn ended, and
/// `inFlight.queued` can stay 1 while a job waits for the user, so neither says anything about
/// turns (measured, CLI 2.1.293). A queued prompt that does start writes to the transcript
/// within the settle time. Any other `tempo`: the transcript tells.
fn job_busy(state: &Value, transcript: &Path) -> bool {
    match get_s(state, "tempo") {
        Some("active") => true,
        Some("idle" | "blocked") => false,
        _ => !turn_ended(transcript),
    }
}

fn job_wait(state: &Value, transcript: &Path, r: &mut Ready) -> &'static str {
    if job_busy(state, transcript) {
        "turn"
    } else if *r
        .waiting
        .get_or_insert_with(|| tool_call_waiting(transcript))
    {
        "answer"
    } else {
        "quiet"
    }
}

/// Whether claude's input box can be taken for typing.
enum Take {
    Yes,
    /// Not now: it holds text.
    NotNow,
    /// Claude cannot be reached to type into; the switch falls back to a restart.
    Never,
}

/// The claude a switch in place types into: on the launcher's own terminal, or a background job
/// opened with `claude attach`.
trait Term {
    /// What the switch waits for: `turn`, `answer`, `typing`, `draft`, `unknown`, or `quiet` at
    /// a pause it may type at.
    fn wait(&self, transcript: &Path, r: &mut Ready) -> &'static str;
    /// No key has reached claude for a while.
    fn settled(&self) -> bool;
    /// Make sure the input box is empty and stays so while recompact types.
    fn take(&mut self) -> Take;
    /// Give the input box back.
    fn give_back(&mut self);
    fn type_resume(&mut self, twin: &str);
    fn type_prompt(&mut self, text: &str);
}

/// Type `/resume <twin>` and Enter. Esc first closes a picker or dialog left open (at the prompt
/// it does nothing); the pause after it keeps Esc from reading as Alt with the next key.
fn type_resume_with(keys: &dyn Fn(&[u8]) -> bool, twin: &str, esc: bool) {
    if esc {
        keys(b"\x1b");
        std::thread::sleep(Duration::from_millis(800));
    }
    keys(format!("/resume {twin}").as_bytes());
    std::thread::sleep(Duration::from_millis(500));
    keys(b"\r");
}

/// Type a prompt into the twin once its conversation has finished drawing.
fn type_prompt_with(keys: &dyn Fn(&[u8]) -> bool, text: &str) {
    std::thread::sleep(Duration::from_millis(1500));
    keys(text.as_bytes());
    std::thread::sleep(Duration::from_millis(500));
    keys(b"\r");
}

/// Claude on the launcher's terminal: the user's keys pass through it, so the input box is
/// followed and keys can be held.
struct Own<'a>(&'a Proxy);

impl Term for Own<'_> {
    fn wait(&self, transcript: &Path, r: &mut Ready) -> &'static str {
        match claude_idle(self.0, transcript, r) {
            Idle::Busy => "turn",
            Idle::Asking => "answer",
            Idle::Yes if self.0.input_clean() => "quiet",
            Idle::Yes => match self.0.input_box() {
                InputBox::Unknown => "unknown",
                InputBox::Empty => "typing",
                InputBox::Text(_) | InputBox::Draft => "draft",
            },
        }
    }

    fn settled(&self) -> bool {
        self.0.quiet_for() >= SETTLE
    }

    fn take(&mut self) -> Take {
        self.0.hold();
        if self.0.input_clean() {
            Take::Yes
        } else {
            self.0.release();
            Take::NotNow
        }
    }

    fn give_back(&mut self) {
        self.0.release();
    }

    fn type_resume(&mut self, twin: &str) {
        type_resume_with(&|b| self.0.type_keys(b), twin, true);
    }

    fn type_prompt(&mut self, text: &str) {
        type_prompt_with(&|b| self.0.type_keys(b), text);
    }
}

/// After a draft turned up, the job is left alone this long before it is checked again.
const DRAFT_BACKOFF: Duration = Duration::from_secs(10);
/// Two Escs closer than this would clear a draft (Claude Code: "Esc again to clear").
const ESC_GAP: Duration = Duration::from_secs(3);

/// A background job, opened with `claude attach` only for the switch itself. No keys pass
/// through recompact, so a draft in the job's input box shows only on its screen: Esc on a box
/// with text makes Claude Code say "Esc again to clear" (measured, CLI 2.1.293). A draft typed
/// after that check still cannot reach the model: the UserPromptSubmit hook blocks a prompt that
/// carries the `/resume` (`switch_collision`).
struct Attached<'a> {
    job: &'a Job,
    bin: &'a str,
    /// The job's claude, whose terminal size an attach takes.
    pid: u32,
    term: Option<crate::term::Attach>,
    esc_at: Option<Instant>,
    draft_at: Option<Instant>,
}

impl<'a> Attached<'a> {
    fn new(job: &'a Job, bin: &'a str, pid: u32) -> Attached<'a> {
        Attached {
            job,
            bin,
            pid,
            term: None,
            esc_at: None,
            draft_at: None,
        }
    }

    /// An Esc now neither clears a draft (a second Esc soon after the first) nor interrupts a turn
    /// that started since the last look (attaching takes a moment).
    fn esc_ok(&self) -> bool {
        self.esc_at.is_none_or(|at| at.elapsed() >= ESC_GAP)
            && get_s(&self.job.state(), "tempo") != Some("active")
    }

    /// Attached, or attach now (a second try after a pause: the daemon may be busy).
    fn ensure_attached(&mut self) -> bool {
        if !self.term.as_mut().is_some_and(|t| t.running()) {
            self.term = None;
        }
        for pause in [0u64, 2000] {
            if self.term.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(pause));
            self.term = attach(self.bin, &self.job.short, self.pid);
        }
        self.term.is_some()
    }
}

/// The terminal `pid` runs on, as `ps` names it.
fn tty_of(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|t| !t.is_empty())
}

/// Open a background job with `claude attach` on a terminal the size of the job's own (a person
/// attached to it sees no resize), and wait until it has drawn.
fn attach(bin: &str, short: &str, pid: u32) -> Option<crate::term::Attach> {
    let (rows, cols) = tty_of(pid)
        .and_then(|t| crate::term::tty_size(&t))
        .unwrap_or((50, 200));
    let mut cmd = Command::new(bin);
    cmd.args(["attach", short]);
    plain_claude(&mut cmd);
    let mut t = crate::term::Attach::open(&mut cmd, rows, cols).ok()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if !t.running() {
            return None;
        }
        match t.still_for() {
            Some(d) if d >= Duration::from_millis(700) => return Some(t),
            drawn if Instant::now() > deadline => return drawn.map(|_| t),
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Term for Attached<'_> {
    fn wait(&self, transcript: &Path, r: &mut Ready) -> &'static str {
        if self.draft_at.is_some_and(|t| t.elapsed() < DRAFT_BACKOFF) {
            return "draft";
        }
        job_wait(&self.job.state(), transcript, r)
    }

    fn settled(&self) -> bool {
        true
    }

    fn take(&mut self) -> Take {
        if !self.esc_ok() {
            return Take::NotNow;
        }
        if !self.ensure_attached() {
            return Take::Never;
        }
        if !self.esc_ok() {
            self.give_back();
            return Take::NotNow;
        }
        let Some(t) = self.term.as_ref() else {
            return Take::Never;
        };
        let mark = t.mark();
        t.type_keys(b"\x1b");
        self.esc_at = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(800));
        if t.text_since(mark).contains("Escagaintoclear") {
            self.draft_at = Some(Instant::now());
            self.give_back();
            return Take::NotNow;
        }
        Take::Yes
    }

    fn give_back(&mut self) {
        if let Some(t) = self.term.take() {
            t.detach();
        }
    }

    fn type_resume(&mut self, twin: &str) {
        if !self.ensure_attached() {
            return;
        }
        let esc = self.esc_ok();
        if esc {
            self.esc_at = Some(Instant::now());
        }
        if let Some(t) = self.term.as_ref() {
            type_resume_with(&|b| t.type_keys(b), twin, esc);
        }
    }

    fn type_prompt(&mut self, text: &str) {
        if !self.ensure_attached() {
            return;
        }
        if let Some(t) = self.term.as_ref() {
            type_prompt_with(&|b| t.type_keys(b), text);
        }
    }
}

/// Did the session add a prompt, a task notice or a reply after its record `uuid`? Unknown
/// counts as yes.
pub fn activity_after(transcript: &Path, uuid: &str) -> bool {
    let Ok(text) = fs::read_to_string(transcript) else {
        return true;
    };
    for line in text.lines().rev() {
        let Ok(r) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if get_s(&r, "uuid") == Some(uuid) {
            return false;
        }
        if turn_record(&r) {
            return true;
        }
    }
    true
}

/// The last prompt or reply a twin carries over from its source. The twin's own summary and
/// preamble records have ids of their own; carried records keep theirs.
pub fn last_carried_uuid(twin: &Path) -> Option<String> {
    load_jsonl(twin)
        .iter()
        .rev()
        .find(|r| {
            turn_record(r) && !truthy(r, "recompactSynthetic") && !truthy(r, "recompactPreamble")
        })
        .and_then(|r| get_s(r, "uuid").map(String::from))
}

fn in_place(shell: &Shell) -> bool {
    shell.config.get("inplace") == Some(&json!(true))
}

/// An in-place handoff for this session is already under way (in a process still running it).
fn in_flight(shell: &Shell, session: &str) -> bool {
    read_json(&shell.dir.join("handoff.json")).is_some_and(|h| {
        get_s(&h, "session") == Some(session)
            && get_u(&h, "pid").is_none_or(|p| pid_alive(p as u32))
    })
}

/// SessionStart of the twin a switch in place opened: a line for the user and, when work was
/// running, a note for the model that it still is.
pub fn in_place_notice(input: &Value) -> Option<(String, Option<String>)> {
    in_place_notice_in(Shell::from_env().filter(|s| s.is_own_claude()), input)
}

pub fn in_place_notice_in(shell: Option<Shell>, input: &Value) -> Option<(String, Option<String>)> {
    if get_s(input, "source") != Some("resume") {
        return None;
    }
    let shell = shell?;
    let path = shell.dir.join("switch.json");
    let sw = read_json(&path)?;
    if get_s(&sw, "twin") != hook_session(input) {
        return None;
    }
    let _ = fs::remove_file(&path);
    Some((
        get_s(&sw, "message")?.to_string(),
        get_s(&sw, "context").map(String::from),
    ))
}

/// A prompt that carries the `/resume <twin>` the launcher typed was not the switch: text the
/// user left in the input box was in the way. It must not reach the model.
fn switch_collision(shell: &Shell, prompt: &str) -> Option<String> {
    let sw = read_json(&shell.dir.join("switch.json"))?;
    let cmd = format!("resume {}", get_s(&sw, "twin")?);
    if !prompt.contains(&cmd) || prompt.trim() == format!("/{cmd}") {
        return None;
    }
    let left = prompt.replace(&format!("/{cmd}"), "").replace(&cmd, "");
    Some(format!(
        "recompact · not sent: unsent text was in the input box while this session switched to its \
compacted copy. It read: {}",
        left.trim()
    ))
}

fn is_bare_recompact(prompt: &str) -> bool {
    matches!(prompt.trim(), "/recompact" | "/segment-recompact:recompact")
}

fn request(shell: &Shell, input: &Value, reason: &str, kick: bool, force: bool, ready: bool) {
    write_json(
        &shell.dir.join("request.json"),
        &json!({
            "session": hook_session(input),
            "transcript": hook_transcript(input),
            "reason": reason,
            "kick": kick,
            "force": force,
            "ready": ready,
            "carry": carry_for(shell, hook_session(input)),
        }),
    );
    if ready {
        wake(shell);
    }
}

/// The launcher watches for requests itself; a background job has no one watching, so a ready
/// request starts its handoff worker (detached, like the prewarm), unless one runs already.
fn wake(shell: &Shell) {
    let (Some(dir), Some(pid)) = (
        get_s(&shell.config, "job_dir"),
        get_u(&shell.config, "child"),
    ) else {
        return;
    };
    let marker = shell.dir.join("worker.json");
    if marked_process(&marker, " job-handoff ").is_some() {
        return;
    }
    let Some(bin) = get_s(&shell.config, "worker_bin")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
    else {
        return;
    };
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(shell.dir.join("worker.log"));
    let mut cmd = Command::new(bin);
    cmd.arg("job-handoff")
        .arg(dir)
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log.map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
        .env_remove("RECOMPACT_SHELL");
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    if let Ok(child) = cmd.spawn() {
        write_json(&marker, &json!({"pid": child.id()}));
    }
}

// ------------------------------------------------------------------------------------ carry

/// Restarting claude ends its background shells and monitors and drops its scheduled wakeups
/// (measured: all processes gone after SIGTERM). The Stop hook sees them all, with commands and
/// schedules, so a handoff records them and the resumed session is asked to start them again.
fn remember_background(shell: &Shell, session: &str, input: &Value) {
    let tasks: Vec<Value> = input
        .get("background_tasks")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|t| get_s(t, "status").is_none_or(|s| s == "running"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let crons = input
        .get("session_crons")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    write_json(
        &shell.dir.join("background.json"),
        &json!({"session": session, "tasks": tasks, "crons": crons}),
    );
}

fn carry_for(shell: &Shell, session: Option<&str>) -> Value {
    carry_in(&shell.dir, session)
}

fn carry_in(dir: &Path, session: Option<&str>) -> Value {
    read_json(&dir.join("background.json"))
        .filter(|b| session.is_some() && get_s(b, "session") == session)
        .map(|b| json!({"tasks": b["tasks"], "crons": b["crons"]}))
        .unwrap_or(json!({"tasks": [], "crons": []}))
}

/// A background command with no natural end (a watcher or poll loop): waiting for it to finish
/// would mean never compacting, so it is restarted instead.
pub fn is_open_ended(task: &Value) -> bool {
    let cmd = get_s(task, "command").unwrap_or("").to_lowercase();
    let desc = get_s(task, "description").unwrap_or("").to_lowercase();
    [
        "while true",
        "while :",
        "tail -f",
        "tail -F",
        "--watch",
        "watch ",
        "sleep infinity",
        "until false",
    ]
    .iter()
    .any(|p| cmd.contains(&p.to_lowercase()))
        || ["monitor", "watch", "poll", "tail"]
            .iter()
            .any(|w| desc.contains(w))
}

fn carry_items(carry: &Value) -> Vec<String> {
    let mut items = Vec::new();
    for t in carry["tasks"].as_array().into_iter().flatten() {
        let cmd = get_s(t, "command").unwrap_or("?");
        let short: String = cmd.chars().take(160).collect();
        items.push(format!(
            "background command `{short}`{}",
            get_s(t, "description")
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        ));
    }
    for c in carry["crons"].as_array().into_iter().flatten() {
        items.push(format!(
            "scheduled prompt `{}` on `{}`{}",
            get_s(c, "prompt").unwrap_or("?"),
            get_s(c, "schedule").unwrap_or("?"),
            if c.get("recurring") == Some(&json!(false)) {
                " (one-off)"
            } else {
                " (recurring)"
            }
        ));
    }
    items
}

/// The prompt a resumed session gets when compaction stopped background work.
pub fn restore_prompt(carry: &Value, then_continue: bool) -> Option<String> {
    let items = carry_items(carry);
    if items.is_empty() {
        return None;
    }
    Some(format!(
        "Recompact compacted this session and restarted Claude, which stopped: {}. Start again the \
ones that are still needed, exactly as before (Bash with run_in_background or Monitor for \
commands, CronCreate for scheduled prompts; check CronList first so none is doubled){}",
        items.join("; "),
        if then_continue {
            ", then continue the work from where you left off."
        } else {
            ", then stop and wait for the user."
        }
    ))
}

/// Stop a prompt and say why. Claude Code shows every hook block in its warning style, so the
/// prompt is not echoed back under it as well.
fn block(reason: impl Into<String>) -> Value {
    json!({"decision": "block", "reason": reason.into(),
           "hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "suppressOriginalPrompt": true}})
}

/// UserPromptSubmit: a bare `/recompact` under the launcher is handled without the model. The
/// prompt is blocked (never reaches the context) and the launcher takes over within ~100 ms.
pub fn on_prompt(input: &Value) -> Option<Value> {
    on_prompt_in(Shell::from_env(), input)
}

pub fn on_prompt_in(shell: Option<Shell>, input: &Value) -> Option<Value> {
    let prompt = get_s(input, "prompt")?;
    if let Some(text) = shell.as_ref().and_then(|sh| switch_collision(sh, prompt)) {
        return Some(block(text));
    }
    if matches!(
        prompt.trim(),
        "/recompact setup" | "/segment-recompact:recompact setup"
    ) {
        let (ok, lines) = install(None);
        let mut text = if ok {
            lines.join(" ")
        } else {
            format!("recompact setup failed: {}", lines.join(" "))
        };
        if let Some(short) = shell.as_ref().and_then(Shell::job) {
            text.push_str(&format!(
                " Background job {short} needs none of this: it compacts in place as it is."
            ));
        }
        return Some(block(text));
    }
    if let Some((cmd, arg)) = switch_command(prompt) {
        let transcript = hook_transcript(input);
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
        return Some(block(text));
    }
    if !is_bare_recompact(prompt) {
        return None;
    }
    let shell = shell?;
    let session = hook_session(input)?;
    track_job(&shell, input);
    if !shell.tracks(session) && !shell.is_own_claude() {
        return None;
    }
    if in_place(&shell) {
        if in_flight(&shell, session) {
            return Some(block(
                "recompact · already compacting; switches at the next pause",
            ));
        }
        request(&shell, input, "manual", false, true, true);
        return Some(block(
            "recompact · compacting in the background; switches to the compacted copy at the next pause",
        ));
    }
    request(&shell, input, "manual", false, true, true);
    let restart = carry_items(&carry_for(&shell, Some(session))).len();
    Some(block(format!(
        "recompact: compacting this session; it resumes here in a moment{}.",
        if restart > 0 {
            format!(", and restarts its {restart} background task(s) and wakeup(s)")
        } else {
            String::new()
        }
    )))
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
    let transcript = hook_transcript(input)?;
    let Some(shell) = shell else {
        return suggest(session, &transcript);
    };
    track_job(&shell, input);
    if !shell.tracks(session) {
        return None;
    }
    remember_background(&shell, session, input);
    let switching = in_place(&shell);
    if switching && in_flight(&shell, session) {
        return None;
    }
    // A handoff the agent queued during the turn goes now.
    if let Some(mut req) = read_json(&shell.dir.join("request.json")) {
        if get_s(&req, "session") == Some(session) && req.get("ready") != Some(&json!(true)) {
            req["ready"] = json!(true);
            write_json(&shell.dir.join("request.json"), &req);
            wake(&shell);
            return Some(json!({"systemMessage": if switching {
                "recompact · compacting; switches to the compacted copy in a moment"
            } else {
                "recompact: compacting this session; it resumes here in a moment."
            }}));
        }
    }
    let (on, source) = auto_for(&shell.config, Some(session));
    if !on {
        return None;
    }
    let (live, model) = live_status(&transcript)?;
    let (at, checkpoint, target) = thresholds(&shell, session, &model, live);
    if live < at {
        // From halfway on, keep the summary cache warm in the background, so the handoff (or a
        // manual /recompact) finds almost every summary already written.
        if shell.config.get("summarize") == Some(&json!(true)) && live * 2 >= at && live > target {
            maybe_prewarm(&shell, session, &transcript, live, target);
        }
        return None;
    }
    // A restart stops background work: wakeups and open-ended monitors are restarted after it,
    // and a job with an end (a build, a test run) is worth waiting for, up to the checkpoint
    // size. A switch in place stops nothing.
    let finite: Vec<Value> = carry_for(&shell, Some(session))["tasks"]
        .as_array()
        .map(|a| a.iter().filter(|t| !is_open_ended(t)).cloned().collect())
        .unwrap_or_default();
    if !switching && !finite.is_empty() && live < checkpoint {
        let mark = shell
            .dir
            .join(format!("deferred-{}.json", short_id(session)));
        if read_json(&mark).is_none() {
            write_json(&mark, &json!({"tokens": live}));
            return Some(json!({"systemMessage": format!(
                "recompact: context is {} (≥ {}); compaction waits for {} background job(s) to finish \
(until {} at most, then they are restarted). Type /recompact to do it now.",
                fmt_k(live), fmt_k(at), finite.len(), fmt_k(checkpoint))}));
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
    if switching {
        return Some(json!({"systemMessage": format!(
            "recompact · {} ≥ {}: compacting in the background; switches to the compacted copy at the next pause",
            fmt_k(live), fmt_k(at))}));
    }
    let restart = carry_items(&carry_for(&shell, Some(session))).len();
    Some(json!({"systemMessage": format!(
        "recompact: context is {} (≥ {}); compacting and resuming here{}.", fmt_k(live), fmt_k(at),
        if restart > 0 { format!("; {restart} background task(s) and wakeup(s) will be restarted") } else { String::new() })}))
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
    track_job(&shell, input);
    if !shell.tracks(session) || !auto_on(&shell.config, Some(session)) {
        return None;
    }
    let transcript = hook_transcript(input)?;
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

/// The process `marker` names, if it is still running as `recompact <role>` (`" prewarm "`,
/// `" job-handoff "`): the pid may have been reused since.
fn marked_process(marker: &Path, role: &str) -> Option<u32> {
    let pid = get_u(&read_json(marker)?, "pid")? as u32;
    if !pid_alive(pid) {
        return None;
    }
    let cmd = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&cmd.stdout)
        .contains(role)
        .then_some(pid)
}

fn prewarm_running(marker: &Path) -> Option<u32> {
    marked_process(marker, " prewarm ")
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
    let Some(session) = pos.first().cloned().or_else(own_session) else {
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
                "Handoff queued. When this turn ends, recompact compacts session {} and {}{}.",
                short_id(&session),
                match shell.job() {
                    Some(short) => format!("switches background job {short} to the compacted copy"),
                    None => "resumes it in this terminal".into(),
                },
                if kick {
                    ", and the resumed session continues the work"
                } else {
                    ""
                }
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
    /// Run claude on the launcher's own terminal and hand off in place (`--pty`, `--no-pty`,
    /// `RECOMPACT_PTY=0`); unset means: when the launcher runs in a terminal.
    pty: Option<bool>,
    /// How long `/resume <twin>` may take to open the twin before it is typed again (then the
    /// launcher restarts claude instead).
    switch_timeout: Duration,
    /// The recompact binary that runs `pty-leader` (this one, unless a test names it).
    leader: PathBuf,
    copts_raw: serde_json::Map<String, Value>,
    /// Run claude as a Claude Code background session and attach this terminal to it
    /// (`RECOMPACT_JOBS=1`, `--jobs`; off by default).
    jobs: bool,
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
        pty: (std::env::var("RECOMPACT_PTY").ok().as_deref() == Some("0")).then_some(false),
        switch_timeout: Duration::from_secs(12),
        leader: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("recompact")),
        copts_raw: serde_json::Map::new(),
        jobs: std::env::var("RECOMPACT_JOBS").ok().as_deref() == Some("1"),
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
            "--pty" => {
                l.pty = Some(true);
                i += 1;
            }
            "--jobs" => {
                l.jobs = true;
                i += 1;
            }
            "--no-jobs" => {
                l.jobs = false;
                i += 1;
            }
            "--no-pty" => {
                l.pty = Some(false);
                i += 1;
            }
            "--switch-timeout" => {
                l.switch_timeout = Duration::from_secs_f64(val().parse().unwrap_or(12.0));
                i += 2;
            }
            "--leader" => {
                l.leader = PathBuf::from(val());
                i += 2;
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
    if l.jobs {
        if let Some(code) = run_as_job(&l, &parsed, &claude_args) {
            return code;
        }
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
    let proxy = if l.pty.unwrap_or(!l.interactive) {
        Proxy::start(&recompact_home().join("shell.log"))
    } else {
        None
    };

    let mut cycles = 0usize;
    loop {
        cycles += 1;
        for f in [
            "request.json",
            "nudged.json",
            "session.json",
            "warned.json",
            "background.json",
            "handoff.json",
            "switch.json",
            "child.json",
        ] {
            let _ = fs::remove_file(state.join(f));
        }
        let config = |child: u32, rearm: usize| {
            json!({
                "launcher": std::process::id(), "child": child, "at": l.at,
                "checkpoint_at": l.checkpoint_at, "auto": l.auto, "rearm": rearm,
                "summarize": copts.summarize.is_some(),
                "summarize_with": copts.summarize.as_ref().map(|c| c.model.clone()),
                "target": copts.target, "launch_model": origin.launch_model,
                "inplace": proxy.is_some(),
            })
        };
        write_json(&state.join("config.json"), &config(0, rearm));
        let mut cmd = match &proxy {
            Some(_) => {
                let mut c = Command::new(&l.leader);
                c.arg("pty-leader").arg(&l.bin);
                c
            }
            None => Command::new(&l.bin),
        };
        cmd.args(&next_args).env("RECOMPACT_SHELL", &state);
        if let Some(d) = read_json(&state.join("start.json"))
            .and_then(|s| get_s(&s, "cwd").map(PathBuf::from))
            .filter(|d| d.is_dir())
        {
            cmd.current_dir(d);
        }
        let spawned = match &proxy {
            Some(p) => p.spawn(&mut cmd),
            None => cmd.spawn(),
        };
        let child = match spawned {
            Ok(c) => c,
            Err(e) => {
                eprintln!("recompact shell: cannot run {}: {e}", l.bin);
                let _ = fs::remove_dir_all(&state);
                return 127;
            }
        };
        let pid = child.id();
        let claude = if proxy.is_some() {
            led_pid(&state).unwrap_or(pid)
        } else {
            pid
        };
        write_json(&state.join("config.json"), &config(claude, rearm));
        let (code, req, built) = match &proxy {
            Some(p) => {
                let mut ip = InPlace {
                    l: &l,
                    copts: &copts,
                    origin: &origin,
                    state: &state,
                    pid,
                    claude,
                    rearm,
                    config: &config,
                };
                let out = supervise_in_place(p, &mut ip);
                rearm = ip.rearm;
                out
            }
            None => {
                let (code, req) = supervise(pid, &state);
                (code, req, None)
            }
        };
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
        let at = handoff_at(&req, &l, &origin, &transcript);
        let (twin, est) = match built.or_else(|| run_handoff(&req, &copts, at)) {
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
        let mut carry = req.get("carry").cloned().unwrap_or(json!({}));
        // Scheduled prompts belong to the session id: resuming the same session brings them
        // back by themselves (measured), so only a new twin needs them recreated.
        if get_s(&req, "session") == Some(twin.as_str()) {
            carry["crons"] = json!([]);
        }
        let restore = restore_prompt(&carry, kick);
        if !carry_items(&carry).is_empty() {
            say(&format!(
                "the resumed session restarts: {}",
                carry_items(&carry).join("; ")
            ));
        }
        let prompt = restore.or_else(|| kick.then(|| l.kick.clone()));
        next_args = relaunch_args(&origin, &twin, &transcript, prompt.as_deref());
    }
}

/// Claude's pid under `recompact pty-leader`, which reports it as soon as it has forked.
fn led_pid(state: &Path) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(pid) = read_json(&state.join("child.json")).and_then(|c| get_u(&c, "pid")) {
            return Some(pid as u32);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The trigger size a handoff of `req` compacts against.
fn handoff_at(req: &Value, l: &Launch, origin: &Origin, transcript: &Path) -> usize {
    user_at(get_s(req, "session")).or(l.at).unwrap_or_else(|| {
        let (live, model) = live_status(transcript).unwrap_or((0, String::new()));
        let model = if model.is_empty() {
            origin.launch_model.clone().unwrap_or_default()
        } else {
            model
        };
        default_at_for(&model, live)
    })
}

// ------------------------------------------------------------------------------------ in place

/// What an in-place handoff needs from the launcher.
struct InPlace<'a> {
    l: &'a Launch,
    copts: &'a ContinueOpts,
    origin: &'a Origin,
    state: &'a Path,
    /// `recompact pty-leader`: what the launcher waits for and signals.
    pid: u32,
    /// Claude itself: what the hooks identify their claude by.
    claude: u32,
    rearm: usize,
    config: &'a dyn Fn(u32, usize) -> Value,
}

enum Phase {
    /// Compacting in a worker thread while claude keeps running.
    Building(std::thread::JoinHandle<Option<(String, usize)>>),
    /// The twin is ready; waiting for a pause: turn over, transcript and keyboard quiet, input
    /// box empty.
    Ready(Ready),
    /// `/resume <twin>` was typed; waiting for the twin's SessionStart.
    Switching {
        twin: String,
        est: usize,
        since: Instant,
        tries: u32,
    },
}

struct Ready {
    twin: String,
    est: usize,
    /// What the switch is waiting for, as the status line shows it.
    wait: &'static str,
    /// The last record of the session the twin carries: anything after it is not in the twin.
    covered: String,
    /// The transcript's length, and since when it has stayed that.
    len: u64,
    since: Instant,
    /// `tool_call_waiting` for that length.
    waiting: Option<bool>,
}

struct Handoff {
    req: Value,
    session: String,
    transcript: PathBuf,
    at: usize,
    live: usize,
    rebuilds: u32,
    phase: Option<Phase>,
}

enum Next {
    Wait,
    Done,
    /// The switch did not take: restart claude on this twin.
    Restart(String, usize),
}

/// How long the transcript and the keyboard must stay still before typing into claude: a turn
/// that just started can take a moment to reach the transcript.
const SETTLE: Duration = Duration::from_millis(1500);

fn build(req: &Value, copts: &ContinueOpts, at: usize) -> Phase {
    let (req, copts) = (req.clone(), copts.clone());
    Phase::Building(std::thread::spawn(move || run_handoff(&req, &copts, at)))
}

/// Drop a twin that was never switched to.
fn discard(transcript: &Path, twin: &str) {
    if let Some(dir) = transcript.parent() {
        let _ = fs::remove_file(dir.join(format!("{twin}.jsonl")));
        lineage_remove(dir, twin);
    }
    let _ = fs::remove_file(session_setting_path(twin));
}

fn tracked_session(state: &Path) -> Option<String> {
    read_json(&state.join("session.json")).and_then(|s| get_s(&s, "session").map(String::from))
}

/// Typed into the terminal, these compact without a model turn or a hook (see `is_bare_recompact`).
const TYPED: &[&str] = &["/recompact", "/segment-recompact:recompact"];

/// The request a typed `/recompact` makes, as the UserPromptSubmit hook would.
fn typed_request(state: &Path, session: &str) -> Option<Value> {
    let s = read_json(&state.join("session.json"))?;
    Some(json!({
        "session": session, "transcript": s.get("transcript"), "reason": "manual",
        "kick": false, "force": true, "ready": true, "carry": carry_in(state, Some(session)),
    }))
}

fn start_handoff(ip: &InPlace, mut req: Value) -> Option<Handoff> {
    let session = get_s(&req, "session")?.to_string();
    let transcript = request_transcript(&req, ip.l)?;
    req["transcript"] = json!(transcript);
    let at = handoff_at(&req, ip.l, ip.origin, &transcript);
    stop_prewarm(ip.state);
    write_json(
        &ip.state.join("handoff.json"),
        &json!({"session": session, "pid": std::process::id()}),
    );
    let live = live_tokens(&transcript).unwrap_or(0);
    let feed = ip.state.join("progress.json");
    let _ = fs::remove_file(&feed);
    crate::set_progress_file(Some(feed));
    crate::progress_update(
        json!({"phase": "reading", "done": 0, "total": 0, "live": live,
                                  "started": crate::now_unix()}),
    );
    Some(Handoff {
        live,
        phase: Some(build(&req, ip.copts, at)),
        req,
        session,
        transcript,
        at,
        rebuilds: 0,
    })
}

/// The switch message for the user, and for the twin's model when work was running.
fn switch_notice(h: &Handoff, twin: &str, est: usize) -> Value {
    let running = ["tasks", "crons"]
        .iter()
        .any(|k| h.req["carry"][k].as_array().is_some_and(|a| !a.is_empty()));
    json!({
        "twin": twin,
        "message": format!(
            "recompact · compacted in place: {} → ~{}{}",
            fmt_k(h.live),
            fmt_k(est),
            if running { " · background work kept running" } else { "" }
        ),
        "context": running.then_some(
            "recompact compacted this session in place: the same Claude Code process switched to \
    this compacted copy, so the background commands, monitors, agents and scheduled prompts started \
    before that are still running, and their notices arrive here."
        ),
    })
}

fn step(h: &mut Handoff, ip: &mut InPlace, t: &mut dyn Term) -> Next {
    let tracked = tracked_session(ip.state);
    let (next, phase) = match h.phase.take() {
        None => (Next::Done, None),
        Some(Phase::Building(job)) if !job.is_finished() => {
            (Next::Wait, Some(Phase::Building(job)))
        }
        Some(Phase::Building(job)) => match job.join().ok().flatten() {
            Some((twin, est)) if twin != h.session => {
                match last_carried_uuid(&h.transcript.with_file_name(format!("{twin}.jsonl"))) {
                    Some(covered) => {
                        carry_session_setting(&h.session, &twin);
                        crate::progress_update(json!({"phase": "waiting", "est": est}));
                        let ready = Ready {
                            twin,
                            est,
                            wait: "",
                            covered,
                            len: file_len(&h.transcript),
                            since: Instant::now(),
                            waiting: None,
                        };
                        (Next::Wait, Some(Phase::Ready(ready)))
                    }
                    None => {
                        discard(&h.transcript, &twin);
                        progress_end(ip.state, None);
                        (Next::Done, None)
                    }
                }
            }
            Some(_) => {
                progress_end(ip.state, Some("noop"));
                (Next::Done, None)
            }
            None => {
                progress_end(ip.state, None);
                (Next::Done, None)
            }
        },
        Some(Phase::Ready(r)) if tracked.as_deref() == Some(r.twin.as_str()) => {
            // The user typed `/resume <twin>` themselves: that is the switch.
            (finish_switch(h, ip, t, &r.twin, r.est), None)
        }
        Some(Phase::Ready(mut r)) => {
            let len = file_len(&h.transcript);
            if len != r.len {
                r.len = len;
                r.since = Instant::now();
                r.waiting = None;
            }
            let settled = r.since.elapsed() >= SETTLE && t.settled();
            let wait = t.wait(&h.transcript, &mut r);
            if wait != r.wait {
                crate::progress_update(json!({"wait": wait}));
                r.wait = wait;
            }
            if tracked.as_deref() != Some(h.session.as_str()) {
                // The user opened another session (/resume, /clear): this twin is for one they left.
                discard(&h.transcript, &r.twin);
                progress_end(ip.state, None);
                (Next::Done, None)
            } else if !(settled && wait == "quiet") {
                (Next::Wait, Some(Phase::Ready(r)))
            } else {
                match t.take() {
                    Take::NotNow => (Next::Wait, Some(Phase::Ready(r))),
                    Take::Never => {
                        progress_end(ip.state, None);
                        (Next::Restart(r.twin, r.est), None)
                    }
                    Take::Yes if activity_after(&h.transcript, &r.covered) => {
                        // The session moved on while compacting: build again from where it is now.
                        t.give_back();
                        discard(&h.transcript, &r.twin);
                        if h.rebuilds >= 3 {
                            progress_end(ip.state, None);
                            (Next::Done, None)
                        } else {
                            h.rebuilds += 1;
                            crate::progress_update(
                                json!({"phase": "reading", "done": 0, "total": 0}),
                            );
                            (Next::Wait, Some(build(&h.req, ip.copts, h.at)))
                        }
                    }
                    Take::Yes => {
                        write_json(
                            &ip.state.join("switch.json"),
                            &switch_notice(h, &r.twin, r.est),
                        );
                        crate::progress_update(json!({"phase": "switching"}));
                        t.type_resume(&r.twin);
                        (
                            Next::Wait,
                            Some(Phase::Switching {
                                twin: r.twin,
                                est: r.est,
                                since: Instant::now(),
                                tries: 1,
                            }),
                        )
                    }
                }
            }
        }
        Some(Phase::Switching {
            twin,
            est,
            since,
            tries,
        }) => {
            if tracked.as_deref() == Some(twin.as_str()) {
                (finish_switch(h, ip, t, &twin, est), None)
            } else if since.elapsed() < ip.l.switch_timeout {
                (
                    Next::Wait,
                    Some(Phase::Switching {
                        twin,
                        est,
                        since,
                        tries,
                    }),
                )
            } else if tries < 2 {
                t.type_resume(&twin);
                (
                    Next::Wait,
                    Some(Phase::Switching {
                        twin,
                        est,
                        since: Instant::now(),
                        tries: tries + 1,
                    }),
                )
            } else {
                let _ = fs::remove_file(ip.state.join("switch.json"));
                progress_end(ip.state, None);
                (Next::Restart(twin, est), None)
            }
        }
    };
    h.phase = phase;
    next
}

/// Claude now has the twin open: re-arm, continue the work if it was mid-task, report.
fn finish_switch(h: &Handoff, ip: &mut InPlace, t: &mut dyn Term, twin: &str, est: usize) -> Next {
    ip.rearm = rearm_for(est, h.at);
    write_json(
        &ip.state.join("config.json"),
        &(ip.config)(ip.claude, ip.rearm),
    );
    let records = load_jsonl(&h.transcript.with_file_name(format!("{twin}.jsonl")));
    let kick = (h.req.get("kick") == Some(&json!(true)))
        .then(|| ip.l.kick.clone())
        .or_else(|| has_active_goal(&records).then(|| "continue".to_string()));
    if let Some(k) = kick {
        t.type_prompt(&k);
    }
    t.give_back();
    progress_end(ip.state, Some("done"));
    say(&format!(
        "switched in place to {} ({} → ~{})",
        short_id(twin),
        fmt_k(h.live),
        fmt_k(est)
    ));
    Next::Done
}

/// The progress row's last state: a result to show for a moment, or nothing.
fn progress_end(state: &Path, shown: Option<&str>) {
    match shown {
        Some(phase) => crate::progress_update(json!({ "phase": phase })),
        None => {
            let _ = fs::remove_file(state.join("progress.json"));
        }
    }
    crate::set_progress_file(None);
}

/// Claude quit with a handoff under way: stop the compaction (it removes its own output) or
/// drop the twin that was never switched to.
fn abandon(h: Handoff, state: &Path) {
    progress_end(state, None);
    match h.phase {
        Some(Phase::Building(job)) => {
            CANCEL.store(true, Ordering::SeqCst);
            let _ = job.join();
            CANCEL.store(false, Ordering::SeqCst);
        }
        Some(Phase::Ready(Ready { twin, .. })) | Some(Phase::Switching { twin, .. }) => {
            discard(&h.transcript, &twin)
        }
        None => {}
    }
}

/// `supervise` for claude on the launcher's terminal: handoffs happen in place while it runs.
/// Returns claude's exit code, plus the request and its twin when a switch did not take and
/// claude was stopped so the twin can be resumed the old way.
fn supervise_in_place(
    p: &Proxy,
    ip: &mut InPlace,
) -> (i32, Option<Value>, Option<(String, usize)>) {
    let pid = ip.pid;
    let req_path = ip.state.join("request.json");
    let mut job: Option<Handoff> = None;
    loop {
        p.tick();
        match poll_child(pid) {
            ChildState::Exited(code) => {
                p.child_gone();
                if let Some(h) = job.take() {
                    abandon(h, ip.state);
                }
                for f in ["handoff.json", "switch.json"] {
                    let _ = fs::remove_file(ip.state.join(f));
                }
                return (code, None, None);
            }
            // Claude stopped itself (Ctrl-Z): hand the terminal back and stop the launcher with
            // it; `fg` resumes both.
            ChildState::Stopped => {
                p.suspend();
                unsafe {
                    kill(0, sigtstp());
                }
                p.resume();
                send_signal(pid, libc::SIGCONT);
            }
            ChildState::Running => {}
        }
        let tracked = tracked_session(ip.state);
        // After a restart, keys held during the failed switch go to the new claude once it is up.
        if job.is_none() && p.holding() && tracked.is_some() {
            p.release();
        }
        // A bare `/recompact` typed into the terminal never reaches claude: blocking it in a hook
        // would show as an error. The hook still handles it when it arrives another way.
        p.intercept(if tracked.is_some() { TYPED } else { &[] });
        if p.take_typed().is_some() && job.is_none() {
            if let Some(req) = tracked.and_then(|s| typed_request(ip.state, &s)) {
                job = start_handoff(ip, req);
            }
        }
        if job.is_none() {
            if let Some(req) = read_json(&req_path).filter(|r| r.get("ready") == Some(&json!(true)))
            {
                let _ = fs::remove_file(&req_path);
                job = start_handoff(ip, req);
            }
        }
        if let Some(h) = job.as_mut() {
            match step(h, ip, &mut Own(p)) {
                Next::Wait => {}
                Next::Done => {
                    job = None;
                    let _ = fs::remove_file(ip.state.join("handoff.json"));
                }
                Next::Restart(twin, est) => {
                    let req = job.take().map(|h| h.req);
                    let _ = fs::remove_file(ip.state.join("handoff.json"));
                    send_signal(pid, SIGTERM);
                    let deadline = Instant::now() + Duration::from_secs(15);
                    let code = loop {
                        if let ChildState::Exited(code) = poll_child(pid) {
                            break code;
                        }
                        if Instant::now() > deadline {
                            send_signal(pid, SIGKILL);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    };
                    p.child_gone();
                    p.say("\x1b[36mrecompact · the switch in place did not take; restarting claude on the compacted session\x1b[0m");
                    return (code, req, Some((twin, est)));
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
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

// ------------------------------------------------------------------------------------ job handoff

/// The claude a job's worker runs: `RECOMPACT_CLAUDE_BIN` (or `--claude-bin`), else `claude`
/// on the PATH, else the binary the job itself runs.
fn job_claude_bin(l: &Launch) -> String {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&l.bin).is_file()));
    if l.bin != "claude" || on_path {
        return l.bin.clone();
    }
    std::env::var("CLAUDE_CODE_EXECPATH")
        .ok()
        .filter(|p| Path::new(p).is_file())
        .unwrap_or_else(|| l.bin.clone())
}

/// `recompact job-handoff <job dir> <claude pid> [shell options]`: hand a background job off in
/// place. Its hooks start this, detached, for a ready request. It compacts while the job keeps
/// working, waits for a pause, attaches, types `/resume <twin>`, and detaches. It takes requests
/// until none is left, and stops when the job's claude does.
pub fn cmd_job_handoff(args: &[String]) -> i32 {
    let (Some(dir), Some(pid)) = (
        args.first(),
        args.get(1).and_then(|p| p.parse::<u32>().ok()),
    ) else {
        eprintln!("usage: recompact job-handoff <job dir> <claude pid>");
        return 2;
    };
    let Some(job) = Job::at(Path::new(dir)) else {
        eprintln!("job-handoff: {dir} is not a background job");
        return 1;
    };
    let (l, _) = split_launcher_args(&args[2..]);
    let bin = job_claude_bin(&l);
    let shell = job_shell(&job, pid);
    let state = shell.dir.clone();
    let marker = state.join("worker.json");
    if marked_process(&marker, " job-handoff ").is_some_and(|w| w != std::process::id()) {
        return 0;
    }
    write_json(&marker, &json!({"pid": std::process::id()}));
    prune_job_states(&job);
    let copts = continue_opts(&l.copts_raw, Some("haiku"));
    let origin = job_origin(&job);
    let config = |_: u32, rearm: usize| json!({ "rearm": rearm });
    let mut ip = InPlace {
        l: &l,
        copts: &copts,
        origin: &origin,
        state: &state,
        pid,
        claude: pid,
        rearm: get_u(&shell.config, "rearm").unwrap_or(0),
        config: &config,
    };
    let mut term = Attached::new(&job, &bin, pid);
    let req_path = state.join("request.json");
    let mut current: Option<Handoff> = None;
    loop {
        if !pid_alive(pid) {
            // The job stopped or restarted: there is nothing to switch.
            if let Some(h) = current.take() {
                abandon(h, &state);
            }
            let _ = fs::remove_file(state.join("handoff.json"));
            break;
        }
        let Some(h) = current.as_mut() else {
            let Some(req) = read_json(&req_path).filter(|r| r.get("ready") == Some(&json!(true)))
            else {
                break;
            };
            let _ = fs::remove_file(&req_path);
            current = start_handoff(&ip, req);
            continue;
        };
        match step(h, &mut ip, &mut term) {
            Next::Wait => {}
            Next::Done => {
                current = None;
                let _ = fs::remove_file(state.join("handoff.json"));
            }
            Next::Restart(twin, est) => {
                term.give_back();
                if let Some(h) = current.take() {
                    restart_job(&job, &ip, &bin, &h, &twin, est);
                }
                let _ = fs::remove_file(state.join("handoff.json"));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    term.give_back();
    if read_json(&marker).and_then(|m| get_u(&m, "pid")) == Some(std::process::id() as usize) {
        let _ = fs::remove_file(&marker);
    }
    0
}

/// The switch did not take: start the twin as a new background job (`claude --bg --resume`) and
/// stop the old job once the new one runs. The old job's background work ends with it, so the
/// twin's first prompt restarts it, and the twin's first session start says what happened.
fn restart_job(job: &Job, ip: &InPlace, bin: &str, h: &Handoff, twin: &str, est: usize) -> bool {
    let kick = h.req.get("kick") == Some(&json!(true));
    let carry = h.req.get("carry").cloned().unwrap_or(json!({}));
    let prompt = restore_prompt(&carry, kick).or_else(|| kick.then(|| ip.l.kick.clone()));
    let state = job.state();
    let mut args = vec!["--bg".to_string()];
    // A new job is otherwise named after its first prompt; keep the name the old one showed.
    let named = ip
        .origin
        .carry
        .iter()
        .any(|a| a == "-n" || a == "--name" || a.starts_with("--name="));
    if let Some(name) = get_s(&state, "name").filter(|n| !named && !n.is_empty()) {
        args.extend(["--name".to_string(), name.to_string()]);
    }
    args.extend(relaunch_args(
        ip.origin,
        twin,
        &h.transcript,
        prompt.as_deref(),
    ));
    leave_notice(
        twin,
        &format!(
            "recompact · compacted {} → ~{} into this new background job: background job {} could \
not be switched in place, so it was stopped{}",
            fmt_k(h.live),
            fmt_k(est),
            job.short,
            if carry_items(&carry).is_empty() {
                ""
            } else {
                "; its background work is restarted here"
            }
        ),
    );
    let mut cmd = Command::new(bin);
    cmd.args(&args).stdin(Stdio::null()).stderr(Stdio::null());
    plain_claude(&mut cmd);
    if let Some(cwd) = get_s(&state, "cwd")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
    {
        cmd.current_dir(cwd);
    }
    let printed = cmd
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let named = printed.as_deref().and_then(started_job);
    let new = printed
        .as_ref()
        .and_then(|_| wait_for_job(bin, named.as_deref(), twin));
    let Some(short) = new else {
        let _ = fs::remove_file(notice_path(twin));
        leave_notice(
            &h.session,
            &format!(
                "recompact: this background job could not be switched to its compacted copy. \
`claude --resume {twin}` opens the copy, or `claude --bg --resume {twin}` as a new job."
            ),
        );
        say(&format!(
            "could not switch job {} or start {} as a new one",
            job.short,
            short_id(twin)
        ));
        return false;
    };
    let _ = fs::create_dir_all(job_state_dir(&short));
    write_json(
        &job_state_dir(&short).join("config.json"),
        &json!({"rearm": rearm_for(est, h.at)}),
    );
    let mut stop = Command::new(bin);
    stop.args(["stop", &job.short])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    plain_claude(&mut stop);
    let _ = stop.status();
    say(&format!(
        "job {} could not be switched in place: started job {short} on the compacted session {}, \
then stopped job {}",
        job.short,
        short_id(twin),
        job.short
    ));
    true
}

/// The short id `claude --bg` prints: `backgrounded · 1a2b3c4d`, then (CLI 2.1.293) the job's
/// name and, without a prompt, a hint that it waits for one.
fn started_job(printed: &str) -> Option<String> {
    printed
        .lines()
        .find_map(|l| {
            let mut words = l.split_whitespace();
            (words.next()? == "backgrounded").then_some(())?;
            words.find(|w| *w != "·")
        })
        .filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .map(String::from)
}

/// Wait, up to a minute, until `claude agents --json` lists the new job with a process; its
/// short id.
fn wait_for_job(bin: &str, short: Option<&str>, session: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let mut cmd = Command::new(bin);
        cmd.args(["agents", "--json"])
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        plain_claude(&mut cmd);
        let list = cmd
            .output()
            .ok()
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok());
        for a in list.iter().filter_map(Value::as_array).flatten() {
            let id = get_s(a, "id");
            let ours = (short.is_some() && id == short) || get_s(a, "sessionId") == Some(session);
            if ours && get_s(a, "kind") == Some("background") && a.get("pid").is_some() {
                return id.or(short).map(String::from);
            }
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Forget the state of jobs that no longer exist (`claude rm`).
fn prune_job_states(job: &Job) {
    let Some(jobs) = job.dir.parent() else {
        return;
    };
    let Ok(rd) = fs::read_dir(recompact_home().join("jobs")) else {
        return;
    };
    for e in rd.flatten() {
        if !jobs.join(e.file_name()).exists() {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

// ------------------------------------------------------------------------- background sessions

/// The terminal's identity and Claude Code's own process variables: a background session has a
/// terminal of its own, so these never come from the terminal that started it.
const JOB_ENV_SKIP: &[&str] = &[
    "_",
    "CLAUDECODE",
    "CLAUDE_PID",
    "COLORTERM",
    "COLUMNS",
    "LINES",
    "OLDPWD",
    "PWD",
    "RECOMPACT_INTERNAL",
    "RECOMPACT_JOBS",
    "RECOMPACT_SHELL",
    "SHLVL",
    "SSH_TTY",
    "STY",
    "TERM",
    "TERMINFO",
    "TERMINFO_DIRS",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "TMUX",
    "TMUX_PANE",
    "WINDOWID",
];
const JOB_ENV_SKIP_PREFIXES: &[&str] = &[
    "ALACRITTY_",
    "CLAUDE_CODE_",
    "CLAUDE_JOB",
    "GHOSTTY_",
    "ITERM_",
    "KITTY_",
    "VSCODE_",
    "WEZTERM_",
    "XPC_",
    "__CF",
];

/// What a background session takes from the terminal that starts it. Claude Code's daemon starts
/// every session with the daemon's own environment, fixed when the daemon started (measured, CLI
/// 2.1.295), so a variable a terminal exported (direnv's, `RECOMPACT_AT`) would be missing. The
/// launcher hands them over in the session's `--settings` file: Claude Code applies its `env` to
/// the session's commands and hooks and restarts the session with the same file. A key a
/// settings file sets keeps that file's value, as in a terminal session.
pub fn job_env(
    vars: impl IntoIterator<Item = (String, String)>,
    settings_keys: &std::collections::HashSet<String>,
) -> serde_json::Map<String, Value> {
    vars.into_iter()
        .filter(|(k, _)| {
            !JOB_ENV_SKIP.contains(&k.as_str())
                && !JOB_ENV_SKIP_PREFIXES.iter().any(|p| k.starts_with(p))
                && !settings_keys.contains(k)
        })
        .map(|(k, v)| (k, Value::String(v)))
        .collect()
}

/// The `env` keys of the settings files Claude Code reads in `cwd`.
fn settings_env_keys(cwd: &Path) -> std::collections::HashSet<String> {
    [
        home().join(".claude").join("settings.json"),
        cwd.join(".claude").join("settings.json"),
        cwd.join(".claude").join("settings.local.json"),
    ]
    .iter()
    .filter_map(|p| read_json(p))
    .filter_map(|v| {
        v.get("env")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
    })
    .flatten()
    .collect()
}

/// A picker, a second settings file and tmux need claude on this terminal.
pub fn job_unsupported(args: &[Arg]) -> Option<&'static str> {
    let flag = |names: &[&str]| {
        args.iter()
            .find(|a| a.flag.as_deref().is_some_and(|f| names.contains(&f)))
    };
    if flag(&["-r", "--resume"]).is_some_and(|a| a.values.is_empty()) {
        Some("the session picker")
    } else if flag(&["--settings"]).is_some() {
        Some("--settings")
    } else if flag(&["--tmux"]).is_some() {
        Some("--tmux")
    } else {
        None
    }
}

/// The session settings file, readable only by the user: it holds whatever the terminal
/// exported, secrets included.
fn write_job_env(dir: &Path, env: serde_json::Map<String, Value>) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()?;
    let path = dir.join(format!("{}.json", crate::uuid_v4()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    f.write_all(json!({ "env": env }).to_string().as_bytes()).ok()?;
    Some(path)
}

/// Delete settings files no session restarts with any more: `claude rm` leaves them behind.
fn prune_job_env(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut used = String::new();
    if let Ok(jobs) = fs::read_dir(home().join(".claude").join("jobs")) {
        for j in jobs.flatten() {
            used.push_str(&fs::read_to_string(j.path().join("state.json")).unwrap_or_default());
        }
    }
    for e in rd.flatten() {
        let path = e.path();
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d > Duration::from_secs(3600));
        if old && !used.contains(path.to_string_lossy().as_ref()) {
            let _ = fs::remove_file(path);
        }
    }
}

/// The running background session that has `session` open.
fn running_job_for(bin: &str, session: &str) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args(["agents", "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    plain_claude(&mut cmd);
    let list = cmd
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())?;
    list.as_array()?
        .iter()
        .find(|a| get_s(a, "sessionId") == Some(session) && a.get("pid").is_some())
        .and_then(|a| get_s(a, "id"))
        .map(String::from)
}

/// Variables the daemon gets if this call starts it: enough to run, and nothing a later session
/// could inherit by accident (each session gets its own terminal's through `job_env`).
const DAEMON_ENV: &[&str] = &[
    "COLORTERM",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LOGNAME",
    "PATH",
    "SHELL",
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TMPDIR",
    "USER",
];

/// `recompact shell` with `RECOMPACT_JOBS=1` (or `--jobs`): start claude as a Claude Code
/// background session and attach this terminal to it. A switch to a compacted copy then waits
/// for the daemon's own record of whether a turn runs, not for what the terminal shows. `None`
/// when this invocation cannot run that way; the launcher then runs claude here as usual.
fn run_as_job(l: &Launch, parsed: &[Arg], claude_args: &[String]) -> Option<i32> {
    if let Some(what) = job_unsupported(parsed) {
        say(&format!(
            "a background session cannot use {what}; running claude in this terminal"
        ));
        return None;
    }
    // A Ctrl+C that reaches this process group (while claude starts, or as `claude attach`
    // changes screens) must not end the launcher before it says which session runs.
    catch_interrupts();
    let here = std::env::current_dir().ok()?;
    let short = match flag_value(parsed, &["-r", "--resume"])
        .and_then(|s| running_job_for(&l.bin, s))
    {
        Some(short) => short,
        None => {
            let dir = l
                .state_root
                .parent()
                .map_or_else(|| recompact_home().join("job-env"), |p| p.join("job-env"));
            prune_job_env(&dir);
            let vars = std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
            let mut env = job_env(vars, &settings_env_keys(&here));
            let opts = [
                ("RECOMPACT_AT", l.at.map(|n| n.to_string())),
                ("RECOMPACT_CHECKPOINT_AT", l.checkpoint_at.map(|n| n.to_string())),
                ("RECOMPACT_AUTO", l.auto.map(|a| if a { "1" } else { "0" }.to_string())),
                (
                    "RECOMPACT_SUMMARIZE_WITH",
                    l.copts_raw
                        .get("summarize-with")
                        .and_then(Value::as_str)
                        .map(String::from),
                ),
            ];
            for (k, v) in opts {
                if let Some(v) = v {
                    env.insert(k.into(), Value::String(v));
                }
            }
            let file = write_job_env(&dir, env)?;
            say("starting a background session…");
            let mut cmd = Command::new(&l.bin);
            cmd.arg("--bg")
                .arg("--settings")
                .arg(&file)
                .args(claude_args)
                .env_clear()
                .envs(DAEMON_ENV.iter().filter_map(|k| Some((k, std::env::var_os(k)?))))
                .stdin(Stdio::null())
                .stderr(Stdio::inherit());
            let started = cmd
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| started_job(&String::from_utf8_lossy(&o.stdout)));
            match started {
                Some(short) => short,
                None if crate::cancelled() => {
                    let _ = fs::remove_file(&file);
                    say("stopped before a background session opened; `claude agents` lists any \
that started");
                    return Some(130);
                }
                None => {
                    let _ = fs::remove_file(&file);
                    say("could not start a background session; running claude in this terminal");
                    return None;
                }
            }
        }
    };
    let how = format!("`claude attach {short}` reopens it · `claude stop {short}` ends it");
    if crate::cancelled() {
        say(&format!("background session {short} keeps running · {how}"));
        return Some(130);
    }
    say(&format!(
        "background session {short} · Ctrl+Z returns to your shell and leaves it running · {how}"
    ));
    let mut attach = Command::new(&l.bin);
    attach.args(["attach", &short]);
    plain_claude(&mut attach);
    let code = attach.status().ok().and_then(|s| s.code()).unwrap_or(1);
    say(&format!("background session {short} keeps running · {how}"));
    Some(code)
}

// ------------------------------------------------------------------------------------ setup

const BLOCK_START: &str = "# >>> recompact >>>";
const BLOCK_END: &str = "# <<< recompact <<<";

pub(crate) fn recompact_home() -> PathBuf {
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
        "zsh" => Ok(std::env::var("ZDOTDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home())
            .join(".zshrc")),
        "bash" if cfg!(target_os = "macos") => {
            // macOS terminals start login shells, which read only the first of these that
            // exists; .bashrc is never read. Creating .bash_profile when none exists is safe.
            Ok([".bash_profile", ".bash_login", ".profile"]
                .iter()
                .map(|f| home().join(f))
                .find(|p| p.exists())
                .unwrap_or_else(|| home().join(".bash_profile")))
        }
        "bash" => Ok(home().join(".bashrc")),
        other => Err(format!(
            "shell `{other}` is not supported by `recompact install`; define `claude` to run \
`{} shell \"$@\"` yourself",
            recompact_home().join("bin").join("recompact").display()
        )),
    }
}

/// The shell block `install` writes: interactive `claude` runs through the launcher, and falls
/// back to plain claude whenever the launcher is missing.
/// `$HOME/...` keeps a path valid for a dotfiles repo shared across machines.
fn home_relative(path: &Path) -> String {
    match path.strip_prefix(home()) {
        Ok(rel) => format!("$HOME/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

pub fn shell_block(launcher: &Path) -> String {
    let l = home_relative(launcher);
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

/// Turn on Claude Code's auto-update for this plugin's marketplace (off by default for
/// third-party marketplaces) by adding `"autoUpdate": true` to its entry under
/// `extraKnownMarketplaces` in the user's settings. A targeted insertion, not a rewrite: the rest
/// of the file keeps its order and formatting, and the result must parse and differ only by that
/// key. `Ok(false)` when already on or when there is no such entry (a directory source, or a
/// marketplace declared in a project's settings).
pub fn with_auto_update(text: &str) -> Result<Option<String>, String> {
    let before: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let Some(entry) = before.pointer("/extraKnownMarketplaces/segment-recompact") else {
        return Ok(None);
    };
    if entry.get("autoUpdate") == Some(&json!(true))
        || entry.pointer("/source/source") == Some(&json!("directory"))
    {
        return Ok(None);
    }
    if entry.get("autoUpdate").is_some() {
        return Err("autoUpdate is set to something else; change it in /plugin".into());
    }
    let section = text
        .find("\"extraKnownMarketplaces\"")
        .ok_or("extraKnownMarketplaces not found")?;
    let key = text[section..]
        .find("\"segment-recompact\"")
        .map(|i| section + i)
        .ok_or("marketplace entry not found")?;
    let brace = text[key..]
        .find('{')
        .map(|i| key + i)
        .ok_or("marketplace entry is not an object")?;
    if !text[key + "\"segment-recompact\"".len()..brace]
        .chars()
        .all(|c| c.is_whitespace() || c == ':')
    {
        return Err("unexpected layout".into());
    }
    let empty = text[brace + 1..].trim_start().starts_with('}');
    let insert = if empty {
        "\"autoUpdate\": true"
    } else {
        "\"autoUpdate\": true,"
    };
    let out = format!("{}{insert}{}", &text[..=brace], &text[brace + 1..]);
    let mut expected = before.clone();
    expected["extraKnownMarketplaces"]["segment-recompact"]["autoUpdate"] = json!(true);
    let after: Value =
        serde_json::from_str(&out).map_err(|e| format!("edit broke the JSON: {e}"))?;
    if after != expected {
        return Err("edit changed more than autoUpdate".into());
    }
    Ok(Some(out))
}

fn claude_settings() -> PathBuf {
    home().join(".claude").join("settings.json")
}

/// Enable marketplace auto-update in the user's settings; a description of what happened.
fn enable_auto_update(settings: &Path) -> String {
    let Ok(text) = fs::read_to_string(settings) else {
        return "auto-update: no user settings file; enable it in /plugin → Marketplaces".into();
    };
    match with_auto_update(&text) {
        Ok(Some(updated)) => {
            let _ = fs::write(settings.with_extension("json.recompact-backup"), &text);
            let tmp = settings.with_extension("json.recompact-tmp");
            if fs::write(&tmp, &updated).is_ok() && fs::rename(&tmp, settings).is_ok() {
                "auto-update: turned on for the segment-recompact marketplace".into()
            } else {
                "auto-update: could not write the settings file".into()
            }
        }
        Ok(None) => "auto-update: nothing to change".into(),
        Err(e) => format!("auto-update: left as is ({e})"),
    }
}

/// `recompact install [--rc <file>]`: make interactive `claude` run through the launcher, and
/// turn on auto-update for the plugin. Safe to run again: it only fills in what is missing.
pub fn cmd_install(args: &[String]) -> i32 {
    let (_, opts) = parse_opts(args);
    let rc = opts.get("rc").and_then(|v| v.as_str()).map(PathBuf::from);
    let (ok, lines) = install(rc.as_deref());
    for l in &lines {
        if ok {
            println!("{l}");
        } else {
            eprintln!("install: {l}");
        }
    }
    if ok {
        0
    } else {
        1
    }
}

/// Add the shell block (and, for the user's own rc file, marketplace auto-update). Safe to run
/// again. Returns success and what to tell the user.
pub fn install(rc: Option<&Path>) -> (bool, Vec<String>) {
    let custom = rc.is_some();
    let rc = match rc.map(Path::to_path_buf).map(Ok).unwrap_or_else(rc_file) {
        Ok(p) => p,
        Err(e) => return (false, vec![e]),
    };
    let launcher = recompact_home().join("bin").join("recompact");
    let text = fs::read_to_string(&rc).unwrap_or_default();
    let updated = match with_block(&text, &shell_block(&launcher)) {
        Ok(t) => t,
        Err(e) => return (false, vec![format!("not changing {}: {e}", rc.display())]),
    };
    let mut lines = Vec::new();
    if updated != text {
        if !text.is_empty() {
            let _ = fs::write(rc.with_extension("recompact-backup"), &text);
        }
        if let Err(e) = fs::write(&rc, &updated) {
            return (
                false,
                vec![format!(
                    "cannot write {}: {e}. Run `{} install` in a terminal outside claude.",
                    rc.display(),
                    launcher.display()
                )],
            );
        }
        lines.push(format!(
            "shell: claude now runs through recompact ({})",
            rc.display()
        ));
    } else {
        lines.push(format!("shell: already set up ({})", rc.display()));
    }
    let _ = fs::remove_file(recompact_home().join("declined"));
    if !custom {
        lines.push(enable_auto_update(&claude_settings()));
        lines.push(crate::statusline::wrap_statusline(
            &claude_settings(),
            &home_relative(&launcher),
        ));
    }
    lines.push(
        "Next: open a new terminal and start claude as usual. Sessions already running keep the \
old setup until restarted. Check with `recompact doctor`; undo with `recompact uninstall`."
            .into(),
    );
    (true, lines)
}

/// Why this claude cannot compact in place, if it cannot, and what fixes it. `None` when it
/// runs under the launcher or is a background job recompact drives (`managed`), or the user
/// removed the setup on purpose.
pub fn setup_gap(managed: bool) -> Option<String> {
    if managed || recompact_home().join("declined").exists() {
        return None;
    }
    if let Some(job) = Job::from_env() {
        return Some(unidentified_job(&job));
    }
    match rc_file() {
        Err(_) => Some(
            "recompact: this shell is not zsh or bash, so /recompact cannot compact in place here \
(it compacts and prints a resume command instead). See `recompact install`."
                .into(),
        ),
        Ok(rc) => {
            let text = fs::read_to_string(&rc).unwrap_or_default();
            if text.contains(BLOCK_START) {
                Some(
                    "recompact: this claude was not started through recompact (a terminal opened \
before setup, an editor, or the desktop app), so /recompact here needs a restart. Open a new \
terminal and run claude there to compact in place."
                        .into(),
                )
            } else if with_block(&text, "").is_err() {
                Some(format!(
                    "recompact: {} defines `claude` itself, so recompact cannot wrap it. Remove \
that definition, then type /recompact setup.",
                    rc.display()
                ))
            } else {
                Some(
                    "recompact: in-place compaction is not set up yet. Type /recompact setup \
once, then open a new terminal."
                        .into(),
                )
            }
        }
    }
}

/// A background job whose claude `runs_job` cannot confirm (Claude Code changed how it records
/// sessions): no setup or new terminal changes that.
fn unidentified_job(job: &Job) -> String {
    format!(
        "recompact: background job {} cannot compact in place (recompact could not identify its \
claude process), so /recompact here compacts and prints a resume command.",
        job.short
    )
}

/// At session start, say once a day when this claude cannot compact in place and why.
pub fn setup_notice(input: &Value) -> Option<String> {
    let source = get_s(input, "source").unwrap_or("");
    if !matches!(source, "startup" | "resume") {
        return None;
    }
    let under = Shell::from_env().is_some_and(|s| s.is_own_claude());
    let gap = setup_gap(under)?;
    let mark = recompact_home().join("setup-notice.json");
    let now = crate::now_unix();
    if read_json(&mark)
        .and_then(|v| v.get("at").and_then(|a| a.as_i64()))
        .is_some_and(|at| now - at < 24 * 3600)
    {
        return None;
    }
    let _ = fs::create_dir_all(recompact_home());
    write_json(&mark, &json!({"at": now}));
    Some(gap)
}

// ------------------------------------------------------------------------------------ doctor

/// The installed plugin, from Claude Code's own record: (version, install path).
fn installed_plugin(home: &Path) -> Option<(String, PathBuf)> {
    let v = read_json(
        &home
            .join(".claude")
            .join("plugins")
            .join("installed_plugins.json"),
    )?;
    let plugins = v.get("plugins").unwrap_or(&v);
    let entry = plugins
        .get("segment-recompact@segment-recompact")?
        .as_array()?
        .first()?;
    Some((
        get_s(entry, "version")?.to_string(),
        PathBuf::from(get_s(entry, "installPath")?),
    ))
}

fn marketplace_source(home: &Path) -> Option<Value> {
    read_json(&home.join(".claude").join("settings.json"))?
        .pointer("/extraKnownMarketplaces/segment-recompact")
        .cloned()
}

/// Every check an installer (person or agent) needs: each line is `ok`, `fix` (with the command
/// that fixes it), or `note`. Returns (lines, all ok).
pub fn doctor_report(home: &Path, rc: Option<&Path>) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut ok = true;
    let fix = |lines: &mut Vec<String>, s: String| {
        lines.push(format!("fix   {s}"));
    };
    let installed = installed_plugin(home);
    match &installed {
        Some((v, _)) => lines.push(format!("ok    plugin segment-recompact {v} is installed")),
        None => {
            ok = false;
            fix(&mut lines, "plugin not installed: curl -fsSL https://raw.githubusercontent.com/Don-Osipov/segment_recompact/main/install.sh | sh".into());
        }
    }
    let source = marketplace_source(home);
    let directory =
        source.as_ref().and_then(|s| s.pointer("/source/source")) == Some(&json!("directory"));
    match &source {
        _ if directory => lines.push(
            "note  marketplace is a local directory: update by pulling and building it".into(),
        ),
        Some(s) if s.get("autoUpdate") == Some(&json!(true)) => {
            lines.push("ok    auto-update is on: new versions arrive when claude starts".into())
        }
        Some(_) => {
            ok = false;
            fix(
                &mut lines,
                "auto-update is off, so new versions never arrive: recompact install".into(),
            );
        }
        None if installed.is_some() => lines.push(
            "note  marketplace is declared elsewhere (a project's settings): auto-update follows that".into(),
        ),
        None => {}
    }
    if let Some((v, _)) = &installed {
        let offered = read_json(
            &home
                .join(".claude/plugins/marketplaces/segment-recompact/plugins/segment_recompact/.claude-plugin/plugin.json"),
        )
        .and_then(|m| get_s(&m, "version").map(String::from));
        if let Some(o) = offered.filter(|o| o != v) {
            ok = false;
            fix(
                &mut lines,
                format!("version {o} is available (installed {v}): recompact update"),
            );
        }
    }
    let launcher = home.join(".claude/recompact/bin/recompact");
    let running = Command::new(&launcher)
        .arg("version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let want = installed.as_ref().map(|(v, _)| format!("recompact {v}"));
    match running {
        Some(v) if want.as_ref().is_some_and(|w| w != &v) => {
            ok = false;
            fix(
                &mut lines,
                format!(
                    "launcher runs {v} but {} is installed: recompact update",
                    want.unwrap_or_default()
                ),
            );
        }
        Some(v) => lines.push(format!("ok    launcher {} runs: {v}", launcher.display())),
        None => {
            ok = false;
            fix(
                &mut lines,
                format!(
                    "launcher {} missing or not runnable: recompact update",
                    launcher.display()
                ),
            );
        }
    }
    match rc {
        Some(rc) => {
            let text = fs::read_to_string(rc).unwrap_or_default();
            if text.contains(BLOCK_START) {
                lines.push(format!(
                    "ok    shell: claude runs through recompact ({})",
                    rc.display()
                ));
            } else if with_block(&text, "").is_err() {
                ok = false;
                fix(
                    &mut lines,
                    format!(
                        "{} defines `claude` itself: remove that definition, then run recompact install",
                        rc.display()
                    ),
                );
            } else {
                ok = false;
                fix(
                    &mut lines,
                    format!("shell not set up ({}): recompact install", rc.display()),
                );
            }
        }
        None => lines.push("note  shell is not zsh or bash: see `recompact install`".into()),
    }
    if std::env::var("CLAUDE_CODE_SESSION_ID").is_ok() {
        let shell = Shell::from_env().filter(|s| s.is_own_claude());
        lines.push(match (shell.as_ref().map(Shell::job), Job::from_env()) {
            (Some(Some(short)), _) => format!(
                "ok    this claude session is background job {short}: it compacts in place through \
`claude attach`"
            ),
            (Some(None), _) => "ok    this claude session runs through recompact".into(),
            (None, Some(job)) => format!(
                "note  {}",
                unidentified_job(&job).trim_start_matches("recompact: ")
            ),
            (None, None) => "note  this claude session started before setup: /recompact here \
compacts and prints a resume command; sessions started from a new terminal compact in place"
                .into(),
        });
    }
    let defaults = user_settings();
    lines.push(format!(
        "note  auto-compaction is {} by default{}; /recompact on turns it on per session",
        if defaults.get("auto") == Some(&json!(true)) {
            "ON"
        } else {
            "off"
        },
        get_u(&defaults, "at")
            .map(|a| format!(" (size {})", fmt_k(a)))
            .unwrap_or_default()
    ));
    (lines, ok)
}

/// `recompact doctor`: is everything installed and wired up; what exactly to run if not.
pub fn cmd_doctor(_args: &[String]) -> i32 {
    let (lines, ok) = doctor_report(&home(), rc_file().ok().as_deref());
    println!("recompact doctor ({})", env!("CARGO_PKG_VERSION"));
    for l in &lines {
        println!("  {l}");
    }
    println!(
        "{}",
        if ok {
            "All set."
        } else {
            "Run the commands after `fix`, then `recompact doctor` again."
        }
    );
    if ok {
        0
    } else {
        1
    }
}

/// `recompact update`: newest plugin version, binary, shell setup, then a doctor report. Safe to
/// run inside a claude session; the new version applies to sessions started afterwards.
pub fn cmd_update(_args: &[String]) -> i32 {
    let claude = std::env::var("RECOMPACT_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    if let Some(path) = marketplace_source(&home())
        .filter(|s| s.pointer("/source/source") == Some(&json!("directory")))
        .and_then(|s| {
            s.pointer("/source/path")
                .and_then(|p| p.as_str())
                .map(String::from)
        })
    {
        println!(
            "The marketplace is the local directory {path}: pull it and run `cargo build --release` \
in plugins/segment_recompact first; continuing with the plugin update."
        );
    }
    for args in [
        vec!["plugin", "marketplace", "update", "segment-recompact"],
        vec!["plugin", "update", "segment-recompact@segment-recompact"],
    ] {
        match Command::new(&claude).args(&args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("update: `claude {}` exited {s}", args.join(" "));
                return 1;
            }
            Err(e) => {
                eprintln!("update: cannot run {claude}: {e}");
                return 1;
            }
        }
    }
    let Some((version, path)) = installed_plugin(&home()) else {
        eprintln!("update: the plugin is not installed; run install.sh");
        return 1;
    };
    // The new version's launcher fetches its binary, repoints the stable link, and refreshes
    // the shell setup; then it reports.
    let launcher = path.join("bin").join("recompact");
    println!("Installed segment-recompact {version}.");
    let _ = Command::new(&launcher).arg("install").status();
    Command::new(&launcher)
        .arg("doctor")
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(1)
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
            let _ = fs::create_dir_all(recompact_home());
            let _ = fs::write(recompact_home().join("declined"), "");
            println!(
                "Removed from {}. New terminals run plain claude.",
                rc.display()
            );
            if let Some(line) = crate::statusline::unwrap_statusline(&claude_settings()) {
                println!("{line}");
            }
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
        _ => user_at(session).unwrap_or_else(|| default_at(1_000_000)),
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
        match Job::from_env() {
            Some(job) => {
                text.push(' ');
                text.push_str(unidentified_job(&job).trim_start_matches("recompact: "));
            }
            None => text.push_str(
                " This claude was not started through recompact, so it cannot compact in place: \
run /recompact setup once, then open a new terminal.",
            ),
        }
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
        .or_else(own_session);
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
