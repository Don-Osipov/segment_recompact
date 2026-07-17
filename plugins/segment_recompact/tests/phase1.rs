//! Phase-1 integration tests: mechanical worksheet lane, deterministic index, provenance +
//! rehydrate, synthetic-summary pinning, and probe.

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

fn tool_use_named(uuid: &str, parent: &str, tool_id: &str, name: &str, input: Value) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:02.000Z", "userType": "external", "isSidechain": false,
        "message": {
            "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-4-7",
            "type": "message", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": tool_id, "name": name, "input": input}]
        }
    })
}

fn tool_result_full(uuid: &str, parent: &str, tool_id: &str, text: &str, is_error: bool) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-07-17T00:00:03.000Z", "userType": "external", "isSidechain": false,
        "sourceToolAssistantUUID": parent,
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": tool_id, "content": text, "is_error": is_error}
        ]}
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

/// A two-segment session whose first segment reads a file, edits it, runs a command that fails,
/// hits an empty grep, and repeats an identical read.
fn rich_session() -> Vec<Value> {
    vec![
        user("u1", None, "fix the build"),
        tool_use_named("a1", "u1", "t_read", "Read", json!({"file_path": "/src/app.rs"})),
        tool_result_full("r1", "a1", "t_read", "fn main() {}", false),
        tool_use_named("a2", "r1", "t_grep", "Grep", json!({"pattern": "nonexistent"})),
        tool_result_full("r2", "a2", "t_grep", "   ", false),
        tool_use_named("a3", "r2", "t_bash", "Bash", json!({"command": "cargo build"})),
        tool_result_full("r3", "a3", "t_bash", "error[E0308]: mismatched types", true),
        tool_use_named("a4", "r3", "t_edit", "Edit", json!({"file_path": "/src/app.rs", "old_string": "x", "new_string": "y"})),
        tool_result_full("r4", "a4", "t_edit", "ok", false),
        tool_use_named("a5", "r4", "t_read2", "Read", json!({"file_path": "/src/app.rs"})),
        tool_result_full("r5", "a5", "t_read2", "fn main() {}", false),
        assistant("a6", "r5", "fixed"),
        user("u2", Some("a6"), "thanks"),
        assistant("a7", "u2", "done"),
        last_prompt("a7", "thanks"),
    ]
}

// ---------------------------------------------------------------------------------------- tests

#[test]
fn head_tail_truncation_keeps_the_tail() {
    let s = format!("HEAD{}TAIL", "x".repeat(5000));
    let t = truncate_head_tail(&s, 100, 0.6);
    assert!(t.starts_with("HEAD"));
    assert!(t.ends_with("TAIL"));
    assert!(t.contains("chars elided"));
    assert!(truncate_head_tail("short", 100, 0.6) == "short");
}

#[test]
fn segment_index_derives_files_commands_errors() {
    let records = rich_session();
    let (kept, _) = select_active(records);
    let (_, segs) = segment(&kept);
    let idx = segment_index(&kept, &segs[0]);
    assert_eq!(idx["files"]["/src/app.rs"], json!(["read", "edited"]));
    assert_eq!(idx["commands"], json!(["cargo build"]));
    assert_eq!(idx["error_count"], 1);
    assert_eq!(idx["tool_counts"]["Read"], 2);
}

#[test]
fn worksheet_labels_tools_and_grades_results() {
    let dir = tmp_dir();
    let src = write_jsonl_file(&dir, "rich.jsonl", &rich_session());
    let ws = dir.join("segments.json");
    assert_eq!(
        cmd_extract(&[
            src.to_string_lossy().into_owned(),
            "--out".into(),
            ws.to_string_lossy().into_owned(),
        ]),
        0
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&ws).unwrap()).unwrap();
    let acts = doc["segments"][0]["activity"].as_array().unwrap();
    let results: Vec<&Value> = acts.iter().filter(|a| a["kind"] == "tool_result").collect();
    let status_of = |tool: &str| -> Vec<&str> {
        results
            .iter()
            .filter(|r| r["tool"] == tool)
            .map(|r| r["status"].as_str().unwrap())
            .collect()
    };
    assert_eq!(status_of("Grep"), vec!["empty"]);
    assert_eq!(status_of("Bash"), vec!["error"]);
    assert_eq!(status_of("Read"), vec!["ok", "duplicate"]);
    assert!(results.iter().all(|r| r["chars"].is_number()));
    assert!(doc["segments"][0]["derived_index"]["files"]["/src/app.rs"].is_array());
}

#[test]
fn assemble_embeds_provenance_and_index_and_rehydrate_recovers() {
    let dir = tmp_dir();
    let src = write_jsonl_file(&dir, "rich.jsonl", &rich_session());
    let sums = dir.join("summaries.json");
    fs::write(&sums, json!({"0": "I fixed the build."}).to_string()).unwrap();
    let out = dir.join("compacted.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            sums.to_string_lossy().into_owned(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ]),
        0
    );

    let assembled = load_jsonl(&out);
    let synth = assembled
        .iter()
        .find(|r| truthy(r, "recompactSynthetic"))
        .expect("synthetic summary present");
    let prov = &synth["recompactProvenance"];
    assert_eq!(prov["sourceSessionId"], SESSION);
    let covered: Vec<&str> = prov["coveredUuids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(covered.contains(&"u1") && covered.contains(&"a1") && covered.contains(&"r3"));
    assert_eq!(synth["recompactIndex"]["error_count"], 1);

    // rehydrate list mode and dump mode both succeed against the intact source.
    assert_eq!(cmd_rehydrate(&[out.to_string_lossy().into_owned()]), 0);
    assert_eq!(
        cmd_rehydrate(&[out.to_string_lossy().into_owned(), "0".into()]),
        0
    );
    // dump for a summary that does not exist fails cleanly.
    assert_eq!(
        cmd_rehydrate(&[out.to_string_lossy().into_owned(), "5".into()]),
        1
    );
}

#[test]
fn synthetic_summaries_are_never_resummarized() {
    // A previously-compacted session fed back through the tool: the segment holding the
    // recompactSynthetic record must be pinned verbatim, not offered for summarization.
    let mut synth = assistant("syn1", "u1", "Earlier summary text.");
    synth["recompactSynthetic"] = json!(true);
    let records = vec![
        user("u1", None, "original ask"),
        synth,
        user("u2", Some("syn1"), "follow-up"),
        assistant("a2", "u2", "answer"),
        last_prompt("a2", "follow-up"),
    ];
    let (kept, _) = select_active(records);
    let (_, segs) = segment(&kept);
    let plans = plan(&kept, &segs, 1);
    assert!(plans[0].kept_verbatim);
    assert!(!plans[0].needs_summary);
}

#[test]
fn mask_mode_elides_bulk_keeps_errors_and_structure() {
    let dir = tmp_dir();
    let big = "x".repeat(5000);
    let mut sig_carrier = assistant("a0", "u1", "");
    sig_carrier["message"]["content"] =
        json!([{"type": "thinking", "thinking": "", "signature": "SIGBLOB".repeat(500)}]);
    let mut heavy_result = tool_result_full("r1", "a1", "t_read", &big, false);
    heavy_result["toolUseResult"] = json!({"file": {"content": "z".repeat(8000)}});
    let records = vec![
        user("u1", None, "investigate"),
        sig_carrier,
        tool_use_named("a1", "a0", "t_read", "Read", json!({"file_path": "/src/big.rs"})),
        heavy_result,
        tool_use_named("a2", "r1", "t_bash", "Bash", json!({"command": "cargo build"})),
        tool_result_full("r2", "a2", "t_bash", "error[E0308]: mismatched types", true),
        tool_use_named(
            "a3",
            "r2",
            "t_write",
            "Write",
            json!({"file_path": "/src/gen.rs", "content": "y".repeat(6000)}),
        ),
        tool_result_full("r3", "a3", "t_write", "ok", false),
        assistant("a4", "r3", "I looked at the big file and rebuilt."),
        user("u2", Some("a4"), "and then?"),
        assistant("a5", "u2", "done"),
        last_prompt("a5", "and then?"),
    ];
    let src = write_jsonl_file(&dir, "maskable.jsonl", &records);
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
    // Structure survives the strictest checks, including verbatim user turns.
    assert_eq!(
        cmd_verify(&[
            out.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned(),
        ]),
        0
    );
    let text = fs::read_to_string(&out).unwrap();
    assert!(!text.contains(&big), "bulk result must be elided");
    assert!(text.contains("elided 5000-char result"));
    assert!(text.contains("error[E0308]"), "error text stays verbatim");
    assert!(text.contains("\\\"ok\\\"") || text.contains("ok"), "short results stay verbatim");
    assert!(!text.contains(&"y".repeat(6000)), "oversized tool_use input truncated");
    assert!(text.contains("recompactMasked"));
    assert!(
        text.contains("I looked at the big file"),
        "assistant prose is never touched by masking"
    );
    // No LLM summary record in mask mode.
    assert!(!text.contains("recompactSynthetic"));
    // Empty-thinking signature carriers are dropped whole.
    assert!(!text.contains("SIGBLOB"), "empty-thinking signature blob must be gone");
    // The top-level toolUseResult duplicate is elided.
    assert!(!text.contains(&"z".repeat(8000)), "bulky toolUseResult must be elided");
    assert!(text.contains("recompactElided"));
}

#[test]
fn mask_mode_rejects_summaries_file_and_bad_mode() {
    let dir = tmp_dir();
    let src = write_jsonl_file(&dir, "s.jsonl", &rich_session());
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            "sums.json".into(),
            "--mode".into(),
            "mask".into(),
        ]),
        2
    );
    assert_eq!(
        cmd_assemble(&[src.to_string_lossy().into_owned(), "--mode".into(), "bogus".into()]),
        2
    );
}

#[test]
fn probe_passes_on_wellformed_and_fails_on_userless() {
    let dir = tmp_dir();
    let good = write_jsonl_file(&dir, "good.jsonl", &rich_session());
    assert_eq!(cmd_probe(&[good.to_string_lossy().into_owned()]), 0);

    let userless = write_jsonl_file(
        &dir,
        "userless.jsonl",
        &[
            assistant("a1", "nowhere", "hello"),
            last_prompt("a1", "hello"),
        ],
    );
    assert_eq!(cmd_probe(&[userless.to_string_lossy().into_owned()]), 1);
}
