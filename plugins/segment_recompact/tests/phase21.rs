//! Background jobs (`claude --bg`, agent view): the daemon runs claude on a terminal of its own,
//! so no launcher is there. The hooks find the job from its folder and start a worker, which
//! compacts, opens the job with `claude attach`, types `/resume <twin>`, and detaches. When that
//! does not take, the twin starts as a new job and the old one is stopped.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use recompact::*;
use serde_json::{json, Value};

const SESSION: &str = "5e5516a0-0000-4000-8000-000000000021";

/// Hooks read the user's switch from RECOMPACT_HOME; point this test binary at an empty one.
fn isolate() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let home = std::env::temp_dir().join(format!("recompact-test-home-{}", uuid_v4()));
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("settings.json"), r#"{"auto": true}"#).unwrap();
        std::env::set_var("RECOMPACT_HOME", home);
    });
}

fn tmp_dir() -> PathBuf {
    isolate();
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn read(p: PathBuf) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
}

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    json!({"type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
           "timestamp": "2026-10-07T10:00:00.000Z", "userType": "external", "isSidechain": false,
           "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
}

fn assistant(uuid: &str, parent: &str, text: &str, prompt_tokens: Option<u64>) -> Value {
    let mut r = json!({"type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
           "timestamp": "2026-10-07T10:00:01.000Z", "userType": "external", "isSidechain": false,
           "message": {"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
                       "type": "message", "stop_reason": "end_turn",
                       "content": [{"type": "text", "text": text}]}});
    if let Some(t) = prompt_tokens {
        r["message"]["usage"] = json!({"input_tokens": 5, "cache_read_input_tokens": t - 5,
                                       "cache_creation_input_tokens": 0, "output_tokens": 10});
    }
    r
}

fn append(path: &Path, records: &[Value]) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for r in records {
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
}

/// A session big enough for mask mode to shrink: an old turn with a large tool result.
fn big_session(dir: &Path, prompt_tokens: u64) -> PathBuf {
    let big = "x".repeat(600_000);
    let p = dir.join(format!("{SESSION}.jsonl"));
    append(
        &p,
        &[
            user("u1", None, "read the big file"),
            json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": SESSION,
                   "timestamp": "2026-10-07T10:00:01.000Z", "userType": "external", "isSidechain": false,
                   "message": {"id": "msg_a1", "role": "assistant", "model": "claude-opus-5-5",
                       "type": "message", "stop_reason": "tool_use",
                       "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/big"}}]}}),
            json!({"type": "user", "uuid": "r1", "parentUuid": "a1", "sessionId": SESSION,
                   "timestamp": "2026-10-07T10:00:02.000Z", "userType": "external", "isSidechain": false,
                   "sourceToolAssistantUUID": "a1",
                   "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": big}]}}),
            assistant("a2", "r1", "read it", None),
            user("u2", Some("a2"), "thanks"),
            assistant("a3", "u2", "done", Some(prompt_tokens)),
            json!({"type": "system", "subtype": "stop_hook_summary", "sessionId": SESSION}),
            json!({"type": "system", "subtype": "turn_duration", "sessionId": SESSION}),
            json!({"type": "last-prompt", "leafUuid": "a3", "sessionId": SESSION, "lastPrompt": "thanks"}),
        ],
    );
    p
}

fn write_stub(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

/// One folder for every job of this test binary, as `~/.claude/jobs` holds them all: a worker
/// forgets the state of jobs no longer there.
fn jobs_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("recompact-test-jobs-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    root
}

/// A job folder as the daemon writes it, idle between turns.
fn job_folder(extra: Value) -> (Job, PathBuf) {
    isolate();
    let short = uuid_v4()[..8].to_string();
    let dir = jobs_root().join(&short);
    fs::create_dir_all(&dir).unwrap();
    let mut state = json!({
        "state": "done", "tempo": "idle", "backend": "daemon", "daemonShort": short,
        "inFlight": {"tasks": 0, "queued": 0, "kinds": [], "drainableMonitors": 0},
        "sessionId": SESSION, "resumeSessionId": SESSION, "cwd": "/tmp",
        "respawnFlags": ["--model", "haiku"],
    });
    if let (Some(s), Some(e)) = (state.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            s.insert(k.clone(), v.clone());
        }
    }
    fs::write(dir.join("state.json"), state.to_string()).unwrap();
    (Job::at(&dir).expect("a job"), dir)
}

/// The job's shared state, with its session recorded as its SessionStart hook would.
fn job_state(job: &Job, transcript: &Path, config: Value) -> Shell {
    let mut shell = job_shell(job, std::process::id());
    for (k, v) in config.as_object().unwrap() {
        shell.config[k] = v.clone();
    }
    fs::write(
        shell.dir.join("session.json"),
        json!({"session": SESSION, "transcript": transcript}).to_string(),
    )
    .unwrap();
    shell
}

fn reload(shell: &Shell) -> Shell {
    Shell {
        dir: shell.dir.clone(),
        config: shell.config.clone(),
    }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------------------------ finding a job

#[test]
fn a_background_job_is_its_folder_with_a_daemon_state_and_its_own_claude() {
    let (job, dir) = job_folder(json!({}));
    assert_eq!(
        job.short,
        dir.file_name().unwrap().to_str().unwrap(),
        "from daemonShort"
    );
    let no_short = tmp_dir().join("0b5e55ed");
    fs::create_dir_all(&no_short).unwrap();
    fs::write(no_short.join("state.json"), r#"{"backend": "daemon"}"#).unwrap();
    assert_eq!(
        Job::at(&no_short).unwrap().short,
        "0b5e55ed",
        "else the folder's name"
    );
    let other = tmp_dir();
    fs::write(
        other.join("state.json"),
        r#"{"backend": "tmux", "daemonShort": "abc"}"#,
    )
    .unwrap();
    assert!(Job::at(&other).is_none(), "only the daemon's jobs");
    fs::write(
        other.join("state.json"),
        r#"{"backend": "daemon", "daemonShort": "../x"}"#,
    )
    .unwrap();
    assert!(Job::at(&other).is_none(), "a short id names a folder");
    assert!(Job::at(&tmp_dir()).is_none(), "no state");

    // The job's claude is the session Claude Code records with the job's short id; a claude
    // started inside the job inherits CLAUDE_JOB_DIR but is recorded without it.
    let sessions = tmp_dir();
    fs::write(
        sessions.join("4242.json"),
        json!({"pid": 4242, "kind": "bg", "jobId": job.short}).to_string(),
    )
    .unwrap();
    fs::write(
        sessions.join("4343.json"),
        json!({"pid": 4343, "kind": "interactive", "entrypoint": "sdk-cli"}).to_string(),
    )
    .unwrap();
    assert!(runs_job(&sessions, 4242, &job.short));
    assert!(!runs_job(&sessions, 4343, &job.short));
    assert!(!runs_job(&sessions, 4444, &job.short));
    assert!(!runs_job(&sessions, 4242, "someone"));
}

#[test]
fn a_job_keeps_the_launchers_defaults_switches_in_place_and_remembers_its_rearm() {
    let (job, _) = job_folder(json!({"respawnFlags": ["--model", "haiku", "first prompt"]}));
    let shell = job_shell(&job, 4242);
    assert_eq!(shell.job(), Some(job.short.as_str()));
    assert_eq!(shell.config["child"], 4242);
    assert_eq!(shell.config["inplace"], true);
    assert_eq!(shell.config["launch_model"], "haiku");
    assert!(shell.dir.ends_with(format!("jobs/{}", job.short)));
    fs::write(shell.dir.join("config.json"), r#"{"rearm": 345000}"#).unwrap();
    assert_eq!(job_shell(&job, 4242).config["rearm"], 345000);
}

// ------------------------------------------------------------------------------ hooks

/// Records how the hooks start the worker, instead of starting it.
fn worker_stub(dir: &Path) -> String {
    write_stub(
        dir,
        "worker-stub.sh",
        "#!/bin/sh\necho \"$*\" >> \"$(dirname \"$0\")/worker.log\"\n",
    )
}

#[test]
fn in_a_job_the_stop_hook_asks_for_a_switch_and_starts_the_worker() {
    let dir = tmp_dir();
    let t = big_session(&dir, 200_000);
    let (job, job_dir) = job_folder(json!({}));
    let shell = job_state(
        &job,
        &t,
        json!({"auto": true, "at": 150_000, "summarize": false, "worker_bin": worker_stub(&dir)}),
    );
    let input = json!({"session_id": SESSION, "transcript_path": t, "cwd": "/tmp",
        "hook_event_name": "Stop", "stop_hook_active": false, "session_crons": [],
        "background_tasks": [{"id": "b1", "type": "shell", "status": "running", "command": "sleep 600"}]});
    let out = on_stop_in(Some(reload(&shell)), &input).expect("handoff");
    let msg = out["systemMessage"].as_str().unwrap();
    assert!(msg.contains("compacting in the background"), "{msg}");
    assert!(!msg.contains("setup") && !msg.contains("terminal"), "{msg}");
    let req = read(shell.dir.join("request.json")).unwrap();
    assert_eq!(req["ready"], true);
    assert_eq!(req["carry"]["tasks"][0]["command"], "sleep 600");
    let log = dir.join("worker.log");
    wait_for("the worker", || log.exists());
    assert_eq!(
        fs::read_to_string(&log).unwrap().trim(),
        format!("job-handoff {} {}", job_dir.display(), std::process::id())
    );

    // While a worker has that handoff under way, later turns ask nothing more.
    fs::remove_file(shell.dir.join("request.json")).unwrap();
    fs::write(
        shell.dir.join("handoff.json"),
        json!({"session": SESSION, "pid": std::process::id()}).to_string(),
    )
    .unwrap();
    assert!(on_stop_in(Some(reload(&shell)), &input).is_none());
    // A worker that died leaves no handoff under way.
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    fs::write(
        shell.dir.join("handoff.json"),
        json!({"session": SESSION, "pid": dead}).to_string(),
    )
    .unwrap();
    assert!(on_stop_in(Some(reload(&shell)), &input).is_some());
    assert_eq!(read(shell.dir.join("request.json")).unwrap()["ready"], true);
}

#[test]
fn a_job_started_before_the_hooks_were_on_tracks_the_session_its_hooks_report() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let (job, _) = job_folder(json!({}));
    let mut shell = job_shell(&job, std::process::id());
    shell.config["worker_bin"] = json!(worker_stub(&dir));
    assert!(shell.session().is_none());
    let input = json!({"session_id": SESSION, "transcript_path": t, "prompt": "/recompact"});
    let out = on_prompt_in(Some(reload(&shell)), &input).expect("handled");
    assert_eq!(out["decision"], "block");
    assert!(out["reason"]
        .as_str()
        .unwrap()
        .contains("switches to the compacted copy"));
    assert_eq!(shell.session().unwrap()["session"], SESSION);
    assert_eq!(read(shell.dir.join("request.json")).unwrap()["ready"], true);
    wait_for("the worker", || dir.join("worker.log").exists());
}

#[test]
fn a_notice_is_shown_once_to_the_session_it_is_for() {
    isolate();
    let id = uuid_v4();
    assert!(take_notice(&id).is_none());
    // Left by a worker for a session that opens later; only that session takes it.
    let path = std::env::var("RECOMPACT_HOME").unwrap();
    let notices = Path::new(&path).join("notices");
    fs::create_dir_all(&notices).unwrap();
    fs::write(
        notices.join(format!("{id}.json")),
        json!({"session": id, "message": "moved"}).to_string(),
    )
    .unwrap();
    assert!(take_notice(&uuid_v4()).is_none());
    assert_eq!(take_notice(&id).as_deref(), Some("moved"));
    assert!(take_notice(&id).is_none(), "once");
}

// ------------------------------------------------------------------------------ the worker

/// `claude`, as the worker uses it: `attach <short>` reads keys raw, as Claude Code does, and
/// opens a session given `/resume <id>` (its SessionStart hook would write session.json);
/// Ctrl+Z detaches. `--bg`, `agents --json`, and `stop` act like a daemon that starts the twin
/// as a new job, folder included (a worker forgets the state of jobs with no folder). `mode`: `switch`, `ignore` (never opens the twin), `draft` (the first Esc finds
/// text in the box).
fn claude_stub(dir: &Path, state: &Path, mode: &str) -> String {
    write_stub(
        dir,
        "claude-stub.sh",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "run $*" >> "$D/runs.log"
case "$1" in
  attach)
    stty raw -echo 2>/dev/null
    printf 'attached %s\r\n' "$2"
    ESC=$(printf '\033'); SUB=$(printf '\032'); CR=$(printf '\r')
    line=""
    while :; do
      c=$(dd bs=1 count=1 2>/dev/null)
      [ -z "$c" ] && exit 0
      case "$c" in
        "$SUB") echo "detach" >> "$D/typed.log"; exit 0;;
        "$ESC")
          echo "esc" >> "$D/typed.log"
          if [ "{mode}" = draft ] && [ ! -e "$D/drafted" ]; then
            touch "$D/drafted"; printf 'Esc again to clear\r\n'
          fi;;
        "$CR")
          echo "got $line" >> "$D/typed.log"
          case "$line" in
            */resume\ *)
              [ "{mode}" = ignore ] || printf '{{"session":"%s"}}' "${{line##*/resume }}" > "{state}/session.json";;
          esac
          line="";;
        *) line="$line$c";;
      esac
    done;;
  --bg)
    echo "$*" >> "$D/bg.log"
    twin=$(echo "$*" | sed -n 's/.*--resume \([^ ]*\).*/\1/p')
    echo "$twin" > "$D/twin.txt"
    # The daemon makes the new job's folder before it lists the job.
    s=$(echo "$twin" | cut -c1-8)
    mkdir -p "{jobs}/$s"
    printf '{{"backend":"daemon","daemonShort":"%s","sessionId":"%s"}}' "$s" "$twin" > "{jobs}/$s/state.json"
    printf 'backgrounded · %s · a name (idle — send a prompt to start)\n' "$(echo "$twin" | cut -c1-8)";;
  agents)
    twin=$(cat "$D/twin.txt" 2>/dev/null)
    # Listed by its short id only: the worker must have read it from what --bg printed.
    printf '[{{"id":"%s","kind":"background","pid":4242,"sessionId":"elsewhere","status":"idle"}}]\n' "$(echo "$twin" | cut -c1-8)";;
  stop) echo "$2" >> "$D/stopped.log";;
esac
exit 0
"#,
            state = state.display(),
            jobs = jobs_root().display()
        ),
    )
}

/// A ready `/recompact` for the job, with background work running.
fn ready_request(shell: &Shell, t: &Path, kick: bool) {
    fs::write(
        shell.dir.join("request.json"),
        json!({"session": SESSION, "transcript": t, "reason": "manual", "force": true,
               "ready": true, "kick": kick,
               "carry": {"tasks": [{"command": "sleep 600", "description": "long job"}], "crons": []}})
        .to_string(),
    )
    .unwrap();
}

fn run_worker(job_dir: &Path, project: &Path, stub: &str) -> i32 {
    cmd_job_handoff(&s(&[
        job_dir.to_str().unwrap(),
        &std::process::id().to_string(),
        "--claude-bin",
        stub,
        "--mask",
        "--switch-timeout",
        "1",
        "--dir",
        project.to_str().unwrap(),
    ]))
}

fn lines(p: PathBuf) -> Vec<String> {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn the_worker_switches_the_job_in_place_through_claude_attach() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let (job, job_dir) = job_folder(json!({}));
    let shell = job_state(&job, &t, json!({}));
    ready_request(&shell, &t, true);
    let stub = claude_stub(&dir, &shell.dir, "switch");
    assert_eq!(run_worker(&job_dir, &dir, &stub), 0);

    let twin = lineage_latest(&dir, SESSION);
    assert_ne!(twin, SESSION, "the session was compacted");
    let runs = lines(dir.join("runs.log"));
    assert_eq!(
        runs,
        [format!("run attach {}", job.short)],
        "attached once; nothing restarted"
    );
    let typed = lines(dir.join("typed.log"));
    assert_eq!(typed[0], "esc", "Esc first, alone: {typed:?}");
    assert_eq!(typed[1], format!("got /resume {twin}"), "{typed:?}");
    assert!(
        typed[2].starts_with("got Continue the work"),
        "the request asked to carry on: {typed:?}"
    );
    assert_eq!(typed.last().unwrap(), "detach", "{typed:?}");
    assert_eq!(
        read(shell.dir.join("session.json")).unwrap()["session"],
        twin
    );
    assert!(
        read(shell.dir.join("config.json")).unwrap()["rearm"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        read(shell.dir.join("switch.json")).unwrap()["twin"],
        twin,
        "left for the twin's SessionStart hook"
    );
    for gone in ["request.json", "handoff.json", "worker.json"] {
        assert!(!shell.dir.join(gone).exists(), "{gone}");
    }
}

#[test]
fn a_busy_job_is_left_alone_until_its_turn_ends() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    // A background task runs after the turn ended: `state` stays "working", `tempo` is idle.
    let (job, job_dir) = job_folder(json!({"state": "working", "tempo": "active",
        "inFlight": {"tasks": 1, "queued": 0, "kinds": ["local_bash"], "drainableMonitors": 0}}));
    let shell = job_state(&job, &t, json!({}));
    ready_request(&shell, &t, false);
    let stub = claude_stub(&dir, &shell.dir, "switch");
    let state = job_dir.join("state.json");
    let idle_later = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(4));
        // The turn ended asking the user something; a stale queue count stays (measured).
        let mut s: Value = serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
        s["tempo"] = json!("blocked");
        s["inFlight"]["queued"] = json!(1);
        fs::write(&state, s.to_string()).unwrap();
        Instant::now()
    });
    assert_eq!(run_worker(&job_dir, &dir, &stub), 0);
    let idle_at = idle_later.join().unwrap();
    let attached = fs::metadata(dir.join("runs.log"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(
        attached.elapsed().unwrap() <= idle_at.elapsed(),
        "attached only once the turn was over"
    );
    let twin = lineage_latest(&dir, SESSION);
    assert!(lines(dir.join("typed.log")).contains(&format!("got /resume {twin}")));
}

#[test]
fn a_draft_in_the_jobs_input_box_holds_the_switch_until_it_is_gone() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let (job, job_dir) = job_folder(json!({}));
    let shell = job_state(&job, &t, json!({}));
    ready_request(&shell, &t, false);
    let stub = claude_stub(&dir, &shell.dir, "draft");
    assert_eq!(run_worker(&job_dir, &dir, &stub), 0);
    let twin = lineage_latest(&dir, SESSION);
    let typed = lines(dir.join("typed.log"));
    // First visit: one Esc, the hint, nothing typed, detached (a second Esc would clear it).
    assert_eq!(&typed[..2], ["esc", "detach"], "{typed:?}");
    // Later: Esc finds the box empty, and the switch goes.
    assert_eq!(
        &typed[2..],
        [
            "esc".to_string(),
            format!("got /resume {twin}"),
            "detach".into()
        ],
        "{typed:?}"
    );
    assert_eq!(
        read(shell.dir.join("session.json")).unwrap()["session"],
        twin
    );
}

#[test]
fn a_job_that_will_not_switch_restarts_as_a_new_job_on_the_twin() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let (job, job_dir) = job_folder(json!({"name": "fix the flaky test",
        "respawnFlags": ["--agent", "claude", "--model", "opus",
        "--permission-mode", "bypassPermissions", "the job's first prompt"]}));
    let shell = job_state(&job, &t, json!({}));
    ready_request(&shell, &t, false);
    let stub = claude_stub(&dir, &shell.dir, "ignore");
    assert_eq!(run_worker(&job_dir, &dir, &stub), 0);

    let twin = lineage_latest(&dir, SESSION);
    assert_ne!(twin, SESSION);
    let typed = lines(dir.join("typed.log"));
    assert_eq!(
        typed
            .iter()
            .filter(|l| **l == format!("got /resume {twin}"))
            .count(),
        2,
        "typed twice: {typed:?}"
    );
    let bg = fs::read_to_string(dir.join("bg.log")).unwrap();
    assert!(bg.starts_with("--bg "), "{bg}");
    assert!(bg.contains(&format!("--resume {twin}")), "{bg}");
    assert!(
        bg.contains("--agent claude") && bg.contains("--permission-mode bypassPermissions"),
        "a job keeps how it runs, permission mode included: {bg}"
    );
    assert!(
        bg.contains("--model opus"),
        "the model it was started with: {bg}"
    );
    assert!(
        bg.contains("--name fix the flaky test"),
        "and the name it showed in agent view: {bg}"
    );
    assert!(
        !bg.contains("first prompt"),
        "not its first prompt again: {bg}"
    );
    assert!(
        bg.contains("Start again the ones that are still needed") && bg.contains("sleep 600"),
        "a new process ends the old one's background work, so the twin restarts it: {bg}"
    );
    assert_eq!(
        lines(dir.join("stopped.log")),
        [job.short.clone()],
        "the old job is stopped once the new one runs"
    );
    let notice = take_notice(&twin).expect("the twin hears what happened");
    assert!(notice.contains(&job.short), "{notice}");
    // Another job's first hook forgets the state of jobs with no folder; the new job has one.
    let (other, _) = job_folder(json!({}));
    job_shell(&other, std::process::id());
    assert!(
        read(job_state_dir(&twin[..8]).join("config.json")).unwrap()["rearm"]
            .as_u64()
            .unwrap()
            > 0,
        "the new job is re-armed like a switch"
    );
}

#[test]
fn a_job_that_ended_is_not_switched() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let (job, job_dir) = job_folder(json!({}));
    let shell = job_state(&job, &t, json!({}));
    ready_request(&shell, &t, false);
    let stub = claude_stub(&dir, &shell.dir, "switch");
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    let rc = cmd_job_handoff(&s(&[
        job_dir.to_str().unwrap(),
        &dead.to_string(),
        "--claude-bin",
        &stub,
        "--mask",
    ]));
    assert_eq!(rc, 0);
    assert!(!dir.join("runs.log").exists(), "never attached");
    assert_eq!(lineage_latest(&dir, SESSION), SESSION, "nothing compacted");
}

#[test]
fn terminal_output_reads_as_text_without_its_escapes() {
    let drawn = b"\x1b[2K\x1b[1G\x1b[38;2;153;153;153mEsc\x1b[1C again\x1b]0;\xe2\x9c\xb3 Ready\x07 to\r\nclear\x1b[0m";
    assert_eq!(plain_text(drawn), "Escagaintoclear");
    assert_eq!(plain_text("❯ /resume".as_bytes()), "❯/resume");
}
