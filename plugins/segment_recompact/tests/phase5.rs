//! Shell + headless-summarizer tests, driven by stub claude binaries so CI never needs a real
//! CLI or network. Stubs are passed via --claude-bin (never env vars: cargo runs tests in
//! threads, and process-global env mutation races).

use recompact::*;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

const SESSION: &str = "sess-orig";

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:00.000Z", "cwd": "/tmp/p", "version": "2.0.0",
        "userType": "external", "isSidechain": false,
        "message": {"role": "user", "content": text}
    })
}

fn assistant(uuid: &str, parent: &str, text: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:01.000Z", "userType": "external", "isSidechain": false,
        "message": {
            "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-4-7",
            "type": "message", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}]
        }
    })
}

fn goal_status(uuid: &str, parent: &str, met: bool) -> Value {
    json!({
        "type": "attachment", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:02.000Z", "userType": "external", "isSidechain": false,
        "attachment": {"type": "goal_status", "met": met, "condition": "count to 30"}
    })
}

fn last_prompt(leaf: &str, text: &str) -> Value {
    json!({"type": "last-prompt", "leafUuid": leaf, "sessionId": SESSION, "lastPrompt": text})
}

fn write_session(dir: &PathBuf, records: &[Value]) -> PathBuf {
    let path = dir.join(format!("{SESSION}.jsonl"));
    let body: String = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
    fs::write(&path, body).unwrap();
    path
}

/// Prose-heavy session: two big collapsible assistant turns (mask cannot reduce prose), small tail.
fn prose_session() -> Vec<Value> {
    vec![
        user("u1", None, "write chapter one"),
        assistant("a1", "u1", &format!("CHAPTER-ONE {}", "lorem ".repeat(8000))),
        user("u2", Some("a1"), "write chapter two"),
        assistant("a2", "u2", &format!("CHAPTER-TWO {}", "ipsum ".repeat(8000))),
        user("u3", Some("a2"), "thanks"),
        assistant("a3", "u3", "done"),
        last_prompt("a3", "thanks"),
    ]
}

fn write_stub(dir: &PathBuf, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

/// Stub summarizer: reads the prompt on stdin, emits a JSON object with one canned summary per
/// "### UNIT <key>" marker, and logs the model it was invoked with.
fn summarizer_stub(dir: &PathBuf) -> String {
    write_stub(
        dir,
        "stub-claude.sh",
        r#"#!/bin/sh
# argv: -p --model <M> --strict-mcp-config
echo "$3" >> "$(dirname "$0")/models.log"
keys=$(sed -n 's/^### UNIT //p')
printf '{'
first=1
for k in $keys; do
  [ $first -eq 1 ] || printf ','
  printf '"%s":"Stub summary for unit %s."' "$k" "$k"
  first=0
done
printf '}\n'
"#,
    )
}

// ---------------------------------------------------------------------------------------- tests

#[test]
fn active_goal_detection_reads_latest_status() {
    let none = vec![user("u1", None, "hi"), assistant("a1", "u1", "hello")];
    assert!(!has_active_goal(&none));
    let active = vec![
        user("u1", None, "hi"),
        goal_status("g1", "u1", false),
    ];
    assert!(has_active_goal(&active));
    // The LATEST status wins: an achieved goal is not active.
    let done = vec![
        user("u1", None, "hi"),
        goal_status("g1", "u1", false),
        goal_status("g2", "g1", true),
    ];
    assert!(!has_active_goal(&done));
}

#[test]
fn continue_summarize_with_stub_compacts_prose() {
    let dir = tmp_dir();
    let src = write_session(&dir, &prose_session());
    let stub = summarizer_stub(&dir);
    let before = approx_tokens(&load_jsonl(&src));

    let rc = cmd_continue(&[
        src.to_string_lossy().into_owned(),
        "--threshold".into(),
        "2000".into(),
        "--summarize-with".into(),
        "stub-model".into(),
        "--claude-bin".into(),
        stub,
    ]);
    assert_eq!(rc, 0);

    let latest = lineage_latest(&dir, SESSION);
    assert_ne!(latest, SESSION, "a compacted descendant must exist");
    let out = dir.join(format!("{latest}.jsonl"));
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.contains("Stub summary for unit"), "stub summaries assembled");
    assert!(!text.contains("CHAPTER-ONE"), "big prose replaced");
    let after = approx_tokens(&load_jsonl(&out));
    assert!(after * 3 < before, "major reduction expected: {before} -> {after}");
    // Cache was populated by content hash, so a re-run needs no summarizer at all.
    assert!(dir.join(".recompact-summary-cache.json").exists());
    assert_eq!(
        cmd_verify(&[
            out.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned(),
        ]),
        0
    );
}

#[test]
fn escalation_routes_high_salience_units_to_second_model() {
    let dir = tmp_dir();
    let src = write_session(&dir, &prose_session());
    let stub = summarizer_stub(&dir);
    let rc = cmd_continue(&[
        src.to_string_lossy().into_owned(),
        "--threshold".into(),
        "2000".into(),
        "--summarize-with".into(),
        "base-model".into(),
        "--escalate-with".into(),
        "strong-model".into(),
        "--escalate-above".into(),
        "0.0".into(),
        "--claude-bin".into(),
        stub,
    ]);
    assert_eq!(rc, 0);
    let log = fs::read_to_string(dir.join("models.log")).unwrap();
    // escalate-above 0.0 routes every unit to the escalation model.
    assert!(log.contains("strong-model"), "escalation model used: {log}");
    assert!(!log.contains("base-model"), "no unit left on the base model: {log}");
}

#[test]
fn shell_auto_cycle_with_stub_compacts_and_reports_id() {
    let dir = tmp_dir();
    // Tool-heavy compressible session so mask-mode continue actually compacts.
    let big = "x".repeat(24000);
    let records = vec![
        user("u1", None, "inspect"),
        json!({
            "type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": SESSION,
            "timestamp": "2026-07-17T00:00:01.000Z", "userType": "external", "isSidechain": false,
            "message": {"id": "msg_a1", "role": "assistant", "model": "claude-opus-4-7",
                "type": "message", "stop_reason": "tool_use",
                "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/big"}}]}
        }),
        json!({
            "type": "user", "uuid": "r1", "parentUuid": "a1", "sessionId": SESSION,
            "timestamp": "2026-07-17T00:00:02.000Z", "userType": "external", "isSidechain": false,
            "sourceToolAssistantUUID": "a1",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": big}]}
        }),
        assistant("a2", "r1", "saw it"),
        user("u2", Some("a2"), "ok"),
        assistant("a3", "u2", "done"),
        last_prompt("a3", "ok"),
    ];
    write_session(&dir, &records);
    // Stub interactive claude: records its argv, exits 0 (no new session file: head stays).
    let stub = write_stub(
        &dir,
        "stub-interactive.sh",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/args.log\"\nexit 0\n",
    );
    let rc = cmd_shell(&[
        SESSION.into(),
        "--dir".into(),
        dir.to_string_lossy().into_owned(),
        "--threshold".into(),
        "1000".into(),
        "--auto".into(),
        "--max-cycles".into(),
        "1".into(),
        "--claude-bin".into(),
        stub,
    ]);
    assert_eq!(rc, 0);
    let latest = lineage_latest(&dir, SESSION);
    assert_ne!(latest, SESSION, "shell must have compacted before spawning");
    let args = fs::read_to_string(dir.join("args.log")).unwrap();
    assert!(args.contains("--resume"), "spawned with --resume: {args}");
    assert!(args.contains(&latest), "resumed the compacted descendant: {args}");
    assert!(!args.contains("continue\n"), "no kick without an active goal: {args}");
}

#[test]
fn shell_kicks_when_goal_is_active() {
    let dir = tmp_dir();
    let records = vec![
        user("u1", None, "count"),
        assistant("a1", "u1", "1"),
        goal_status("g1", "a1", false),
        last_prompt("a1", "count"),
    ];
    write_session(&dir, &records);
    let stub = write_stub(
        &dir,
        "stub-interactive.sh",
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/args.log\"\nexit 0\n",
    );
    // Tiny session stays under threshold; shell resumes it directly with the kick.
    let rc = cmd_shell(&[
        SESSION.into(),
        "--dir".into(),
        dir.to_string_lossy().into_owned(),
        "--threshold".into(),
        "60000".into(),
        "--auto".into(),
        "--max-cycles".into(),
        "1".into(),
        "--claude-bin".into(),
        stub,
    ]);
    assert_eq!(rc, 0);
    let args = fs::read_to_string(dir.join("args.log")).unwrap();
    assert!(
        args.lines().last() == Some("continue"),
        "active goal must trigger the kick-prompt: {args}"
    );
}

#[test]
fn scan_fast_skips_estimate_and_estimate_flag_computes_it() {
    let dir = tmp_dir();
    write_session(&dir, &prose_session());
    assert_eq!(cmd_scan(&[dir.to_string_lossy().into_owned()]), 0);
    assert_eq!(
        cmd_scan(&[dir.to_string_lossy().into_owned(), "--estimate".into()]),
        0
    );
}
