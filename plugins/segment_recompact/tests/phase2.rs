//! Phase-2 integration tests: the iterated-recompaction lifecycle. Summary cache reuse, ledger
//! injection and supersession, and the double-recompaction provenance chain.

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

fn write_jsonl_file(dir: &PathBuf, name: &str, records: &[Value]) -> PathBuf {
    let path = dir.join(name);
    let body: String = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
    fs::write(&path, body).unwrap();
    path
}

fn three_turn_session() -> Vec<Value> {
    vec![
        user("u1", None, "first ask"),
        assistant("a1", "u1", "first answer with FACT-ALPHA"),
        user("u2", Some("a1"), "second ask"),
        assistant("a2", "u2", "second answer"),
        user("u3", Some("a2"), "third ask"),
        assistant("a3", "u3", "third answer"),
        last_prompt("a3", "third ask"),
    ]
}

fn run_assemble(args: &[String]) -> i32 {
    cmd_assemble(args)
}

// ---------------------------------------------------------------------------------------- tests

#[test]
fn cache_supplies_summaries_on_second_pass() {
    let dir = tmp_dir();
    let src = write_jsonl_file(&dir, "a.jsonl", &three_turn_session());
    let cache = dir.join("cache.json");
    let sums = dir.join("sums.json");
    fs::write(
        &sums,
        json!({"0": "I answered the first ask (FACT-ALPHA).", "1": "I answered the second ask."})
            .to_string(),
    )
    .unwrap();
    let out1 = dir.join("b1.jsonl");
    assert_eq!(
        run_assemble(&[
            src.to_string_lossy().into_owned(),
            sums.to_string_lossy().into_owned(),
            "--cache".into(),
            cache.to_string_lossy().into_owned(),
            "--out".into(),
            out1.to_string_lossy().into_owned(),
        ]),
        0
    );
    assert!(cache.exists(), "cache file written");

    // Second pass over the SAME source with EMPTY summaries: everything resolves from cache.
    let empty = dir.join("empty.json");
    fs::write(&empty, "{}").unwrap();
    let out2 = dir.join("b2.jsonl");
    assert_eq!(
        run_assemble(&[
            src.to_string_lossy().into_owned(),
            empty.to_string_lossy().into_owned(),
            "--cache".into(),
            cache.to_string_lossy().into_owned(),
            "--out".into(),
            out2.to_string_lossy().into_owned(),
        ]),
        0,
        "cache must supply the missing summaries"
    );
    let text = fs::read_to_string(&out2).unwrap();
    assert!(text.contains("FACT-ALPHA"));
    // Without the cache the same call fails.
    let out3 = dir.join("b3.jsonl");
    assert_eq!(
        run_assemble(&[
            src.to_string_lossy().into_owned(),
            empty.to_string_lossy().into_owned(),
            "--out".into(),
            out3.to_string_lossy().into_owned(),
        ]),
        1
    );
}

#[test]
fn ledger_is_injected_before_tail_and_superseded() {
    let dir = tmp_dir();
    let src = write_jsonl_file(&dir, "a.jsonl", &three_turn_session());
    let sums = dir.join("sums.json");
    fs::write(
        &sums,
        json!({
            "0": "First summary.",
            "1": "Second summary.",
            "ledger": "Never push to main without asking."
        })
        .to_string(),
    )
    .unwrap();
    let out1 = dir.join("b1.jsonl");
    assert_eq!(
        run_assemble(&[
            src.to_string_lossy().into_owned(),
            sums.to_string_lossy().into_owned(),
            "--out".into(),
            out1.to_string_lossy().into_owned(),
        ]),
        0
    );
    let b1 = load_jsonl(&out1);
    let ledgers: Vec<usize> = b1
        .iter()
        .enumerate()
        .filter(|(_, r)| truthy(r, "recompactLedger"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(ledgers.len(), 1);
    // The ledger sits immediately before the verbatim tail's user turn (u3).
    let u3_pos = b1
        .iter()
        .position(|r| rec_uuid(r) == Some("u3"))
        .expect("tail user turn present");
    assert_eq!(ledgers[0] + 1, u3_pos, "ledger must immediately precede the tail");
    assert_eq!(cmd_verify(&[out1.to_string_lossy().into_owned()]), 0);

    // Continue the session, recompact again with a NEW ledger: the old record must vanish.
    let mut continued = b1.clone();
    continued.retain(|r| rec_type(r) != "last-prompt");
    let leaf = continued.iter().rev().find_map(rec_uuid).unwrap().to_string();
    let new_sess = continued
        .iter()
        .find_map(|r| r.get("sessionId").and_then(|v| v.as_str()))
        .unwrap()
        .to_string();
    let mut u4 = user("u4", Some(leaf.as_str()), "fourth ask");
    u4["sessionId"] = json!(new_sess);
    let mut a4 = assistant("a4", "u4", "fourth answer");
    a4["sessionId"] = json!(new_sess);
    continued.push(u4);
    continued.push(a4);
    continued.push(json!({"type": "last-prompt", "leafUuid": "a4", "sessionId": new_sess, "lastPrompt": "fourth ask"}));
    let src2 = write_jsonl_file(&dir, "continued.jsonl", &continued);

    let sums2 = dir.join("sums2.json");
    fs::write(
        &sums2,
        json!({
            "2": "Third summary.",
            "ledger": "Never push to main without asking. Always run the tests."
        })
        .to_string(),
    )
    .unwrap();
    let out2 = dir.join("c1.jsonl");
    assert_eq!(
        run_assemble(&[
            src2.to_string_lossy().into_owned(),
            sums2.to_string_lossy().into_owned(),
            "--out".into(),
            out2.to_string_lossy().into_owned(),
        ]),
        0
    );
    let text = fs::read_to_string(&out2).unwrap();
    assert_eq!(
        text.matches("recompactLedger").count(),
        1,
        "exactly one ledger after supersession"
    );
    assert!(text.contains("Always run the tests."));
    assert_eq!(cmd_verify(&[out2.to_string_lossy().into_owned()]), 0);
}

#[test]
fn double_recompaction_preserves_multihop_provenance() {
    let dir = tmp_dir();
    // Pass 1: A -> B. Segment 0 collapses into a synthetic summary whose provenance points at A.
    let src_a = write_jsonl_file(&dir, "a.jsonl", &three_turn_session());
    let sums = dir.join("sums.json");
    fs::write(
        &sums,
        json!({"0": "I answered the first ask (FACT-ALPHA).", "1": "I answered the second ask."})
            .to_string(),
    )
    .unwrap();
    let out_b = dir.join("b.jsonl");
    assert_eq!(
        run_assemble(&[
            src_a.to_string_lossy().into_owned(),
            sums.to_string_lossy().into_owned(),
            "--out".into(),
            out_b.to_string_lossy().into_owned(),
        ]),
        0
    );

    // The session continues after B (as a resumed session would).
    let mut b = load_jsonl(&out_b);
    let b_sess = b
        .iter()
        .find_map(|r| r.get("sessionId").and_then(|v| v.as_str()))
        .unwrap()
        .to_string();
    b.retain(|r| rec_type(r) != "last-prompt");
    let leaf = b.iter().rev().find_map(rec_uuid).unwrap().to_string();
    let mut u4 = user("u4", Some(leaf.as_str()), "fourth ask");
    u4["sessionId"] = json!(b_sess);
    let mut a4 = assistant("a4", "u4", "fourth answer");
    a4["sessionId"] = json!(b_sess);
    b.push(u4);
    b.push(a4);
    b.push(json!({"type": "last-prompt", "leafUuid": "a4", "sessionId": b_sess, "lastPrompt": "fourth ask"}));
    let src_b = write_jsonl_file(&dir, "b-continued.jsonl", &b);

    // Pass 2: B -> C. The B-era synthetic summary must be pinned (never re-summarized); the newly
    // collapsible segments get fresh summaries whose provenance points at B.
    let ws = dir.join("ws.json");
    assert_eq!(
        cmd_extract(&[
            src_b.to_string_lossy().into_owned(),
            "--out".into(),
            ws.to_string_lossy().into_owned(),
        ]),
        0
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&ws).unwrap()).unwrap();
    let seg0 = &doc["segments"][0];
    assert_eq!(seg0["kept_verbatim"], true, "synthetic-summary segment is pinned");
    assert_eq!(seg0["needs_summary"], false);

    let needs: Vec<String> = doc["segments_needing_summary"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let sums2: Value = needs
        .iter()
        .map(|s| (s.clone(), Value::String(format!("Summary for segment {s}."))))
        .collect::<serde_json::Map<String, Value>>()
        .into();
    let sums2_path = dir.join("sums2.json");
    fs::write(&sums2_path, sums2.to_string()).unwrap();
    let out_c = dir.join("c.jsonl");
    assert_eq!(
        run_assemble(&[
            src_b.to_string_lossy().into_owned(),
            sums2_path.to_string_lossy().into_owned(),
            "--out".into(),
            out_c.to_string_lossy().into_owned(),
        ]),
        0
    );
    assert_eq!(
        cmd_verify(&[
            out_c.to_string_lossy().into_owned(),
            "--source".into(),
            src_b.to_string_lossy().into_owned(),
        ]),
        0
    );

    let c = load_jsonl(&out_c);
    let synths: Vec<&Value> = c.iter().filter(|r| truthy(r, "recompactSynthetic")).collect();
    assert!(synths.len() >= 2, "B-era summary carried + at least one new C summary");
    let sources: Vec<&str> = synths
        .iter()
        .filter_map(|r| r.pointer("/recompactProvenance/source").and_then(|v| v.as_str()))
        .collect();
    // Multi-hop lineage: the carried B-era summary still points at A; new summaries point at B.
    assert!(
        sources.iter().any(|s| s.ends_with("a.jsonl")),
        "B-era summary provenance must still reach A: {sources:?}"
    );
    assert!(
        sources.iter().any(|s| s.ends_with("b-continued.jsonl")),
        "new summary provenance must point at B: {sources:?}"
    );
    // The pass-1 summary text survived verbatim (never re-summarized).
    assert!(fs::read_to_string(&out_c).unwrap().contains("FACT-ALPHA"));
}
