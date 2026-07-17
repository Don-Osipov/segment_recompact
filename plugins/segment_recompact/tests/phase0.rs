//! Phase-0 integration tests over synthetic sessions: active-path selection, fail-open user
//! classification, compaction-summary pinning, tool-pair sanitization, and verify.

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
        "message": {"role": "user", "content": [{"type": "text", "text": text}]}
    })
}

fn assistant(uuid: &str, parent: &str, text: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:01.000Z", "cwd": "/tmp/p", "version": "2.0.0",
        "userType": "external", "isSidechain": false,
        "message": {
            "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-4-7",
            "type": "message", "stop_reason": "end_turn",
            "usage": {"input_tokens": 1000, "output_tokens": 10},
            "content": [{"type": "text", "text": text}]
        }
    })
}

fn tool_use(uuid: &str, parent: &str, tool_id: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:02.000Z", "userType": "external", "isSidechain": false,
        "message": {
            "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-4-7",
            "type": "message", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": tool_id, "name": "Bash", "input": {"command": "true"}}]
        }
    })
}

fn tool_result(uuid: &str, parent: &str, tool_id: &str, text: &str) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:03.000Z", "userType": "external", "isSidechain": false,
        "sourceToolAssistantUUID": parent,
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": text}]}
    })
}

fn compact_summary(uuid: &str, text: &str) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": null, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:04.000Z", "userType": "external", "isSidechain": false,
        "isCompactSummary": true,
        "message": {"role": "user", "content": [{"type": "text", "text": text}]}
    })
}

fn last_prompt(leaf: &str, text: &str) -> Value {
    json!({"type": "last-prompt", "leafUuid": leaf, "sessionId": SESSION, "lastPrompt": text})
}

fn write_session(dir: &PathBuf, name: &str, records: &[Value]) -> PathBuf {
    let path = dir.join(name);
    let body: String = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
    fs::write(&path, body).unwrap();
    path
}

fn assemble_to(dir: &PathBuf, src: &PathBuf, summaries: &Value, keep: usize) -> PathBuf {
    let sums = dir.join("summaries.json");
    fs::write(&sums, serde_json::to_string(summaries).unwrap()).unwrap();
    let out = dir.join(format!("out-{}.jsonl", uuid_v4()));
    let rc = cmd_assemble(&[
        src.to_string_lossy().into_owned(),
        sums.to_string_lossy().into_owned(),
        "--keep".into(),
        keep.to_string(),
        "--out".into(),
        out.to_string_lossy().into_owned(),
    ]);
    assert_eq!(rc, 0, "assemble failed");
    out
}

fn verify_ok(out: &PathBuf, src: Option<&PathBuf>) {
    let mut args = vec![out.to_string_lossy().into_owned()];
    if let Some(s) = src {
        args.push("--source".into());
        args.push(s.to_string_lossy().into_owned());
    }
    assert_eq!(cmd_verify(&args), 0, "verify failed for {}", out.display());
}

fn file_contains(path: &PathBuf, needle: &str) -> bool {
    fs::read_to_string(path).unwrap().contains(needle)
}

// ---------------------------------------------------------------------------------------- tests

#[test]
fn active_path_drops_abandoned_branch() {
    let dir = tmp_dir();
    let records = vec![
        user("u1", None, "first ask"),
        assistant("a1", "u1", "first answer"),
        user("u2", Some("a1"), "second ask"),
        assistant("a2b", "u2", "ABANDONED branch answer"),
        assistant("a2", "u2", "final answer"),
        last_prompt("a2", "second ask"),
    ];
    let src = write_session(&dir, "branchy.jsonl", &records);

    let (kept, dropped) = select_active(load_jsonl(&src));
    assert_eq!(dropped, 1);
    assert!(kept.iter().all(|r| rec_uuid(r) != Some("a2b")));

    let out = assemble_to(&dir, &src, &json!({"0": "I answered the first ask."}), 1);
    verify_ok(&out, Some(&src));
    assert!(!file_contains(&out, "ABANDONED"));
    assert!(file_contains(&out, "final answer"));
}

#[test]
fn compacted_session_keeps_only_post_boundary() {
    let dir = tmp_dir();
    let records = vec![
        user("u0", None, "PRE-COMPACT ask"),
        assistant("a0", "u0", "PRE-COMPACT answer"),
        compact_summary("cs", "Summary of everything before the boundary."),
        user("u1", Some("cs"), "post-compact ask"),
        assistant("a1", "u1", "post-compact answer"),
        last_prompt("a1", "post-compact ask"),
    ];
    let src = write_session(&dir, "compacted.jsonl", &records);

    let (kept, dropped) = select_active(load_jsonl(&src));
    assert_eq!(dropped, 2, "pre-boundary records must be off-path");
    assert!(kept.iter().any(|r| truthy(r, "isCompactSummary")));

    // With keep=1 the only segment is the verbatim tail; no summaries needed.
    let out = assemble_to(&dir, &src, &json!({}), 1);
    verify_ok(&out, Some(&src));
    assert!(!file_contains(&out, "PRE-COMPACT"));
    assert!(file_contains(&out, "Summary of everything before the boundary."));
}

#[test]
fn compact_summary_segment_is_pinned_verbatim() {
    // A compaction summary landing inside a segment (not head) must pin that segment verbatim:
    // a hand-written summary of a compaction summary is recursive loss.
    let records = vec![
        user("u1", None, "ask"),
        assistant("a1", "u1", "answer"),
        compact_summary("cs", "boundary blob"),
        user("u2", Some("cs"), "later ask"),
        assistant("a2", "u2", "later answer"),
        last_prompt("a2", "later ask"),
    ];
    // Force the compact summary into segment 0's activity by making it chain mid-file.
    let mut records = records;
    records[2]["parentUuid"] = json!("a1");
    records[3]["parentUuid"] = json!("cs");

    let (kept, _) = select_active(records);
    let (_, segs) = segment(&kept);
    let plans = plan(&kept, &segs, 1);
    assert!(plans[0].kept_verbatim, "segment holding a compact summary must stay verbatim");
    assert!(!plans[0].needs_summary);
}

#[test]
fn image_first_user_turn_is_genuine() {
    let mut r = user("u1", None, "look at this");
    r["message"]["content"] = json!([
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}},
        {"type": "text", "text": "look at this"}
    ]);
    assert!(is_genuine_user(&r), "image-first prompts are still user turns");

    let mut unknown = user("u2", None, "x");
    unknown["message"]["content"] = json!([{"type": "some_future_block"}]);
    assert!(is_genuine_user(&unknown), "unknown shapes fail open to verbatim retention");

    let tr = tool_result("t1", "a1", "toolu_1", "output");
    assert!(!is_genuine_user(&tr));
    let mut tr_no_source = tr.clone();
    tr_no_source.as_object_mut().unwrap().remove("sourceToolAssistantUUID");
    assert!(
        !is_genuine_user(&tr_no_source),
        "tool_result content alone must classify as activity"
    );
}

#[test]
fn in_flight_tool_use_is_sanitized() {
    let dir = tmp_dir();
    let records = vec![
        user("u1", None, "run something"),
        tool_use("a1", "u1", "toolu_inflight"),
        last_prompt("a1", "run something"),
    ];
    let src = write_session(&dir, "inflight.jsonl", &records);
    let out = assemble_to(&dir, &src, &json!({}), 1);
    verify_ok(&out, Some(&src));
    assert!(!file_contains(&out, "toolu_inflight"));
}

#[test]
fn roundtrip_with_tools_preserves_user_turns_and_strips_usage() {
    let dir = tmp_dir();
    let records = vec![
        user("u1", None, "please build"),
        tool_use("a1", "u1", "toolu_1"),
        tool_result("t1", "a1", "toolu_1", "build ok"),
        assistant("a2", "t1", "built it"),
        user("u2", Some("a2"), "now test"),
        tool_use("a3", "u2", "toolu_2"),
        tool_result("t2", "a3", "toolu_2", "tests pass"),
        assistant("a4", "t2", "tests pass"),
        last_prompt("a4", "now test"),
    ];
    let src = write_session(&dir, "tools.jsonl", &records);
    let out = assemble_to(&dir, &src, &json!({"0": "I built the project; the build succeeded."}), 1);
    verify_ok(&out, Some(&src));

    let assembled = load_jsonl(&out);
    assert!(assembled.iter().all(|r| r.pointer("/message/usage").is_none()));
    // Collapsed segment 0: its tool pair is gone, replaced by the synthetic summary.
    assert!(!file_contains(&out, "toolu_1"));
    assert!(file_contains(&out, "recompactSynthetic"));
    // Verbatim tail keeps its pair.
    assert!(file_contains(&out, "toolu_2"));
}

#[test]
fn verify_catches_dangling_tool_use_and_broken_chain() {
    let dir = tmp_dir();
    // Dangling tool_use, no sanitization (file written directly, not via assemble).
    let bad = write_session(
        &dir,
        "bad-pair.jsonl",
        &[
            user("u1", None, "hi"),
            tool_use("a1", "u1", "toolu_lost"),
            last_prompt("a1", "hi"),
        ],
    );
    assert_eq!(cmd_verify(&[bad.to_string_lossy().into_owned()]), 1);

    // Broken chain: second record does not point at the first.
    let bad2 = write_session(
        &dir,
        "bad-chain.jsonl",
        &[
            user("u1", None, "hi"),
            assistant("a1", "NOT-u1", "answer"),
            last_prompt("a1", "hi"),
        ],
    );
    assert_eq!(cmd_verify(&[bad2.to_string_lossy().into_owned()]), 1);
}

#[test]
fn select_active_falls_back_without_last_prompt() {
    // No last-prompt record: leaf = last uuid record, path still resolves.
    let records = vec![
        user("u1", None, "ask"),
        assistant("a1", "u1", "answer"),
        assistant("a1b", "u1", "abandoned"),
        assistant("a2", "a1", "follow-up"),
    ];
    let (kept, dropped) = select_active(records);
    assert_eq!(dropped, 1);
    assert!(kept.iter().all(|r| rec_uuid(r) != Some("a1b")));
}
