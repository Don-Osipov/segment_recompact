//! Handoff in place: claude runs on the launcher's own terminal, compaction happens while it
//! keeps running, and `/resume <twin>` typed at a pause switches it to the twin, so background
//! work is never stopped. A switch that does not take falls back to the restart.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use recompact::*;
use serde_json::{json, Value};

const SESSION: &str = "5e5516a0-0000-4000-8000-000000000019";

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

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    json!({"type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
           "timestamp": "2026-09-30T10:00:00.000Z", "userType": "external", "isSidechain": false,
           "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
}

fn assistant(uuid: &str, parent: &str, text: &str, prompt_tokens: Option<u64>) -> Value {
    let mut r = json!({"type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
           "timestamp": "2026-09-30T10:00:01.000Z", "userType": "external", "isSidechain": false,
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
                   "timestamp": "2026-09-30T10:00:01.000Z", "userType": "external", "isSidechain": false,
                   "message": {"id": "msg_a1", "role": "assistant", "model": "claude-opus-5-5",
                       "type": "message", "stop_reason": "tool_use",
                       "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/big"}}]}}),
            json!({"type": "user", "uuid": "r1", "parentUuid": "a1", "sessionId": SESSION,
                   "timestamp": "2026-09-30T10:00:02.000Z", "userType": "external", "isSidechain": false,
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

/// A launcher state dir tracking SESSION, as the SessionStart hook would leave it.
fn shell_state(config: Value, transcript: &Path) -> Shell {
    let dir = tmp_dir();
    let mut config = config;
    config["launcher"] = json!(std::process::id());
    fs::write(dir.join("config.json"), config.to_string()).unwrap();
    fs::write(
        dir.join("session.json"),
        json!({"session": SESSION, "transcript": transcript}).to_string(),
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

fn write_stub(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

// ------------------------------------------------------------------------------ the terminal

fn kinds(splitter: &mut InputSplitter, bytes: &[u8]) -> Vec<(InputKind, String)> {
    splitter
        .split(bytes)
        .into_iter()
        .map(|(k, b)| (k, String::from_utf8_lossy(&b).into_owned()))
        .collect()
}

#[test]
fn terminal_input_is_told_apart_keys_enter_replies_and_reports() {
    use InputKind::*;
    let mut sp = InputSplitter::default();
    assert_eq!(
        kinds(&mut sp, b"fix it\r"),
        vec![(Key, "fix it".into()), (Enter, "\r".into())]
    );
    // Arrows and Alt+Enter edit the box; only a plain Enter (legacy or kitty) sends it.
    assert_eq!(kinds(&mut sp, b"\x1b[A")[0].0, Key);
    assert_eq!(kinds(&mut sp, b"\x1b\r")[0].0, Key);
    assert_eq!(kinds(&mut sp, b"\x1b[13u")[0].0, Enter);
    assert_eq!(kinds(&mut sp, b"\x1b[13;129u")[0].0, Enter, "Num Lock on");
    assert_eq!(kinds(&mut sp, b"\x1b[57414u")[0].0, Enter, "keypad Enter");
    assert_eq!(
        kinds(&mut sp, b"\x1bOM")[0].0,
        Enter,
        "keypad Enter, application mode"
    );
    assert_eq!(kinds(&mut sp, b"\x1b[13;2u")[0].0, Key);
    assert_eq!(
        kinds(&mut sp, b"\x1b[97;1:3u")[0].0,
        Passive,
        "a key release"
    );
    assert_eq!(kinds(&mut sp, b"\x1b")[0].0, Key, "a lone Esc is a key");
    // The terminal answering claude's queries: never held, never text.
    for reply in [
        &b"\x1b[?62;22c"[..],
        b"\x1b[12;40R",
        b"\x1b[?1u",
        b"\x1b[?2026;2$y",
        b"\x1b]11;rgb:0000/0000/0000\x1b\\",
        b"\x1b]11;rgb:ffff/ffff/ffff\x07",
        b"\x1bP>|kitty(0.36)\x1b\\",
    ] {
        assert_eq!(
            kinds(&mut sp, reply),
            vec![(Reply, String::from_utf8_lossy(reply).into_owned())],
            "{reply:?}"
        );
    }
    assert_eq!(kinds(&mut sp, b"\x1b[I")[0].0, Passive);
    assert_eq!(kinds(&mut sp, b"\x1b[<64;10;5M")[0].0, Passive);
    // Mixed in one read, in order.
    assert_eq!(
        kinds(&mut sp, b"a\x1b[?62c\rb"),
        vec![
            (Key, "a".into()),
            (Reply, "\x1b[?62c".into()),
            (Enter, "\r".into()),
            (Key, "b".into())
        ]
    );
}

#[test]
fn a_bracketed_paste_is_one_paste_even_with_newlines_and_across_reads() {
    use InputKind::*;
    let mut sp = InputSplitter::default();
    let first = kinds(&mut sp, b"\x1b[200~line one\rline");
    assert!(first.iter().all(|(k, _)| *k == Paste), "{first:?}");
    let second = kinds(&mut sp, b" two\x1b[201~\r");
    assert_eq!(
        second,
        vec![(Paste, " two\x1b[201~".into()), (Enter, "\r".into())]
    );
}

// ------------------------------------------------------------------------------ hooks

#[test]
fn in_place_the_stop_hook_marks_idle_and_does_not_wait_for_background_jobs() {
    let dir = tmp_dir();
    let t = big_session(&dir, 200_000);
    let shell = shell_state(json!({"auto": true, "at": 150_000, "inplace": true}), &t);
    let mut input = json!({"session_id": SESSION, "transcript_path": t, "cwd": "/tmp",
        "hook_event_name": "Stop", "stop_hook_active": false, "session_crons": [],
        "background_tasks": [{"id": "b1", "type": "shell", "status": "running", "command": "cargo build --release"}]});
    let out = on_stop_in(Some(reload(&shell)), &input).expect("handoff");
    let msg = out["systemMessage"].as_str().unwrap();
    assert!(msg.contains("compacting in the background"), "{msg}");
    let req = read(shell.dir.join("request.json")).unwrap();
    assert_eq!(
        req["ready"], true,
        "no waiting for the build: nothing is stopped"
    );

    // While that handoff is under way, later turns neither ask again nor repeat the message.
    fs::remove_file(shell.dir.join("request.json")).unwrap();
    fs::write(
        shell.dir.join("handoff.json"),
        json!({"session": SESSION}).to_string(),
    )
    .unwrap();
    input["background_tasks"] = json!([]);
    assert!(on_stop_in(Some(reload(&shell)), &input).is_none());
    assert!(!shell.dir.join("request.json").exists());
}

#[test]
fn in_place_a_manual_recompact_says_the_session_switches_at_the_next_pause() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let shell = shell_state(json!({"inplace": true}), &t);
    let input = json!({"session_id": SESSION, "transcript_path": t, "prompt": "/recompact"});
    let out = on_prompt_in(Some(reload(&shell)), &input).expect("handled");
    assert_eq!(out["decision"], "block");
    assert!(out["reason"]
        .as_str()
        .unwrap()
        .contains("switches to the compacted copy"));
    assert_eq!(read(shell.dir.join("request.json")).unwrap()["ready"], true);

    fs::write(
        shell.dir.join("handoff.json"),
        json!({"session": SESSION}).to_string(),
    )
    .unwrap();
    let again = on_prompt_in(Some(reload(&shell)), &input).expect("handled");
    assert!(again["reason"]
        .as_str()
        .unwrap()
        .contains("already compacting"));
}

#[test]
fn a_typed_resume_that_landed_in_a_draft_never_reaches_the_model() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let shell = shell_state(json!({"inplace": true}), &t);
    let twin = "7a1e0000-0000-4000-8000-00000000aaaa";
    fs::write(
        shell.dir.join("switch.json"),
        json!({"twin": twin}).to_string(),
    )
    .unwrap();
    let prompt = |p: &str| json!({"session_id": SESSION, "transcript_path": t, "prompt": p});

    let out = on_prompt_in(
        Some(reload(&shell)),
        &prompt(&format!("also check the logs/resume {twin}")),
    )
    .expect("blocked");
    assert_eq!(out["decision"], "block");
    let reason = out["reason"].as_str().unwrap();
    assert!(
        reason.contains("also check the logs"),
        "the draft is shown back: {reason}"
    );
    // Esc read as Alt can eat the slash; the rest still gives it away.
    assert!(on_prompt_in(Some(reload(&shell)), &prompt(&format!("resume {twin}"))).is_some());
    assert!(on_prompt_in(Some(reload(&shell)), &prompt(&format!("/resume {twin}"))).is_none());
    assert!(on_prompt_in(Some(reload(&shell)), &prompt("an ordinary prompt")).is_none());
}

#[test]
fn the_twin_hears_once_that_its_background_work_kept_running() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let shell = shell_state(json!({"inplace": true}), &t);
    let twin = "7a1e0000-0000-4000-8000-00000000bbbb";
    fs::write(
        shell.dir.join("switch.json"),
        json!({"twin": twin, "message": "recompact: compacted in place", "context": "still running"})
            .to_string(),
    )
    .unwrap();
    let start = |id: &str, source: &str| json!({"session_id": id, "source": source});
    assert!(in_place_notice_in(Some(reload(&shell)), &start(twin, "startup")).is_none());
    assert!(in_place_notice_in(Some(reload(&shell)), &start(SESSION, "resume")).is_none());
    let (msg, ctx) = in_place_notice_in(Some(reload(&shell)), &start(twin, "resume")).unwrap();
    assert_eq!(msg, "recompact: compacted in place");
    assert_eq!(ctx.as_deref(), Some("still running"));
    assert!(
        in_place_notice_in(Some(reload(&shell)), &start(twin, "resume")).is_none(),
        "once"
    );
}

#[test]
fn between_turns_is_read_from_the_transcript_not_from_a_hook() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    assert!(turn_ended(&t), "turn bookkeeping follows the last reply");
    // A task notice starts a turn; its reply and bookkeeping end it. Subagent records and queue
    // bookkeeping change nothing.
    append(
        &t,
        &[
            json!({"type": "queue-operation", "operation": "dequeue", "sessionId": SESSION}),
            user(
                "u9",
                Some("a3"),
                "<task-notification>done</task-notification>",
            ),
        ],
    );
    assert!(!turn_ended(&t));
    append(
        &t,
        &[
            json!({"type": "assistant", "uuid": "s1", "isSidechain": true, "sessionId": SESSION,
                   "message": {"role": "assistant", "content": []}}),
            assistant("a9", "u9", "noted", None),
        ],
    );
    assert!(!turn_ended(&t), "the Stop hooks have not run yet");
    append(
        &t,
        &[json!({"type": "system", "subtype": "turn_duration", "sessionId": SESSION})],
    );
    assert!(turn_ended(&t));
}

#[test]
fn a_twin_is_stale_once_the_session_adds_a_turn_after_its_last_carried_record() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let twin = dir.join("twin.jsonl");
    append(
        &twin,
        &[
            json!({"type": "user", "uuid": "p0", "recompactPreamble": true, "message": {"role": "user", "content": "brief"}}),
            user("u2", Some("a2"), "thanks"),
            assistant("a3", "u2", "done", None),
            json!({"type": "user", "uuid": "z9", "recompactSynthetic": true, "message": {"role": "user", "content": "summary"}}),
        ],
    );
    assert_eq!(last_carried_uuid(&twin).as_deref(), Some("a3"));
    assert!(!activity_after(&t, "a3"), "only bookkeeping after it");
    append(
        &t,
        &[json!({"type": "queue-operation", "operation": "remove", "sessionId": SESSION})],
    );
    assert!(!activity_after(&t, "a3"));
    append(&t, &[user("u9", Some("a3"), "one more thing")]);
    assert!(activity_after(&t, "a3"));
    assert!(activity_after(&t, "not-there"), "unknown counts as stale");
    // Any JSON spacing: the record is found by its uuid, not by its text.
    let spaced = dir.join("spaced.jsonl");
    fs::write(
        &spaced,
        "{\"type\": \"user\", \"uuid\": \"s1\", \"message\": {\"role\": \"user\", \"content\": \"hi\"}}\n\
{\"type\": \"system\", \"subtype\": \"turn_duration\"}\n",
    )
    .unwrap();
    assert!(!activity_after(&spaced, "s1"));
}

// ------------------------------------------------------------------------------ the launcher

/// What the stub writes the way claude's hooks would: the live session and a ready
/// `/recompact` request while background work runs.
fn hooks_say(t: &Path) -> String {
    format!(
        r#"S="$RECOMPACT_SHELL"
printf '{{"session":"{SESSION}","transcript":"{t}"}}' > "$S/session.json"
printf '{{"session":"{SESSION}","transcript":"{t}","reason":"manual","force":true,"ready":true,"kick":false,"carry":{{"tasks":[{{"command":"sleep 600","description":"long job"}}],"crons":[]}}}}' > "$S/request.json"
"#,
        t = t.display()
    )
}

/// Claude, as far as the launcher can tell: it runs `meanwhile`, then reads what is typed into
/// its terminal and, given `/resume <id>`, opens that session (its SessionStart hook writes
/// session.json).
fn switching_stub(dir: &Path, t: &Path, meanwhile: &str) -> String {
    write_stub(
        dir,
        "claude-stub.sh",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "spawn $*" >> "$D/spawns.log"
[ "$(wc -l < "$D/spawns.log")" -eq 1 ] || exit 0
[ -t 0 ] && echo tty >> "$D/typed.log"
{hooks}
{meanwhile}
while IFS= read -r line; do
  echo "got $line" >> "$D/typed.log"
  case "$line" in
    *"/resume "*)
      printf '{{"session":"%s"}}' "${{line##*/resume }}" > "$S/session.json"
      sleep 1
      exit 0;;
  esac
done
exit 0
"#,
            hooks = hooks_say(t)
        ),
    )
}

fn run_in_place(dir: &Path, stub: &str) -> String {
    let rc = cmd_shell(&s(&[
        "--interactive",
        "--pty",
        "--leader",
        env!("CARGO_BIN_EXE_recompact"),
        "--mask",
        "--dir",
        dir.to_str().unwrap(),
        "--state-root",
        tmp_dir().to_str().unwrap(),
        "--claude-bin",
        stub,
    ]));
    assert_eq!(rc, 0);
    let twin = lineage_latest(dir, SESSION);
    assert_ne!(twin, SESSION, "the session was compacted");
    assert!(
        dir.join(format!("{twin}.jsonl")).exists(),
        "and the twin kept"
    );
    let spawns = fs::read_to_string(dir.join("spawns.log")).unwrap();
    assert_eq!(
        spawns.lines().count(),
        1,
        "claude was never restarted: {spawns}"
    );
    let typed = fs::read_to_string(dir.join("typed.log")).unwrap();
    assert!(
        typed.starts_with("tty\n"),
        "claude ran on a terminal: {typed}"
    );
    assert!(typed.contains(&format!("/resume {twin}")), "{typed}");
    twin
}

#[test]
fn the_launcher_switches_the_running_claude_to_the_twin_without_a_restart() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let stub = switching_stub(&dir, &t, "");
    run_in_place(&dir, &stub);
}

#[test]
fn a_turn_that_lands_while_compacting_is_in_the_twin_it_switches_to() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let late = [
        user("u9", Some("a3"), "LATE-TURN while compacting"),
        assistant("a9", "u9", "late reply", None),
        json!({"type": "system", "subtype": "turn_duration", "sessionId": SESSION}),
    ]
    .iter()
    .map(|r| format!("printf '%s\\n' '{r}' >> \"{}\"", t.display()))
    .collect::<Vec<_>>()
    .join("\n");
    let stub = switching_stub(&dir, &t, &format!("sleep 0.3\n{late}"));
    let twin = run_in_place(&dir, &stub);
    let text = fs::read_to_string(dir.join(format!("{twin}.jsonl"))).unwrap();
    assert!(
        text.contains("LATE-TURN while compacting"),
        "the twin is current"
    );
}

#[test]
fn a_switch_that_does_not_take_restarts_claude_on_the_twin() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    // This claude reads what is typed but never opens the twin, until it is stopped.
    let stub = write_stub(
        &dir,
        "claude-stub.sh",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "spawn $*" >> "$D/spawns.log"
[ "$(wc -l < "$D/spawns.log")" -eq 1 ] || exit 0
{hooks}
exec 3<&0
( while IFS= read -r line <&3; do echo "got $line" >> "$D/typed.log"; done ) &
trap 'exit 143' TERM
sleep 60 & wait $!
"#,
            hooks = hooks_say(&t)
        ),
    );
    let rc = cmd_shell(&s(&[
        "--interactive",
        "--pty",
        "--leader",
        env!("CARGO_BIN_EXE_recompact"),
        "--switch-timeout",
        "1",
        "--mask",
        "--dir",
        dir.to_str().unwrap(),
        "--state-root",
        tmp_dir().to_str().unwrap(),
        "--claude-bin",
        &stub,
    ]));
    assert_eq!(rc, 0);
    let twin = lineage_latest(&dir, SESSION);
    assert_ne!(twin, SESSION);
    let typed = fs::read_to_string(dir.join("typed.log")).unwrap();
    assert_eq!(
        typed.matches(&format!("/resume {twin}")).count(),
        2,
        "typed twice: {typed}"
    );
    let spawns = fs::read_to_string(dir.join("spawns.log")).unwrap();
    let lines: Vec<&str> = spawns.lines().collect();
    assert_eq!(lines.len(), 2, "{spawns}");
    assert!(lines[1].contains(&format!("--resume {twin}")), "{spawns}");
    assert!(
        lines[1].contains("Start again the ones that are still needed")
            && lines[1].contains("sleep 600"),
        "the restart stopped the background work, so it is restored: {spawns}"
    );
}

#[test]
fn a_live_session_ends_at_its_newest_turn_not_at_a_stale_last_prompt() {
    let ids = |records: Vec<Value>| -> Vec<String> {
        select_active(records)
            .0
            .iter()
            .filter_map(|r| r.get("uuid").and_then(|v| v.as_str()).map(String::from))
            .collect()
    };
    let lp = |leaf: &str| json!({"type": "last-prompt", "leafUuid": leaf, "sessionId": SESSION});
    let base = vec![
        user("u1", None, "first"),
        assistant("a1", "u1", "one", None),
    ];
    // Written when "second" was submitted, pointing at the tip before it; the turn went on.
    let mut live = base.clone();
    live.extend([
        lp("a1"),
        user("u2", Some("a1"), "second"),
        assistant("a2", "u2", "two", None),
    ]);
    assert_eq!(ids(live), ["u1", "a1", "u2", "a2"]);
    // Rewound to a1 with nothing sent since: the later record is the one that counts.
    let mut rewound = base.clone();
    rewound.extend([
        user("u2", Some("a1"), "second"),
        assistant("a2", "u2", "two", None),
        lp("a1"),
    ]);
    assert_eq!(ids(rewound), ["u1", "a1"]);
}

#[test]
fn the_input_box_is_followed_from_what_each_key_does_to_it() {
    use InputKind::*;
    let feed = |keys: &[(InputKind, &[u8])]| {
        let mut b = InputBox::default();
        for (k, bytes) in keys {
            b.feed(*k, bytes);
        }
        b
    };
    let text = |s: &str| InputBox::Text(s.as_bytes().to_vec());
    assert_eq!(
        feed(&[(Key, b"/recompcat"), (Key, b"\x7f\x7f\x7f"), (Key, b"act")]),
        text("/recompact")
    );
    assert!(feed(&[(Key, b"/recompact")]).is("/recompact"));
    assert_eq!(
        feed(&[(Key, b"hi"), (Enter, b"\r")]),
        InputBox::Empty,
        "Enter sends it"
    );
    assert_eq!(
        feed(&[(Key, b"x"), (Key, b"\x7f")]),
        InputBox::Empty,
        "typed and deleted"
    );
    assert_eq!(
        feed(&[(Key, b"draft"), (Key, b"\x03")]),
        InputBox::Empty,
        "Ctrl+C clears it"
    );
    assert_eq!(
        feed(&[(Key, b"draft"), (Key, b"\x1b[99;5u")]),
        InputBox::Empty,
        "Ctrl+C, kitty protocol"
    );
    // Keys that cannot put text in an empty box leave it empty.
    for key in [
        &b"\x1b"[..],   // Esc
        b"\x1b[27u",    // Esc, kitty protocol
        b"\x1b[D",      // Left
        b"\x1b[1;5C",   // Ctrl+Right
        b"\x1b[Z",      // Shift+Tab
        b"\x1b[9;2u",   // Shift+Tab, kitty protocol
        b"\x1b[3~",     // Delete
        b"\x0f",        // Ctrl+O
        b"\x1b[111;5u", // Ctrl+O, kitty protocol
        b"\x1b[101;3u", // Alt+E
        b"\x1bOP",      // F1
    ] {
        assert_eq!(feed(&[(Key, key)]), InputBox::Empty, "{key:?}");
    }
    // History, completion and the clipboard can fill it with text this does not see.
    for key in [
        &b"\x1b[A"[..],
        b"\t",
        b"\x1b[9u",
        b"\x12",
        b"\x16",
        b"\x1b[118;5u",
    ] {
        assert_eq!(feed(&[(Key, key)]), InputBox::Unknown, "{key:?}");
    }
    assert_eq!(feed(&[(Key, b"\x1b[A"), (Enter, b"\r")]), InputBox::Empty);
    assert_eq!(feed(&[(Paste, b"\x1b[200~x\x1b[201~")]), InputBox::Draft);
    assert_eq!(
        feed(&[(Key, b"ab"), (Key, b"\x1b[D")]),
        InputBox::Draft,
        "cursor moved"
    );
    assert_eq!(feed(&[(Key, "é".as_bytes())]), InputBox::Draft);
    assert_eq!(
        feed(&[(Key, b"line"), (Key, b"\x1b[13;2u")]),
        InputBox::Draft,
        "Shift+Enter"
    );
    assert_eq!(
        feed(&[(Paste, b"x"), (Key, b"\x7f")]),
        InputBox::Unknown,
        "maybe emptied"
    );
    assert_eq!(
        feed(&[(Key, b"\x1b[A"), (Key, b"\x1b[D")]),
        InputBox::Unknown
    );
    assert_eq!(
        feed(&[(Reply, b"\x1b[?62c"), (Passive, b"\x1b[I")]),
        InputBox::Empty
    );
}

// ------------------------------------------------------------------------------ busy or idle

#[test]
fn claudes_title_is_read_even_when_split_across_reads() {
    let mut w = TitleWatch::default();
    assert_eq!(
        w.feed(b"text \x1b]0;\xe2\x9c\xb3 Claude Code\x07 more"),
        ["✳ Claude Code"]
    );
    assert!(
        w.feed(b"\x1b]0;\xe2\x97\x90 Fix the").is_empty(),
        "unfinished"
    );
    assert_eq!(w.feed(b" bug\x1b\\"), ["◐ Fix the bug"]);
    assert!(w.feed(b"\x1b").is_empty());
    assert_eq!(
        w.feed(b"]2;Ready | Claude 0a08\x07"),
        ["Ready | Claude 0a08"]
    );
    // Other OSC sequences, and a title cancelled by another escape, are not titles.
    assert!(w.feed(b"\x1b]11;?\x07\x1b]8;;https://x\x1b\\").is_empty());
    assert!(w.feed(b"\x1b]0;half\x1b[0m").is_empty());

    assert_eq!(title_state("✳ Claude Code"), Some(TitleState::Idle));
    assert_eq!(title_state("◐ Fix the bug"), Some(TitleState::Busy));
    assert_eq!(title_state("◑ Fix the bug"), Some(TitleState::Busy));
    assert_eq!(title_state("⠂ Older spinner"), Some(TitleState::Busy));
    assert_eq!(
        title_state("Needs input | Claude cda6"),
        None,
        "a hook's title says nothing"
    );
    assert_eq!(title_state(""), None);
}

#[test]
fn a_tool_call_without_a_result_means_claude_is_running_it_or_asking() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    assert!(!tool_call_waiting(&t), "between turns");
    let call = |id: &str| {
        json!({"type": "assistant", "uuid": format!("c{id}"), "sessionId": SESSION, "isSidechain": false,
               "message": {"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "Bash", "input": {}}]}})
    };
    let result = |id: &str| {
        json!({"type": "user", "uuid": format!("r{id}"), "sessionId": SESSION, "isSidechain": false,
               "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "ok"}]}})
    };
    append(
        &t,
        &[
            user("u9", Some("a3"), "touch a file"),
            call("t1"),
            call("t2"),
            result("t1"),
        ],
    );
    assert!(
        tool_call_waiting(&t),
        "t2 waits: a permission dialog for it"
    );
    append(&t, &[result("t2")]);
    assert!(!tool_call_waiting(&t));
    append(&t, &[call("t3")]);
    assert!(tool_call_waiting(&t));
    append(
        &t,
        &[json!({"type": "system", "subtype": "turn_duration", "sessionId": SESSION})],
    );
    assert!(!tool_call_waiting(&t), "the turn ended");
    // A prompt whose query a hook dropped leaves no reply and no turn bookkeeping: open in the
    // transcript, but nothing waits.
    append(
        &t,
        &[user(
            "u10",
            Some("a3"),
            "<task-notification>4 tasks</task-notification>",
        )],
    );
    assert!(!turn_ended(&t));
    assert!(!tool_call_waiting(&t));
}

/// A claude that sets its title, writes `then` to its transcript, and reports what it is typed.
/// `busy_for` seconds of spinner title first, then the idle title.
fn titled_stub(dir: &Path, t: &Path, then: &[Value], busy_for: u32, meanwhile: &str) -> String {
    let lines = then
        .iter()
        .map(|r| format!("printf '%s\\n' '{r}' >> \"{}\"", t.display()))
        .collect::<Vec<_>>()
        .join("\n");
    write_stub(
        dir,
        "claude-stub.sh",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "spawn $*" >> "$D/spawns.log"
[ "$(wc -l < "$D/spawns.log")" -eq 1 ] || exit 0
{hooks}
{lines}
i=0
while [ $i -lt {busy_for} ]; do printf '\033]0;\342\227\220 Working\007'; sleep 1; i=$((i+1)); done
echo idle > "$D/idle-at"
printf '\033]0;\342\234\263 Done\007'
{meanwhile}
while IFS= read -r line; do
  [ -f "$D/idle-at" ] || echo early >> "$D/typed.log"
  echo "got $line" >> "$D/typed.log"
  case "$line" in
    *"/resume "*)
      printf '{{"session":"%s"}}' "${{line##*/resume }}" > "$S/session.json"
      sleep 1
      exit 0;;
  esac
done
exit 0
"#,
            hooks = hooks_say(t)
        ),
    )
}

fn in_place_args(dir: &Path, stub: &str) -> Vec<String> {
    s(&[
        "--interactive",
        "--pty",
        "--leader",
        env!("CARGO_BIN_EXE_recompact"),
        "--switch-timeout",
        "3",
        "--mask",
        "--dir",
        dir.to_str().unwrap(),
        "--state-root",
        tmp_dir().to_str().unwrap(),
        "--claude-bin",
        stub,
    ])
}

#[test]
fn a_prompt_whose_query_was_dropped_does_not_hold_the_switch_forever() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    // The stuck case: a task notice written, then its batch dropped by a hook; claude is idle.
    let dropped = [
        user(
            "u9",
            Some("a3"),
            "<task-notification>4 tasks did not finish</task-notification>",
        ),
        json!({"type": "system", "subtype": "informational", "sessionId": SESSION,
               "content": "UserPromptSubmit operation blocked by hook"}),
    ];
    let stub = titled_stub(&dir, &t, &dropped, 0, "");
    assert_eq!(cmd_shell(&in_place_args(&dir, &stub)), 0);
    let twin = lineage_latest(&dir, SESSION);
    assert_ne!(twin, SESSION);
    let typed = fs::read_to_string(dir.join("typed.log")).unwrap_or_default();
    assert!(
        typed.contains(&format!("/resume {twin}")),
        "switched: {typed}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("spawns.log"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn a_title_that_spins_only_for_background_agents_does_not_hold_the_switch() {
    // Claude Code keeps the spinner while a background agent runs, after the turn has ended.
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let stub = write_stub(
        &dir,
        "claude-stub.sh",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo spawn >> "$D/spawns.log"
[ "$(wc -l < "$D/spawns.log")" -eq 1 ] || exit 0
{hooks}
(sleep 40; kill $$) &
printf '\033]0;\342\227\220 Background agent\007'
while IFS= read -r line; do
  echo "got $line" >> "$D/typed.log"
  case "$line" in
    *"/resume "*)
      printf '{{"session":"%s"}}' "${{line##*/resume }}" > "$S/session.json"
      sleep 1
      exit 0;;
  esac
done
exit 0
"#,
            hooks = hooks_say(&t)
        ),
    );
    assert_eq!(cmd_shell(&in_place_args(&dir, &stub)), 0);
    let typed = fs::read_to_string(dir.join("typed.log")).unwrap_or_default();
    assert!(typed.contains("/resume "), "switched: {typed}");
}

#[test]
fn nothing_is_typed_into_claude_while_its_title_says_it_is_working() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    let stub = titled_stub(&dir, &t, &[], 4, "");
    assert_eq!(cmd_shell(&in_place_args(&dir, &stub)), 0);
    let typed = fs::read_to_string(dir.join("typed.log")).unwrap_or_default();
    assert!(!typed.contains("early"), "typed while busy: {typed}");
    assert!(typed.contains("/resume "), "{typed}");
}

#[test]
fn a_twin_the_user_resumes_by_hand_is_the_switch_and_is_kept() {
    let dir = tmp_dir();
    let t = big_session(&dir, 20_000);
    // Busy for long enough that the launcher never types; meanwhile the user opens the twin.
    let meanwhile = format!(
        r#"for n in $(seq 1 100); do
  tw=$(ls "{d}" | grep '\.jsonl$' | grep -v '{SESSION}' | head -1)
  [ -n "$tw" ] && break; sleep 0.1
done
sleep 1
printf '{{"session":"%s"}}' "${{tw%.jsonl}}" > "$S/session.json"
echo "${{tw%.jsonl}}" > "$D/manual"
sleep 3
exit 0"#,
        d = dir.display()
    );
    let stub = write_stub(
        &dir,
        "claude-stub.sh",
        &format!(
            "#!/bin/sh\nD=\"$(dirname \"$0\")\"\necho spawn >> \"$D/spawns.log\"\n\
[ \"$(wc -l < \"$D/spawns.log\")\" -eq 1 ] || exit 0\n{}\nprintf '\\033]0;\\342\\227\\220 Working\\007'\n{meanwhile}\n",
            hooks_say(&t)
        ),
    );
    assert_eq!(cmd_shell(&in_place_args(&dir, &stub)), 0);
    let manual = fs::read_to_string(dir.join("manual")).unwrap();
    let twin = manual.trim();
    assert!(!twin.is_empty());
    assert!(
        dir.join(format!("{twin}.jsonl")).exists(),
        "the twin the user opened is not deleted"
    );
    assert_eq!(
        fs::read_to_string(dir.join("spawns.log"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn a_hook_whose_transcript_path_does_not_exist_finds_the_session_by_id() {
    // `claude --worktree x --resume <id>` reports the worktree's project folder, while the session
    // is still written in the folder it started in.
    let root = tmp_dir();
    let proj = root.join("main-project");
    fs::create_dir_all(&proj).unwrap();
    let t = big_session(&proj, 200_000);
    let moved = root
        .join("worktree-project")
        .join(format!("{SESSION}.jsonl"));
    let shell = shell_state(json!({"auto": true, "at": 150_000, "inplace": true}), &t);
    let input = json!({"session_id": SESSION, "transcript_path": moved, "cwd": "/tmp",
        "hook_event_name": "Stop", "stop_hook_active": false, "session_crons": [], "background_tasks": []});
    let out = on_stop_in(Some(reload(&shell)), &input).expect("auto-compaction sees the session");
    assert!(out["systemMessage"].as_str().unwrap().contains("200k"));
    assert_eq!(
        read(shell.dir.join("request.json")).unwrap()["transcript"],
        json!(t)
    );
}
