//! Recursive rehydration: records buried N compaction generations deep must resolve to ground
//! truth. Born live: a research report three generations back was provably chain-reachable by
//! hand, but `rehydrate` only followed one hop and errored.

use recompact::*;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn raw(uuid: &str, text: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": null, "sessionId": "g0",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
    })
}

fn masked_copy(uuid: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": null, "sessionId": "g1",
        "recompactMasked": true,
        "message": {"role": "assistant", "content": [{"type": "text", "text": "[recompact: elided]"}]}
    })
}

fn synthetic(uuid: &str, part: &str, covered: &[&str], source: &PathBuf) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": null, "sessionId": "gx",
        "recompactSynthetic": true,
        "recompactProvenance": {
            "part": part,
            "coveredUuids": covered,
            "source": source.to_string_lossy(),
            "sourceSessionId": "prior"
        },
        "message": {"role": "user", "content": [{"type": "text", "text": format!("[summary of part {part}]")}]}
    })
}

fn write(dir: &PathBuf, name: &str, records: &[Value]) -> PathBuf {
    let path = dir.join(name);
    let body: String = records.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
    fs::write(&path, body).unwrap();
    path
}

const U_REPORT: &str = "aaaaaaaa-1111-4000-8000-000000000001";
const U_MASKED: &str = "bbbbbbbb-2222-4000-8000-000000000002";

/// gen0 holds ground truth; gen1 summarizes the report and carries a masked copy of the other
/// record; gen2 summarizes gen1's synthetic AND the masked copy. Returns (gen2 records, dir).
fn three_generations() -> (Vec<Value>, PathBuf) {
    let dir = tmp_dir();
    let g0 = write(&dir, "g0.jsonl", &[
        raw(U_REPORT, "RESEARCH-REPORT: 18 searches, 12 fetches, ranked top-10"),
        raw(U_MASKED, &format!("HUGE-RESULT {}", "x".repeat(3000))),
    ]);
    let s1 = synthetic("cccccccc-3333-4000-8000-000000000003", "0", &[U_REPORT], &g0);
    let g1 = write(&dir, "g1.jsonl", &[s1.clone(), masked_copy(U_MASKED)]);
    let s2 = synthetic(
        "dddddddd-4444-4000-8000-000000000004",
        "0",
        &["cccccccc-3333-4000-8000-000000000003", U_MASKED],
        &g1,
    );
    (vec![s2], dir)
}

// ---------------------------------------------------------------------------------------- tests

#[test]
fn uuid_selector_resolves_across_two_generations() {
    let (gen2, _dir) = three_generations();
    let got = rehydrate_select(&gen2, U_REPORT).expect("deep uuid resolves");
    assert_eq!(got.len(), 1);
    let text = serde_json::to_string(&got[0]).unwrap();
    assert!(text.contains("RESEARCH-REPORT"), "ground truth recovered: {text}");
}

#[test]
fn part_expansion_descends_through_nested_synthetics_and_unmasks() {
    let (gen2, _dir) = three_generations();
    let got = rehydrate_select(&gen2, "0").expect("part selector expands");
    let all = got.iter().map(|r| serde_json::to_string(r).unwrap()).collect::<Vec<_>>().join("\n");
    assert!(all.contains("RESEARCH-REPORT"), "nested synthetic expanded to raw: {all}");
    assert!(all.contains("HUGE-RESULT"), "masked copy swapped for the clean gen0 record: {all}");
    assert!(!all.contains("[summary of part"), "no synthetic left in the expansion: {all}");
    assert!(!all.contains("recompactMasked"), "no masked copy left in the expansion");
}

#[test]
fn deleted_ground_truth_degrades_to_nearest_surviving_copy() {
    let (gen2, dir) = three_generations();
    fs::remove_file(dir.join("g0.jsonl")).unwrap();
    // The report's only copy was g0: expansion keeps the gen1 synthetic as the best copy left.
    let got = rehydrate_select(&gen2, "0").expect("expansion still succeeds");
    let all = got.iter().map(|r| serde_json::to_string(r).unwrap()).collect::<Vec<_>>().join("\n");
    assert!(all.contains("[summary of part 0]"), "summary survives as best copy: {all}");
    assert!(all.contains("recompactMasked"), "masked copy survives as best copy: {all}");
}
