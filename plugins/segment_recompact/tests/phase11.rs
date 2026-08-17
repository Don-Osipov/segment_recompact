//! Unit-key stability across generations: every path that computes unit keys and content hashes
//! must see the SAME records. `assemble` strips the previous generation's orientation preamble
//! before segmenting; when `extract` (and `continue`'s pre-pass) did not, the one unit containing
//! the preamble hashed differently on each side, so its cached summary could never be found and
//! every re-compaction of an already-compacted lineage stalled on exactly that unit.

use recompact::*;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

const SESSION: &str = "sess-gen2";

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-08-17T00:00:00.000Z", "cwd": "/tmp/p", "version": "2.0.0",
        "userType": "external", "isSidechain": false,
        "message": {"role": "user", "content": [{"type": "text", "text": text}]}
    })
}

fn assistant(uuid: &str, parent: &str, text: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-08-17T00:00:01.000Z", "userType": "external", "isSidechain": false,
        "message": {
            "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-4-7",
            "type": "message", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}]
        }
    })
}

/// An assistant record carrying the previous generation's orientation preamble.
fn preamble(uuid: &str, parent: &str) -> Value {
    let mut r = assistant(uuid, parent, "This transcript was compacted by segment_recompact; ...");
    r["recompactPreamble"] = json!(true);
    r
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

const U1: &str = "11111111-0000-4000-8000-000000000001";
const A1: &str = "22222222-0000-4000-8000-000000000002";
const P1: &str = "55555555-0000-4000-8000-000000000005";
const U2: &str = "33333333-0000-4000-8000-000000000003";
const A2: &str = "44444444-0000-4000-8000-000000000004";

/// A resumed compacted twin: segment 0 carries a preamble record from the prior generation.
fn resumed_twin_session() -> Vec<Value> {
    vec![
        user(U1, None, "carry on with the work"),
        assistant(A1, U1, &format!("WORKING {}", "lorem ".repeat(400))),
        preamble(P1, A1),
        user(U2, Some(P1), "thanks"),
        assistant(A2, U2, "done"),
        last_prompt(A2, "thanks"),
    ]
}

/// Unit keys and their content hashes, as `extract` writes them into the worksheet.
fn worksheet_hashes(dir: &PathBuf, src: &PathBuf, keep: usize) -> Vec<(String, String)> {
    let out = dir.join(format!("segments-{}.json", uuid_v4()));
    assert_eq!(
        cmd_extract(&[
            src.to_string_lossy().into_owned(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
            "--keep".into(),
            keep.to_string(),
        ]),
        0
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
    let mut got = Vec::new();
    for seg in doc["segments"].as_array().unwrap() {
        match seg.get("parts").and_then(|p| p.as_array()) {
            Some(parts) => {
                for p in parts {
                    got.push((
                        p["key"].as_str().unwrap().to_string(),
                        p["content_hash"].as_str().unwrap().to_string(),
                    ));
                }
            }
            None => got.push((
                seg["index"].as_u64().unwrap().to_string(),
                seg["content_hash"].as_str().unwrap().to_string(),
            )),
        }
    }
    got
}

// ---------------------------------------------------------------------------------------- tests

/// The regression itself: extract's worksheet hashes must equal the ones the assemble/continue
/// side computes, for every unit — including the one holding the old preamble.
#[test]
fn extract_and_assemble_agree_on_unit_hashes_across_a_preamble() {
    let dir = tmp_dir();
    let src = write_session(&dir, &format!("{SESSION}.jsonl"), &resumed_twin_session());

    let from_worksheet = worksheet_hashes(&dir, &src, 1);
    assert!(!from_worksheet.is_empty(), "worksheet has units");

    // The assemble/continue side of the house.
    let (active, _) = select_active_for_units(load_jsonl(&src));
    let units = build_units_ex(&active, 1, DEFAULT_SPLIT_THRESHOLD, false);

    for (key, hash) in &from_worksheet {
        let theirs = units
            .key_hashes
            .get(key)
            .unwrap_or_else(|| panic!("assemble side has no unit {key}; worksheet keys disagree"));
        assert_eq!(
            theirs, hash,
            "unit {key}: extract hashed {hash}, assemble hashed {theirs} — a cached summary \
             written under one can never be found under the other"
        );
    }
}

/// The user-visible symptom the mismatch produced: a summary cached from extract's worksheet
/// hash must satisfy assemble, leaving nothing in `need_summaries`.
#[test]
fn cached_summary_from_worksheet_satisfies_assemble() {
    let dir = tmp_dir();
    let src = write_session(&dir, &format!("{SESSION}.jsonl"), &resumed_twin_session());

    // Cache every unit under the hash extract published, as `continue`'s pre-pass does.
    let cache: serde_json::Map<String, Value> = worksheet_hashes(&dir, &src, 1)
        .into_iter()
        .map(|(_, h)| (h, json!("cached recap of this unit")))
        .collect();
    let cache_path = dir.join(".recompact-summary-cache.json");
    fs::write(&cache_path, serde_json::to_string(&cache).unwrap()).unwrap();

    // An empty summaries file: every needed summary must come from the cache.
    let sums = dir.join("summaries.json");
    fs::write(&sums, "{}").unwrap();
    let out = dir.join("twin.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            sums.to_string_lossy().into_owned(),
            "--keep".into(),
            "1".into(),
            "--cache".into(),
            cache_path.to_string_lossy().into_owned(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ]),
        0,
        "assemble must resolve every summary from the worksheet-keyed cache"
    );
    assert!(out.exists(), "twin written");
}

/// Preambles are the only thing this view drops: the active-path selection itself is unchanged,
/// so a session that never carried one hashes exactly as before (existing caches stay valid).
#[test]
fn preamble_free_sessions_are_unaffected() {
    let dir = tmp_dir();
    let plain = vec![
        user(U1, None, "carry on with the work"),
        assistant(A1, U1, &format!("WORKING {}", "lorem ".repeat(400))),
        user(U2, Some(A1), "thanks"),
        assistant(A2, U2, "done"),
        last_prompt(A2, "thanks"),
    ];
    let src = write_session(&dir, &format!("{SESSION}.jsonl"), &plain);

    let (raw, _) = select_active(load_jsonl(&src));
    let (units_view, _) = select_active_for_units(load_jsonl(&src));
    assert_eq!(
        raw.len(),
        units_view.len(),
        "no preamble to strip: the two views must be identical"
    );

    let a = build_units_ex(&raw, 1, DEFAULT_SPLIT_THRESHOLD, false);
    let b = build_units_ex(&units_view, 1, DEFAULT_SPLIT_THRESHOLD, false);
    assert_eq!(a.key_hashes, b.key_hashes, "hashes unchanged for such sessions");
}
