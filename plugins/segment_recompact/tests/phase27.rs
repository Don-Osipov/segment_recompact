//! `recompact eval`: cut points come from long original sessions, a twin built at a cut is scored
//! on the details the real continuation used again, and the scratch session files are removed.
//! Only the model-free tier runs here.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

const SESSION: &str = "e0e0e0e0-1111-4222-8333-444444444444";

fn rec(kind: &str, uuid: &str, parent: Option<&str>, message: Value) -> Value {
    let mut r = json!({
        "type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-11-09T00:00:00.000Z", "cwd": "/tmp/eval-proj", "userType": "external",
        "isSidechain": false, "message": message
    });
    if kind == "user" && message["content"].is_array() {
        r["sourceToolAssistantUUID"] = json!(parent);
    }
    r
}

fn human(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    rec("user", uuid, parent, json!({"role": "user", "content": text}))
}

fn call(uuid: &str, parent: &str, id: &str, name: &str, input: Value, live: u64) -> Value {
    rec("assistant", uuid, Some(parent), json!({
        "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5", "type": "message",
        "stop_reason": "tool_use", "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
        "usage": {"input_tokens": 2, "cache_read_input_tokens": live, "cache_creation_input_tokens": 0, "output_tokens": 50}
    }))
}

fn reply(uuid: &str, parent: &str, text: &str, live: u64) -> Value {
    rec("assistant", uuid, Some(parent), json!({
        "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5", "type": "message",
        "stop_reason": "end_turn", "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 2, "cache_read_input_tokens": live, "cache_creation_input_tokens": 0, "output_tokens": 50}
    }))
}

fn result(uuid: &str, parent: &str, id: &str, text: &str) -> Value {
    rec("user", uuid, Some(parent), json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": id, "content": text, "is_error": false}
    ]}))
}

/// Two turns before the cut (a big read, then an edit and a passing test), and a third turn the
/// cut starts: the user asks to push the fix, and the agent runs `git push`.
fn session() -> Vec<Value> {
    let big = "fn export_rows(conn: &Conn) -> Rows { query(\"export_rows_v2\") }\n".repeat(16_000);
    vec![
        human("u1", None, "find why the nightly export in src/export.rs drops rows"),
        call("a1", "u1", "t1", "Read", json!({"file_path": "/tmp/eval-proj/src/export.rs"}), 150_000),
        result("r1", "a1", "t1", &big),
        reply("a2", "r1", "The query reads table export_rows_v2, which the migration renamed.", 190_000),
        human("u2", Some("a2"), "fix it and run the tests"),
        call("a3", "u2", "t3", "Edit", json!({"file_path": "/tmp/eval-proj/src/export.rs", "old_string": "export_rows_v2", "new_string": "export_rows"}), 215_000),
        result("r3", "a3", "t3", "The file was updated."),
        call("a4", "r3", "t4", "Bash", json!({"command": "cargo test -q"}), 220_000),
        result("r4", "a4", "t4", "test result: ok. 12 passed"),
        reply("a5", "r4", "Fixed: src/export.rs now reads export_rows; the tests pass.", 225_000),
        human("u3", Some("a5"), "push the src/export.rs fix"),
        call("a6", "u3", "t6", "Bash", json!({"command": "git push origin fix-export"}), 226_000),
        result("r6", "a6", "t6", "pushed"),
        reply("a7", "r6", "Pushed fix-export.", 227_000),
    ]
}

fn write_jsonl(path: &Path, records: &[Value]) {
    let body: String = records.iter().map(|r| format!("{r}\n")).collect();
    fs::write(path, body).unwrap();
}

fn recompact(home: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_recompact"))
        .args(args)
        .env("HOME", home)
        .env("RECOMPACT_HOME", home.join(".claude/recompact"))
        .env_remove("RECOMPACT_SHELL")
        .env_remove("CLAUDE_JOB_DIR")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn scratch_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("recompact-phase27-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(home.join(".claude/projects/-tmp-eval-proj")).unwrap();
    home
}

#[test]
fn a_twin_at_a_past_cut_is_scored_on_the_details_the_real_continuation_used() {
    let home = scratch_home("run");
    let project = home.join(".claude/projects/-tmp-eval-proj");
    write_jsonl(&project.join(format!("{SESSION}.jsonl")), &session());

    let (rc, out, err) = recompact(&home, &["eval", "points", project.to_str().unwrap(), "--min", "200k"]);
    assert_eq!(rc, 0, "{out}{err}");
    let points: Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/recompact/eval/points.json")).unwrap()).unwrap();
    let points = points["points"].as_array().unwrap();
    assert_eq!(points.len(), 1, "one cut: the first human turn past 200k live tokens");
    assert_eq!(points[0]["cut"], json!("u3"));
    // The point keeps its own copy, so later growth, compaction or cleanup of the session cannot
    // change it.
    let frozen = Path::new(points[0]["source"].as_str().unwrap()).to_path_buf();
    assert!(frozen.exists());
    let mut grown = session();
    grown.push(human("u4", Some("a7"), "and open the PR"));
    write_jsonl(&project.join(format!("{SESSION}.jsonl")), &grown[..3]);

    // The same points must serve every arm.
    let (rc, _, err) = recompact(&home, &["eval", "points", project.to_str().unwrap(), "--min", "200k"]);
    assert_eq!(rc, 1);
    assert!(err.contains("--force"), "{err}");

    let (rc, out, err) = recompact(&home, &["eval", "run", "--arm", "masked", "--mask", "--tier0", "--target", "20k"]);
    assert_eq!(rc, 0, "{out}{err}");
    let row: Value = serde_json::from_str(
        fs::read_to_string(home.join(".claude/recompact/eval/results/masked.jsonl")).unwrap().lines().last().unwrap(),
    )
    .unwrap();
    assert_eq!(row["ok"], json!(true), "{row}");
    assert!(row["context_tokens"].as_u64().unwrap() < row["prefix_tokens"].as_u64().unwrap(), "{row}");
    let d = &row["details"];
    assert!(d["total"].as_u64().unwrap() >= 1, "the next turn names src/export.rs: {row}");
    assert_eq!(d["kept"], d["total"], "the edit that names it is in the verbatim tail: {row}");

    // Nothing of the scratch project is left behind.
    let leftovers: Vec<String> = fs::read_dir(home.join(".claude/projects"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("recompact-eval-work"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    let (rc, out, _) = recompact(&home, &["eval", "report"]);
    assert_eq!(rc, 0);
    assert!(out.contains("masked"), "{out}");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_flag_does_not_swallow_the_option_after_it() {
    let home = scratch_home("flags");
    let project = home.join(".claude/projects/-tmp-eval-proj");
    write_jsonl(&project.join(format!("{SESSION}.jsonl")), &session());
    let (rc, out, err) = recompact(&home, &["eval", "points", project.to_str().unwrap(), "--min", "200k"]);
    assert_eq!(rc, 0, "{out}{err}");
    // `--tier0` before `--only`: the filter must still apply, so nothing runs.
    let (rc, _, err) = recompact(&home, &["eval", "run", "--arm", "x", "--mask", "--tier0", "--only", "nope"]);
    assert_eq!(rc, 0, "{err}");
    assert!(err.contains("0 point(s) to run"), "{err}");
    let _ = fs::remove_dir_all(&home);
}
