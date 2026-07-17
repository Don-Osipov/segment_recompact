//! Image-aware compaction: flat token weight for base64 images, elision of image bytes in user
//! turns outside the keep tail (text stays verbatim), and image masking inside tool results.
//! Born from a live failure: three ~600KB screenshots pinned in user turns made a session read
//! as ~616k tokens, compaction reported "no meaningful reduction", and the loop stalled.

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

fn big_image_block() -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(200_000)}})
}

fn user_with_image(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:00.000Z", "cwd": "/tmp/p", "version": "2.0.0",
        "userType": "external", "isSidechain": false,
        "message": {"role": "user", "content": [
            big_image_block(),
            {"type": "text", "text": text}
        ]}
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

// ---------------------------------------------------------------------------------------- tests

#[test]
fn image_tokens_are_flat_not_base64_length() {
    let r = user_with_image("u1", None, "look");
    let raw = serde_json::to_string(&r).unwrap().len() / 4; // ~50k "tokens" the old way
    let est = approx_tokens(std::slice::from_ref(&r));
    assert!(est < 3000, "flat image weight expected, got {est}");
    assert!(raw > 40_000, "fixture must be genuinely heavy, got {raw}");
}

#[test]
fn old_user_turn_images_elided_recent_kept() {
    let dir = tmp_dir();
    let records = vec![
        user_with_image("u1", None, "first screenshot"),
        assistant("a1", "u1", "I looked at the first screenshot."),
        user_with_image("u2", Some("a1"), "second screenshot"),
        assistant("a2", "u2", "And the second."),
        last_prompt("a2", "second screenshot"),
    ];
    let src = write_session(&dir, &records);
    let sums = dir.join("summaries.json");
    fs::write(&sums, serde_json::to_string(&json!({"0": "Reviewed the first screenshot."})).unwrap()).unwrap();
    let out = dir.join("out.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            sums.to_string_lossy().into_owned(),
            "--keep".into(),
            "1".into(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ]),
        0
    );
    let text = fs::read_to_string(&out).unwrap();
    let recs: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let u1 = recs.iter().find(|r| rec_uuid(r) == Some("u1")).expect("u1 kept");
    assert_eq!(u1.get("recompactImagesElided").and_then(|v| v.as_u64()), Some(1));
    assert!(user_text(u1).contains("first screenshot"), "user text verbatim");
    assert!(serde_json::to_string(u1).unwrap().contains("image elided"));
    assert!(serde_json::to_string(u1).unwrap().len() < 5_000, "bytes actually gone");
    let u2 = recs.iter().find(|r| rec_uuid(r) == Some("u2")).expect("u2 kept");
    assert!(
        serde_json::to_string(u2).unwrap().len() > 100_000,
        "keep-tail image must survive untouched"
    );
    // The fidelity check must accept elided images: text is what is compared.
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
fn mask_elides_images_inside_tool_results() {
    let r = json!({
        "type": "user", "uuid": "tr1", "parentUuid": "a1", "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:03.000Z", "userType": "external", "isSidechain": false,
        "sourceToolAssistantUUID": "a1",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [big_image_block()]}
        ]}
    });
    match mask_record(&r) {
        Masked::Replaced(v) => {
            let s = serde_json::to_string(&v).unwrap();
            assert!(s.contains("image elided"), "marker present");
            assert!(s.len() < 5_000, "image bytes gone: {} bytes", s.len());
        }
        _ => panic!("image-bearing tool_result must be masked"),
    }
}
