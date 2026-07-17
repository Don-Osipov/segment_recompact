//! Lifecycle-branch tests: delivered-content classification, delegation-aware splitting, and the
//! dynamic budget planner.

use recompact::*;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

const SESSION: &str = "sess-orig";
const TEAMMATE_PREFIX: &str = "Another Claude session sent a message:\n<teammate-message teammate_id=\"researcher\">";

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

// ---------------------------------------------------------------- delivered-content tests

#[test]
fn delivered_kind_detects_only_anchored_sentinels() {
    let teammate = user("t1", None, &format!("{TEAMMATE_PREFIX}\nbig report body</teammate-message>"));
    assert_eq!(delivered_kind(&teammate), Some("teammate_message"));
    assert!(!is_genuine_user(&teammate));

    let notif = user("t2", None, "<task-notification>\n<task-id>abc</task-id>\n</task-notification>");
    assert_eq!(delivered_kind(&notif), Some("task_notification"));
    assert!(!is_genuine_user(&notif));

    // A human QUOTING the framing mid-message stays genuine.
    let quoting = user("h1", None, "my goal mentions teammate-message and <task-notification> records");
    assert_eq!(delivered_kind(&quoting), None);
    assert!(is_genuine_user(&quoting));

    // Prefix without the full framing stays genuine (fail-open).
    let partial = user("h2", None, "Another Claude session sent a message: hello");
    assert_eq!(delivered_kind(&partial), None);
    assert!(is_genuine_user(&partial));

    // Array-content first text block with the sentinel also classifies as delivered.
    let mut arr = user("t3", None, "x");
    arr["message"]["content"] =
        json!([{"type": "text", "text": format!("{TEAMMATE_PREFIX}\nbody</teammate-message>")}]);
    assert_eq!(delivered_kind(&arr), Some("teammate_message"));
}

#[test]
fn delivered_messages_join_segment_and_roundtrip() {
    let dir = tmp_dir();
    let report = format!("{TEAMMATE_PREFIX}\nFINDING-X: the answer is 42.</teammate-message>");
    let records = vec![
        user("u1", None, "research this"),
        assistant("a1", "u1", "spawning researcher"),
        user("t1", Some("a1"), &report),
        assistant("a2", "t1", "relayed the finding"),
        user("u2", Some("a2"), "thanks"),
        assistant("a3", "u2", "done"),
        last_prompt("a3", "thanks"),
    ];
    let src = write_jsonl_file(&dir, "delegated.jsonl", &records);

    // Segmentation: the teammate report is activity, not a boundary — two segments, not three.
    let (kept, _) = select_active(load_jsonl(&src));
    let (_, segs) = segment(&kept);
    assert_eq!(segs.len(), 2);

    // Worksheet renders it as a delivered_message the summarizer can read.
    let ws = dir.join("ws.json");
    assert_eq!(
        cmd_extract(&[src.to_string_lossy().into_owned(), "--out".into(), ws.to_string_lossy().into_owned()]),
        0
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&ws).unwrap()).unwrap();
    let kinds: Vec<&str> = doc["segments"][0]["activity"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"delivered_message"));

    // Collapse the segment; the report is summarizable and the round trip verifies.
    let sums = dir.join("sums.json");
    fs::write(&sums, json!({"0": "Researcher reported FINDING-X: the answer is 42."}).to_string()).unwrap();
    let out = dir.join("out.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            sums.to_string_lossy().into_owned(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ]),
        0
    );
    assert_eq!(
        cmd_verify(&[
            out.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned(),
        ]),
        0
    );
    let text = fs::read_to_string(&out).unwrap();
    assert!(!text.contains("FINDING-X: the answer is 42.</teammate-message>"));
    assert!(text.contains("Researcher reported FINDING-X"));
}

// ---------------------------------------------------------------- splitting tests

fn tool_use_named(uuid: &str, parent: &str, tool_id: &str, name: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:02.000Z", "userType": "external", "isSidechain": false,
        "message": {
            "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-4-7",
            "type": "message", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": tool_id, "name": name, "input": {"description": "work"}}]
        }
    })
}

fn tool_result_sized(uuid: &str, parent: &str, tool_id: &str, size: usize) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:03.000Z", "userType": "external", "isSidechain": false,
        "sourceToolAssistantUUID": parent,
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": tool_id, "content": "x".repeat(size)}
        ]}
    })
}

/// One user turn spanning two subagent delegations plus follow-up work — the shape of a long
/// agentic session's giant segment.
fn delegated_giant_session() -> Vec<Value> {
    vec![
        user("u1", None, "do the big thing"),
        tool_use_named("a1", "u1", "task_1", "Task"),
        tool_result_sized("r1", "a1", "task_1", 4000),
        tool_use_named("a2", "r1", "task_2", "Task"),
        tool_result_sized("r2", "a2", "task_2", 4000),
        tool_use_named("a3", "r2", "bash_1", "Bash"),
        tool_result_sized("r3", "a3", "bash_1", 4000),
        assistant("a4", "r3", "all delegated work is done"),
        user("u2", Some("a4"), "great"),
        assistant("a5", "u2", "ok"),
        last_prompt("a5", "great"),
    ]
}

#[test]
fn split_respects_pairs_and_prefers_delegation_seams() {
    let records = delegated_giant_session();
    let (kept, _) = select_active(records);
    let (_, segs) = segment(&kept);

    // Under threshold: one part. Over: split, and never between a tool_use and its result.
    assert_eq!(split_parts(&kept, &segs[0], 0).len(), 1);
    assert_eq!(split_parts(&kept, &segs[0], 1_000_000).len(), 1);
    let parts = split_parts(&kept, &segs[0], 2000);
    assert!(parts.len() >= 2, "oversized segment must split, got {}", parts.len());
    for part in &parts {
        let mut pending = 0i64;
        for &i in part {
            let r = serde_json::to_string(&kept[i]).unwrap();
            pending += r.matches("\"type\":\"tool_use\"").count() as i64;
            pending -= r.matches("\"type\":\"tool_result\"").count() as i64;
        }
        assert_eq!(pending, 0, "a part must contain complete pairs only");
    }
    // The first delegation (Task) result closes the first part: a1+r1 end part 0.
    assert!(parts[0].len() == 2, "part 0 should be the first delegation unit, got {:?}", parts[0]);
}

#[test]
fn split_segment_roundtrips_with_part_keys() {
    let dir = tmp_dir();
    let src = write_jsonl_file(&dir, "giant.jsonl", &delegated_giant_session());
    let ws = dir.join("ws.json");
    assert_eq!(
        cmd_extract(&[
            src.to_string_lossy().into_owned(),
            "--out".into(),
            ws.to_string_lossy().into_owned(),
            "--split".into(),
            "2000".into(),
        ]),
        0
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&ws).unwrap()).unwrap();
    let needs: Vec<String> = doc["segments_needing_summary"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(needs.len() >= 2 && needs[0] == "0.0", "part keys expected, got {needs:?}");
    assert!(doc["segments"][0]["parts"].is_array());

    let sums: Value = needs
        .iter()
        .map(|k| (k.clone(), Value::String(format!("Part {k} summary."))))
        .collect::<serde_json::Map<String, Value>>()
        .into();
    let sums_path = dir.join("sums.json");
    fs::write(&sums_path, sums.to_string()).unwrap();
    let out = dir.join("out.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            sums_path.to_string_lossy().into_owned(),
            "--split".into(),
            "2000".into(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ]),
        0
    );
    assert_eq!(
        cmd_verify(&[
            out.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned(),
        ]),
        0
    );
    let assembled = load_jsonl(&out);
    let synths: Vec<&Value> = assembled
        .iter()
        .filter(|r| truthy(r, "recompactSynthetic"))
        .collect();
    assert_eq!(synths.len(), needs.len(), "one synthetic record per part");
    // Each part has its own provenance; part 0 covers the user turn, later parts do not.
    let covered0: Vec<&str> = synths[0]["recompactProvenance"]["coveredUuids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(covered0.contains(&"u1"));
    let covered1 = synths[1]["recompactProvenance"]["coveredUuids"].as_array().unwrap();
    assert!(!covered1.iter().any(|v| v.as_str() == Some("u1")));
    // Missing one part key fails cleanly.
    let mut partial = sums.as_object().unwrap().clone();
    partial.remove(&needs[0]);
    fs::write(&sums_path, Value::Object(partial).to_string()).unwrap();
    let out2 = dir.join("out2.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            sums_path.to_string_lossy().into_owned(),
            "--split".into(),
            "2000".into(),
            "--out".into(),
            out2.to_string_lossy().into_owned(),
        ]),
        1
    );
}

// ---------------------------------------------------------------- budget planner tests

fn build_plan_inputs(
    records: &[Value],
    keep: usize,
    split: usize,
) -> (Vec<Segment>, Vec<SegPlan>, Vec<Vec<Vec<usize>>>, Vec<Vec<String>>) {
    let (_, segs) = segment(records);
    let plans = plan(records, &segs, keep);
    let seg_parts: Vec<Vec<Vec<usize>>> = segs
        .iter()
        .map(|sg| split_parts(records, sg, split))
        .collect();
    let seg_keys: Vec<Vec<String>> = seg_parts
        .iter()
        .enumerate()
        .map(|(s, parts)| {
            let is_split = parts.len() > 1;
            (0..parts.len())
                .map(|p| if is_split { format!("{s}.{p}") } else { s.to_string() })
                .collect()
        })
        .collect();
    (segs, plans, seg_parts, seg_keys)
}

/// Boring bulky segment, then an error segment, then a correction turn, then the tail.
fn salience_session() -> Vec<Value> {
    vec![
        user("u1", None, "read the big file"),
        tool_use_named("a1", "u1", "read_1", "Read"),
        tool_result_sized("r1", "a1", "read_1", 6000),
        assistant("a2", "r1", "read it"),
        user("u2", Some("a2"), "run the build"),
        tool_use_named("a3", "u2", "bash_1", "Bash"),
        {
            let mut r = tool_result_sized("r2", "a3", "bash_1", 1000);
            r["message"]["content"][0]["is_error"] = json!(true);
            r
        },
        assistant("a4", "r2", "build failed with an error"),
        user("u3", Some("a4"), "actually try the other config"),
        assistant("a5", "u3", "trying"),
        user("u4", Some("a5"), "status?"),
        assistant("a6", "u4", "done"),
        last_prompt("a6", "status?"),
    ]
}

#[test]
fn planner_floors_hold_and_salience_orders_demotion() {
    let (kept, _) = select_active(salience_session());
    let (segs, plans, seg_parts, seg_keys) = build_plan_inputs(&kept, 1, 0);

    // Generous target: nothing demoted.
    let b = plan_budget(&kept, &segs, &plans, &seg_parts, &seg_keys, 1_000_000, true, &std::collections::HashMap::new(), |_| Some(100));
    assert!(b.units.iter().all(|u| u.treatment == Treatment::Verbatim));
    assert!(b.planned_total <= 1_000_000);

    // Impossible target: floors hold, planner terminates, reports over budget.
    let b = plan_budget(&kept, &segs, &plans, &seg_parts, &seg_keys, 1, true, &std::collections::HashMap::new(), |_| Some(100));
    assert!(b.planned_total > 1);
    let err_unit = b.units.iter().find(|u| u.floor == "error").expect("error unit exists");
    assert_ne!(err_unit.treatment, Treatment::Summarize, "error floor: never below mask");
    // The error segment is followed by a correction turn, so its salience stacks both signals.
    assert!(err_unit.salience > 0.7, "salience {}", err_unit.salience);
    // The boring bulky unit carries base salience only and gets demoted all the way.
    let boring = b.units.iter().find(|u| u.seg == 0).unwrap();
    assert_eq!(boring.treatment, Treatment::Summarize);
    assert!(boring.salience < 0.2);

    // Without summarize allowed (mask mode), no unit may be planned as Summarize.
    let b = plan_budget(&kept, &segs, &plans, &seg_parts, &seg_keys, 1, false, &std::collections::HashMap::new(), |_| None);
    assert!(b.units.iter().all(|u| u.treatment != Treatment::Summarize));
    assert!(b.need_summaries.is_empty());
}

#[test]
fn assemble_target_applies_plan_and_plan_flag_is_preview_only() {
    let dir = tmp_dir();
    let src = write_jsonl_file(&dir, "sal.jsonl", &salience_session());

    // --plan previews without writing anything.
    let out = dir.join("planned.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            "--mode".into(),
            "mask".into(),
            "--target".into(),
            "500".into(),
            "--plan".into(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ]),
        0
    );
    assert!(!out.exists(), "--plan must not write output");

    // Generous target keeps everything verbatim even in mask mode.
    let out_full = dir.join("full.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            "--mode".into(),
            "mask".into(),
            "--target".into(),
            "1000000".into(),
            "--out".into(),
            out_full.to_string_lossy().into_owned(),
        ]),
        0
    );
    let full_text = fs::read_to_string(&out_full).unwrap();
    assert!(!full_text.contains("recompactMasked"));
    assert!(full_text.contains(&"x".repeat(6000)), "under budget nothing is elided");

    // Tight target masks the boring bulk but keeps the error text verbatim.
    let out_tight = dir.join("tight.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            "--mode".into(),
            "mask".into(),
            "--target".into(),
            "500".into(),
            "--out".into(),
            out_tight.to_string_lossy().into_owned(),
        ]),
        0
    );
    let tight = fs::read_to_string(&out_tight).unwrap();
    assert!(!tight.contains(&"x".repeat(6000)), "bulk masked under tight budget");
    assert!(tight.contains("recompactMasked"));
    assert_eq!(
        cmd_verify(&[
            out_tight.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned(),
        ]),
        0
    );
}

#[test]
fn mask_elides_notifications_and_truncates_big_reports() {
    let dir = tmp_dir();
    let big_report = format!("{TEAMMATE_PREFIX}\n{}</teammate-message>", "r".repeat(9000));
    let records = vec![
        user("u1", None, "delegate work"),
        assistant("a1", "u1", "spawned"),
        user("t1", Some("a1"), &big_report),
        user("t2", Some("t1"), "<task-notification>\n<task-id>xyz</task-id>\n</task-notification>"),
        assistant("a2", "t2", "acknowledged"),
        user("u2", Some("a2"), "next"),
        assistant("a3", "u2", "ok"),
        last_prompt("a3", "next"),
    ];
    let src = write_jsonl_file(&dir, "masked-delegated.jsonl", &records);
    let out = dir.join("masked.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            "--mode".into(),
            "mask".into(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ]),
        0
    );
    assert_eq!(
        cmd_verify(&[
            out.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned(),
        ]),
        0
    );
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.contains("task notification elided"));
    assert!(!text.contains(&"r".repeat(9000)), "big report truncated");
    assert!(text.contains("chars elided"), "head+tail marker present");
}
