//! Autonomous continuation tests: the continue/resume/scan loop that lets a session compact
//! itself and keep running without human input.

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

fn tool_pair(u_use: &str, parent: &str, u_res: &str, tool_id: &str, size: usize) -> Vec<Value> {
    vec![
        json!({
            "type": "assistant", "uuid": u_use, "parentUuid": parent, "sessionId": SESSION,
            "timestamp": "2026-07-17T00:00:02.000Z", "userType": "external", "isSidechain": false,
            "message": {
                "id": format!("msg_{u_use}"), "role": "assistant", "model": "claude-opus-4-7",
                "type": "message", "stop_reason": "tool_use",
                "content": [{"type": "tool_use", "id": tool_id, "name": "Bash", "input": {"command": "make"}}]
            }
        }),
        json!({
            "type": "user", "uuid": u_res, "parentUuid": u_use, "sessionId": SESSION,
            "timestamp": "2026-07-17T00:00:03.000Z", "userType": "external", "isSidechain": false,
            "sourceToolAssistantUUID": u_use,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": tool_id, "content": "y".repeat(size)}
            ]}
        }),
    ]
}

fn last_prompt(leaf: &str, text: &str) -> Value {
    json!({"type": "last-prompt", "leafUuid": leaf, "sessionId": SESSION, "lastPrompt": text})
}

/// Tool-heavy session, written as <sessionId>.jsonl the way real project dirs store it.
fn write_compressible_session(dir: &PathBuf) -> PathBuf {
    let mut records = vec![user("u1", None, "build it")];
    let mut parent = "u1".to_string();
    for n in 0..3 {
        let (uu, ur) = (format!("a{n}"), format!("r{n}"));
        records.extend(tool_pair(&uu, &parent, &ur, &format!("t{n}"), 6000));
        parent = ur;
    }
    records.push(assistant("afin", &parent, "built"));
    records.push(user("u2", Some("afin"), "thanks"));
    records.push(assistant("afin2", "u2", "welcome"));
    records.push(last_prompt("afin2", "thanks"));
    let path = dir.join(format!("{SESSION}.jsonl"));
    let body: String = records.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
    fs::write(&path, body).unwrap();
    path
}

fn jsonl_count(dir: &PathBuf) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .count()
}

// ---------------------------------------------------------------------------------------- tests

#[test]
fn continue_under_threshold_is_identity() {
    let dir = tmp_dir();
    let src = write_compressible_session(&dir);
    assert_eq!(
        cmd_continue(&[src.to_string_lossy().into_owned(), "--threshold".into(), "1000000".into()]),
        0
    );
    assert_eq!(jsonl_count(&dir), 1, "no compaction under threshold");
    assert!(!dir.join(".recompact-lineage.json").exists());
}

#[test]
fn continue_compacts_registers_and_resolves() {
    let dir = tmp_dir();
    let src = write_compressible_session(&dir);
    assert_eq!(
        cmd_continue(&[src.to_string_lossy().into_owned(), "--threshold".into(), "1000".into()]),
        0
    );
    assert_eq!(jsonl_count(&dir), 2, "compacted descendant created");
    let latest = lineage_latest(&dir, SESSION);
    assert_ne!(latest, SESSION, "lineage must resolve to the descendant");
    let new_file = dir.join(format!("{latest}.jsonl"));
    assert!(new_file.exists());
    // The descendant verifies against its source.
    assert_eq!(
        cmd_verify(&[
            new_file.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned(),
        ]),
        0
    );
    // A second continue on the ORIGINAL id resolves through lineage; the (now small) descendant
    // is under threshold, so no third file appears.
    assert_eq!(
        cmd_continue(&[src.to_string_lossy().into_owned(), "--threshold".into(), "1000".into()]),
        0
    );
    assert_eq!(jsonl_count(&dir), 2, "resolution prevented churn");
    // resume is a thin wrapper over the same resolution.
    assert_eq!(cmd_resume(&[src.to_string_lossy().into_owned()]), 0);
}

#[test]
fn continue_churn_guard_keeps_incompressible_sessions_stable() {
    let dir = tmp_dir();
    // One giant genuine user turn: verbatim forever, nothing maskable.
    let records = vec![
        user("u1", None, &"important human text ".repeat(2000)),
        assistant("a1", "u1", "noted"),
        user("u2", Some("a1"), "ok"),
        assistant("a2", "u2", "done"),
        last_prompt("a2", "ok"),
    ];
    let path = dir.join(format!("{SESSION}.jsonl"));
    let body: String = records.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
    fs::write(&path, body).unwrap();

    assert_eq!(
        cmd_continue(&[path.to_string_lossy().into_owned(), "--threshold".into(), "1000".into()]),
        0
    );
    assert_eq!(jsonl_count(&dir), 1, "churn guard must remove the pointless descendant");
    assert_eq!(
        lineage_latest(&dir, SESSION),
        SESSION,
        "no dangling lineage entry after the churn guard"
    );
}

#[test]
fn scan_reports_lineage_flags() {
    let dir = tmp_dir();
    let src = write_compressible_session(&dir);
    assert_eq!(
        cmd_continue(&[src.to_string_lossy().into_owned(), "--threshold".into(), "1000".into()]),
        0
    );
    assert_eq!(cmd_scan(&[dir.to_string_lossy().into_owned()]), 0);
}
