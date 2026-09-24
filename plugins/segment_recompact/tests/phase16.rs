//! Handoff in place: `recompact shell` runs claude, hooks ask for a handoff, and the launcher
//! compacts and resumes in the same terminal.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use recompact::*;
use serde_json::{json, Value};

const SESSION: &str = "5e5516a0-0000-4000-8000-000000000001";

/// Hooks read the user's switch from RECOMPACT_HOME; point this test binary at an empty one so
/// a real `/recompact off` never changes the outcome. (phase17 tests the switch itself.)
fn isolate() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let home = std::env::temp_dir().join(format!("recompact-test-home-{}", uuid_v4()));
        fs::create_dir_all(&home).unwrap();
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

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    json!({"type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
           "timestamp": "2026-09-24T10:00:00.000Z", "userType": "external", "isSidechain": false,
           "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
}

fn assistant(uuid: &str, parent: &str, text: &str, prompt_tokens: Option<u64>) -> Value {
    let mut r = json!({"type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
           "timestamp": "2026-09-24T10:00:01.000Z", "userType": "external", "isSidechain": false,
           "message": {"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
                       "type": "message", "stop_reason": "end_turn",
                       "content": [{"type": "text", "text": text}]}});
    if let Some(t) = prompt_tokens {
        r["message"]["usage"] = json!({"input_tokens": 5, "cache_read_input_tokens": t - 5,
                                       "cache_creation_input_tokens": 0, "output_tokens": 10});
    }
    r
}

fn write_lines(path: &Path, records: &[Value]) {
    let mut f = fs::File::create(path).unwrap();
    for r in records {
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
}

/// A session big enough for mask mode to shrink: an old turn with a large tool result.
fn big_session(dir: &Path, prompt_tokens: u64) -> PathBuf {
    let big = "x".repeat(600_000);
    let records = vec![
        user("u1", None, "read the big file"),
        json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": SESSION,
               "timestamp": "2026-09-24T10:00:01.000Z", "userType": "external", "isSidechain": false,
               "message": {"id": "msg_a1", "role": "assistant", "model": "claude-opus-5-5",
                   "type": "message", "stop_reason": "tool_use",
                   "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/big"}}]}}),
        json!({"type": "user", "uuid": "r1", "parentUuid": "a1", "sessionId": SESSION,
               "timestamp": "2026-09-24T10:00:02.000Z", "userType": "external", "isSidechain": false,
               "sourceToolAssistantUUID": "a1",
               "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": big}]}}),
        assistant("a2", "r1", "read it", None),
        user("u2", Some("a2"), "thanks"),
        assistant("a3", "u2", "done", Some(prompt_tokens)),
        json!({"type": "last-prompt", "leafUuid": "a3", "sessionId": SESSION, "lastPrompt": "thanks"}),
    ];
    let p = dir.join(format!("{SESSION}.jsonl"));
    write_lines(&p, &records);
    p
}

fn write_stub(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

/// A launcher state dir tracking SESSION, as the SessionStart hook would leave it.
fn shell_state(config: Value, transcript: &Path) -> Shell {
    let dir = tmp_dir();
    let mut config = config;
    config["launcher"] = json!(std::process::id());
    fs::write(dir.join("config.json"), config.to_string()).unwrap();
    fs::write(
        dir.join("session.json"),
        json!({"session": SESSION, "transcript": transcript, "cwd": "/tmp", "model": "claude-opus-5-5[1m]"}).to_string(),
    )
    .unwrap();
    Shell { dir, config }
}

fn reload(shell: &Shell) -> Shell {
    Shell {
        dir: shell.dir.clone(),
        config: shell.config.clone(),
    }
}

fn read(p: PathBuf) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
}

fn stop_input(transcript: &Path) -> Value {
    json!({"session_id": SESSION, "transcript_path": transcript, "cwd": "/tmp",
           "hook_event_name": "Stop", "stop_hook_active": false,
           "background_tasks": [], "session_crons": []})
}

// ------------------------------------------------------------------------------ argument handling

#[test]
fn relaunch_keeps_flags_and_drops_the_prompt_and_session_selection() {
    let args = parse_claude_args(&s(&[
        "--model",
        "opus",
        "--effort",
        "max",
        "--add-dir",
        "a",
        "b",
        "-r",
        "abc",
        "--append-system-prompt-file",
        "rules.md",
        "--dangerously-skip-permissions",
        "--permission-mode",
        "plan",
        "-w",
        "feat",
        "--tmux",
        "--settings=x.json",
        "fix the bug",
    ]));
    assert_eq!(
        carry_args(&args),
        s(&[
            "--add-dir",
            "a",
            "b",
            "--append-system-prompt-file",
            "rules.md",
            // Bypass stays available; the session restores the mode it was actually in.
            "--allow-dangerously-skip-permissions",
            "--settings=x.json",
        ])
    );
    assert!(!is_passthrough(&args));
    for p in [
        &["-p", "hi"][..],
        &["mcp", "list"],
        &["--version"],
        &["plugin", "update", "x"],
        // A flag the launcher cannot classify: run claude directly rather than risk mangling it.
        &["--brand-new-flag", "value"],
    ] {
        assert!(
            is_passthrough(&parse_claude_args(&s(p))),
            "{p:?} runs claude directly"
        );
    }
    // A prompt that happens to be a subcommand name only counts in first position.
    assert!(!is_passthrough(&parse_claude_args(&s(&[
        "--model",
        "opus",
        "doctor this"
    ]))));
}

#[test]
fn defaults_follow_the_context_window() {
    // Transcripts log `claude-opus-5-5` for 1M sessions; only Haiku is assumed to be 200k.
    assert_eq!(default_at_for("claude-opus-5-5", 10), 400_000);
    assert_eq!(default_at_for("claude-opus-5-5[1m]", 10), 400_000);
    assert_eq!(default_at_for("claude-haiku-4-5-20251001", 10), 140_000);
    assert_eq!(
        default_at_for("claude-haiku-4-5", 250_000),
        400_000,
        "past 200k it must be 1M"
    );
    assert_eq!(default_target(400_000), 120_000);
    assert_eq!(
        default_target(140_000),
        70_000,
        "far enough below the trigger"
    );
}

#[test]
fn cache_writes_merge_with_what_another_writer_saved() {
    let dir = tmp_dir();
    let p = dir.join("cache.json");
    fs::write(&p, json!({"a": "first writer"}).to_string()).unwrap();
    let mine: serde_json::Map<String, Value> = [("b".to_string(), json!("second writer"))]
        .into_iter()
        .collect();
    write_cache_merged(&p, &mine).unwrap();
    let got = read(p).unwrap();
    assert_eq!(got["a"], "first writer");
    assert_eq!(got["b"], "second writer");
}

#[test]
fn live_size_is_the_last_main_thread_usage_in_the_tail() {
    let dir = tmp_dir();
    let p = dir.join("t.jsonl");
    let mut side = assistant("s1", "a2", "subagent", Some(999_999));
    side["isSidechain"] = json!(true);
    write_lines(
        &p,
        &[
            user("u1", None, "hi"),
            assistant("a1", "u1", "one", Some(50_000)),
            assistant("a2", "a1", "two", Some(61_000)),
            side,
        ],
    );
    // A torn final line (claude mid-write) is skipped, not fatal.
    fs::OpenOptions::new()
        .append(true)
        .open(&p)
        .unwrap()
        .write_all(b"{\"type\":\"assist")
        .unwrap();
    assert_eq!(live_tokens(&p), Some(61_000));
}

// ------------------------------------------------------------------------------ hooks

#[test]
fn a_bare_recompact_is_taken_by_the_launcher_before_the_model_sees_it() {
    let dir = tmp_dir();
    let t = big_session(&dir, 50_000);
    let shell = shell_state(json!({"auto": true}), &t);
    let input =
        |p: &str| json!({"session_id": SESSION, "transcript_path": t, "cwd": "/tmp", "prompt": p});
    let out = on_prompt_in(Some(reload(&shell)), &input("  /recompact ")).expect("handled");
    assert_eq!(out["decision"], "block");
    let req = read(shell.dir.join("request.json")).unwrap();
    assert_eq!(req["ready"], true);
    assert_eq!(req["force"], true);
    assert_eq!(req["reason"], "manual");
    // With arguments it is a normal skill invocation.
    fs::remove_file(shell.dir.join("request.json")).unwrap();
    assert!(on_prompt_in(Some(reload(&shell)), &input("/recompact abc123")).is_none());
    assert!(on_prompt_in(Some(reload(&shell)), &input("please /recompact")).is_none());
    // Outside the launcher, the skill handles it.
    assert!(on_prompt_in(None, &input("/recompact")).is_none());
}

#[test]
fn a_turn_ending_over_the_threshold_requests_a_handoff() {
    let dir = tmp_dir();
    let t = big_session(&dir, 180_000);
    let shell = shell_state(json!({"auto": true, "at": 150_000}), &t);
    // First a warning, and nothing else: never a surprise.
    let warn = on_stop_in(Some(reload(&shell)), &stop_input(&t)).expect("warned");
    let text = warn["systemMessage"].as_str().unwrap();
    assert!(
        text.contains("180k") && text.contains("next turn") && text.contains("/recompact off"),
        "{text}"
    );
    assert!(read(shell.dir.join("request.json")).is_none());
    // The next turn's end hands off.
    let out = on_stop_in(Some(reload(&shell)), &stop_input(&t)).expect("handoff requested");
    assert!(out["systemMessage"]
        .as_str()
        .unwrap()
        .contains("compacting"));
    let req = read(shell.dir.join("request.json")).unwrap();
    assert_eq!(req["reason"], "auto");
    assert_eq!(req["ready"], true);
    assert_eq!(req["force"], false);
    assert_eq!(
        req["kick"], false,
        "the user was driving: no continue prompt"
    );

    // Under the threshold: nothing.
    let t2 = big_session(&tmp_dir(), 90_000);
    let quiet = shell_state(json!({"auto": true, "at": 150_000}), &t2);
    assert!(on_stop_in(Some(reload(&quiet)), &stop_input(&t2)).is_none());
    assert!(read(quiet.dir.join("request.json")).is_none());

    // Disabled: nothing.
    let off = shell_state(json!({"auto": false, "at": 150_000}), &t);
    assert!(on_stop_in(Some(reload(&off)), &stop_input(&t)).is_none());

    // A handoff that could not get below the threshold re-arms higher instead of looping.
    let rearmed = shell_state(json!({"auto": true, "at": 150_000, "rearm": 200_000}), &t);
    assert!(on_stop_in(Some(reload(&rearmed)), &stop_input(&t)).is_none());

    // Another session's stop (a claude started from inside this one) is not ours.
    let mut other = stop_input(&t);
    other["session_id"] = json!("ffffffff-0000-4000-8000-000000000000");
    let fresh = shell_state(json!({"auto": true, "at": 150_000}), &t);
    assert!(on_stop_in(Some(reload(&fresh)), &other).is_none());
}

#[test]
fn background_work_defers_the_handoff_and_says_so_once() {
    let dir = tmp_dir();
    let t = big_session(&dir, 180_000);
    let shell = shell_state(json!({"auto": true, "at": 150_000}), &t);
    let mut input = stop_input(&t);
    input["background_tasks"] = json!([{"id": "b1"}]);
    let out = on_stop_in(Some(reload(&shell)), &input).expect("explains the wait");
    assert!(out["systemMessage"]
        .as_str()
        .unwrap()
        .contains("background tasks"));
    assert!(read(shell.dir.join("request.json")).is_none());
    assert!(
        on_stop_in(Some(reload(&shell)), &input).is_none(),
        "said once"
    );
    // Once the tasks are done: the warning, then the handoff.
    on_stop_in(Some(reload(&shell)), &stop_input(&t)).expect("warned");
    assert!(read(shell.dir.join("request.json")).is_none());
    on_stop_in(Some(reload(&shell)), &stop_input(&t)).expect("handoff");
    assert!(read(shell.dir.join("request.json")).is_some());
}

#[test]
fn a_session_opened_over_the_size_gets_room_before_any_warning() {
    let dir = tmp_dir();
    let t = big_session(&dir, 612_000);
    let shell = shell_state(json!({"auto": true, "at": 400_000}), &t);
    let mut session: Value = read(shell.dir.join("session.json")).unwrap();
    session["start_tokens"] = json!(612_000);
    fs::write(shell.dir.join("session.json"), session.to_string()).unwrap();
    assert!(
        on_stop_in(Some(reload(&shell)), &stop_input(&t)).is_none(),
        "opened as it is"
    );
    // 100k past where it opened, the usual warning comes.
    let t2 = big_session(&dir, 715_000);
    let mut input = stop_input(&t2);
    input["transcript_path"] = json!(t2);
    let warn = on_stop_in(Some(reload(&shell)), &input).expect("warned");
    assert!(warn["systemMessage"].as_str().unwrap().contains("715k"));
}

#[test]
fn a_queued_agent_handoff_goes_at_the_end_of_the_turn() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let shell = shell_state(json!({"auto": true}), &t);
    fs::write(
        shell.dir.join("request.json"),
        json!({"session": SESSION, "reason": "agent", "kick": true, "force": true, "ready": false})
            .to_string(),
    )
    .unwrap();
    assert!(on_stop_in(Some(reload(&shell)), &stop_input(&t)).is_some());
    let req = read(shell.dir.join("request.json")).unwrap();
    assert_eq!(req["ready"], true);
    assert_eq!(req["kick"], true);
}

#[test]
fn a_long_turn_is_asked_to_checkpoint_once_then_handed_off_with_a_continue() {
    let dir = tmp_dir();
    let t = big_session(&dir, 200_000);
    let shell = shell_state(
        json!({"auto": true, "at": 150_000, "checkpoint_at": 180_000}),
        &t,
    );
    let input = json!({"session_id": SESSION, "transcript_path": t, "tool_name": "Bash"});
    // A subagent's tool call carries the parent's session id; it is not the one to stop.
    let mut sub = input.clone();
    sub["agent_id"] = json!("a1b2c3");
    assert!(on_post_tool_use_in(Some(reload(&shell)), &sub).is_none());
    let out = on_post_tool_use_in(Some(reload(&shell)), &input).expect("nudged");
    assert!(out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .contains("clean checkpoint"));
    assert!(
        on_post_tool_use_in(Some(reload(&shell)), &input).is_none(),
        "not on every call"
    );
    on_stop_in(Some(reload(&shell)), &stop_input(&t)).expect("handoff");
    let req = read(shell.dir.join("request.json")).unwrap();
    assert_eq!(
        req["kick"], true,
        "the agent was mid-task: the resumed session continues"
    );
}

// ------------------------------------------------------------------------------ launcher

#[test]
fn the_launcher_stops_claude_compacts_and_resumes_the_twin_with_the_same_flags() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    // First run: queue a ready request the way the hooks do, then wait to be stopped.
    let stub = write_stub(
        &dir,
        "claude-stub.sh",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "spawn $*" >> "$D/spawns.log"
n=$(wc -l < "$D/spawns.log")
if [ "$n" -eq 1 ]; then
  printf '{{"session":"{SESSION}","transcript":"{t}","cwd":"{d}","reason":"manual","force":true,"ready":true,"kick":false}}' > "$RECOMPACT_SHELL/request.json"
  trap 'exit 143' TERM
  sleep 30 & wait $!
  exit 0
fi
exit 0
"#,
            t = t.display(),
            d = dir.display()
        ),
    );
    let rc = cmd_shell(&s(&[
        "--interactive",
        "--mask",
        "--dir",
        dir.to_str().unwrap(),
        "--state-root",
        tmp_dir().to_str().unwrap(),
        "--claude-bin",
        &stub,
        "--model",
        "opus",
        "--dangerously-skip-permissions",
        "--",
        "-hello there",
    ]));
    assert_eq!(rc, 0);
    let twin = lineage_latest(&dir, SESSION);
    assert_ne!(twin, SESSION, "the session was compacted");
    let spawns = fs::read_to_string(dir.join("spawns.log")).unwrap();
    let lines: Vec<&str> = spawns.lines().collect();
    assert_eq!(lines.len(), 2, "{spawns}");
    assert!(lines[0].contains("-- -hello there") && lines[0].contains("--model opus"));
    assert!(lines[1].contains(&format!("--resume {twin}")), "{spawns}");
    assert!(
        lines[1].contains("--allow-dangerously-skip-permissions"),
        "{spawns}"
    );
    assert!(
        lines[1].contains("--model opus"),
        "the launch model is kept: {spawns}"
    );
    assert!(
        !lines[1].contains("hello there"),
        "the first prompt is not replayed: {spawns}"
    );
}

#[test]
fn a_resumed_session_opens_as_it_is_however_big() {
    let dir = tmp_dir();
    big_session(&dir, 900_000);
    let stub = write_stub(
        &dir,
        "claude-stub.sh",
        "#!/bin/sh\necho \"spawn $*\" >> \"$(dirname \"$0\")/spawns.log\"\nexit 0\n",
    );
    let rc = cmd_shell(&s(&[
        "--interactive",
        "--mask",
        "--at",
        "100000",
        "--dir",
        dir.to_str().unwrap(),
        "--state-root",
        tmp_dir().to_str().unwrap(),
        "--claude-bin",
        &stub,
        "-r",
        SESSION,
        "do the next thing",
    ]));
    assert_eq!(rc, 0);
    assert_eq!(lineage_latest(&dir, SESSION), SESSION, "nothing compacted");
    let spawns = fs::read_to_string(dir.join("spawns.log")).unwrap();
    assert!(
        spawns.contains(&format!("-r {SESSION} do the next thing")),
        "{spawns}"
    );
}

#[test]
fn an_ordinary_exit_ends_the_launcher_and_a_bare_sigterm_restarts_claude() {
    let dir = tmp_dir();
    let stub = write_stub(
        &dir,
        "claude-stub.sh",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "spawn $*" >> "$D/spawns.log"
n=$(wc -l < "$D/spawns.log")
[ "$n" -eq 1 ] && exit 143
exit 7
"#,
    );
    let rc = cmd_shell(&s(&[
        "--interactive",
        "--dir",
        dir.to_str().unwrap(),
        "--state-root",
        tmp_dir().to_str().unwrap(),
        "--claude-bin",
        &stub,
    ]));
    assert_eq!(rc, 7, "claude's own exit code comes back");
    let spawns = fs::read_to_string(dir.join("spawns.log")).unwrap();
    assert_eq!(spawns.lines().count(), 2, "{spawns}");
    // No session was ever reported, so there is nothing to resume by id, and `--continue`
    // could open another terminal's session: claude starts fresh with the same flags.
    let second = spawns.lines().nth(1).unwrap();
    assert!(
        !second.contains("--continue") && !second.contains("--resume"),
        "{spawns}"
    );
}

// ------------------------------------------------------------------------------ setup

#[test]
fn install_adds_one_block_replaces_it_on_rerun_and_uninstall_removes_it() {
    let dir = tmp_dir();
    let rc = dir.join(".zshrc");
    fs::write(
        &rc,
        "export PATH=\"$HOME/bin:$PATH\"\nalias comax='claude --model opus'\n",
    )
    .unwrap();
    let rc_arg = rc.to_str().unwrap().to_string();
    assert_eq!(cmd_install(&["--rc".into(), rc_arg.clone()]), 0);
    let once = fs::read_to_string(&rc).unwrap();
    assert!(once.contains("alias comax="), "existing lines kept");
    assert!(once.contains("claude() {") && once.contains(" shell \"$@\""));
    assert!(
        once.contains("command claude \"$@\""),
        "falls back to plain claude"
    );
    assert_eq!(cmd_install(&["--rc".into(), rc_arg.clone()]), 0);
    assert_eq!(fs::read_to_string(&rc).unwrap(), once, "idempotent");
    assert_eq!(cmd_uninstall(&["--rc".into(), rc_arg.clone()]), 0);
    let after = fs::read_to_string(&rc).unwrap();
    assert!(
        !after.contains("recompact") && after.contains("alias comax="),
        "{after}"
    );

    // A file that already defines claude is left alone.
    fs::write(&rc, "claude() { echo mine; }\n").unwrap();
    assert_eq!(cmd_install(&["--rc".into(), rc_arg]), 1);
    assert_eq!(
        fs::read_to_string(&rc).unwrap(),
        "claude() { echo mine; }\n"
    );
}

#[test]
fn the_binary_and_the_plugin_manifest_carry_the_same_version() {
    // The launcher script downloads the release named after plugin.json's version.
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/.claude-plugin/plugin.json"
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
}
