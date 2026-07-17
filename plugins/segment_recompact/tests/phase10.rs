//! Agent legibility: everything a fresh model needs must be model-VISIBLE (message content),
//! because record envelopes (uuids, provenance, part keys) are never shown at inference time.
//! Preamble record, summary footers, marker selectors, and prefix-tolerant rehydrate.

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

fn write_session(dir: &PathBuf, name: &str, records: &[Value]) -> PathBuf {
    let path = dir.join(name);
    let body: String = records.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
    fs::write(&path, body).unwrap();
    path
}

fn assemble(dir: &PathBuf, src: &PathBuf, summaries: &Value) -> Vec<Value> {
    let sums = dir.join("summaries.json");
    fs::write(&sums, serde_json::to_string(summaries).unwrap()).unwrap();
    let out = dir.join(format!("out-{}.jsonl", uuid_v4()));
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
    load_jsonl(&out)
}

fn two_segment_session() -> Vec<Value> {
    vec![
        user("11111111-0000-4000-8000-000000000001", None, "do the research"),
        assistant("22222222-0000-4000-8000-000000000002", "11111111-0000-4000-8000-000000000001", &format!("FINDINGS {}", "lorem ".repeat(500))),
        user("33333333-0000-4000-8000-000000000003", Some("22222222-0000-4000-8000-000000000002"), "thanks"),
        assistant("44444444-0000-4000-8000-000000000004", "33333333-0000-4000-8000-000000000003", "done"),
        last_prompt("44444444-0000-4000-8000-000000000004", "thanks"),
    ]
}

// ---------------------------------------------------------------------------------------- tests

#[test]
fn preamble_present_early_and_regenerated_not_duplicated() {
    let dir = tmp_dir();
    let src = write_session(&dir, format!("{SESSION}.jsonl").as_str(), &two_segment_session());
    let twin = assemble(&dir, &src, &json!({"0": "Did the research."}));
    let preambles: Vec<&Value> = twin.iter().filter(|r| truthy(r, "recompactPreamble")).collect();
    assert_eq!(preambles.len(), 1, "exactly one preamble");
    let text = preambles[0].pointer("/message/content/0/text").and_then(|v| v.as_str()).unwrap();
    assert!(text.contains("recompact rehydrate"), "recovery command taught: {text}");
    assert!(text.contains("recompact continue"), "self-compaction taught");
    assert!(text.contains(SESSION), "names its source session");
    let pos = twin.iter().position(|r| truthy(r, "recompactPreamble")).unwrap();
    assert!(pos <= 2, "preamble lands early, got position {pos}");

    // Second generation: the old preamble is stripped and a fresh one minted — never two.
    let twin_path = write_session(&dir, "gen1.jsonl", &twin);
    let twin2 = assemble(&dir, &twin_path, &json!({}));
    assert_eq!(
        twin2.iter().filter(|r| truthy(r, "recompactPreamble")).count(),
        1,
        "regenerated, not accumulated"
    );
}

#[test]
fn summary_footer_names_its_part_key() {
    let dir = tmp_dir();
    let src = write_session(&dir, format!("{SESSION}.jsonl").as_str(), &two_segment_session());
    let twin = assemble(&dir, &src, &json!({"0": "Did the research."}));
    let synth = twin.iter().find(|r| truthy(r, "recompactSynthetic")).expect("synthetic exists");
    let text = synth.pointer("/message/content/0/text").and_then(|v| v.as_str()).unwrap();
    assert!(
        text.ends_with("[recompact summary 0 — rehydratable]"),
        "footer carries the selector: {text}"
    );
}

#[test]
fn mask_markers_carry_uuid_prefix_selector() {
    let r = json!({
        "type": "user", "uuid": "abcdef12-9999-4000-8000-000000000009", "parentUuid": "a1",
        "sessionId": SESSION, "userType": "external", "isSidechain": false,
        "sourceToolAssistantUUID": "a1",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "y".repeat(3000)}
        ]}
    });
    match mask_record(&r) {
        Masked::Replaced(v) => {
            let s = serde_json::to_string(&v).unwrap();
            assert!(s.contains("rehydrate abcdef12"), "marker names its own selector: {s}");
        }
        _ => panic!("oversized result must be masked"),
    }
}

#[test]
fn rehydrate_resolves_marker_style_uuid_prefix() {
    let dir = tmp_dir();
    let src = write_session(&dir, format!("{SESSION}.jsonl").as_str(), &two_segment_session());
    let twin = assemble(&dir, &src, &json!({"0": "Did the research."}));
    let got = rehydrate_select(&twin, "22222222").expect("8-char prefix resolves");
    assert_eq!(got.len(), 1);
    assert!(serde_json::to_string(&got[0]).unwrap().contains("FINDINGS"));
}
