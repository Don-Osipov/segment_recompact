//! What a summarized or masked stretch must still leave behind, found with `recompact eval`: the
//! tool lookups that loaded deferred tools (else a resumed session must look them up again), the
//! thread or page the human linked (the task's own source), and the names a hidden result held.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use recompact::*;
use serde_json::{json, Value};

const S: &str = "0c000000-0000-4000-8000-000000000001";
const LINK: &str = "https://acme.slack.com/archives/C01/p1700000000";

fn rec(kind: &str, uuid: &str, parent: Option<&str>, content: Value) -> Value {
    let mut r = json!({
        "type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": S,
        "timestamp": "2026-11-10T10:00:00.000Z", "cwd": "/tmp/p", "userType": "external",
        "isSidechain": false, "message": {"role": kind, "content": content}
    });
    if kind == "assistant" {
        r["message"]["id"] = json!(format!("msg_{uuid}"));
        r["message"]["model"] = json!("claude-opus-5-5");
        r["message"]["type"] = json!("message");
    }
    if kind == "user" && content.is_array() {
        r["sourceToolAssistantUUID"] = json!(parent);
    }
    r
}

fn call(uuid: &str, parent: &str, id: &str, name: &str, input: Value) -> Value {
    rec("assistant", uuid, Some(parent), json!([{"type": "tool_use", "id": id, "name": name, "input": input}]))
}

fn result(uuid: &str, parent: &str, id: &str, content: Value) -> Value {
    rec("user", uuid, Some(parent), json!([{"type": "tool_result", "tool_use_id": id, "content": content}]))
}

fn u(n: u32) -> String {
    format!("0d000000-0000-4000-8000-{n:012}")
}

fn session() -> Vec<Value> {
    let thread = format!("THREAD-START {} THREAD-END", "the export drops rows after 02:00. ".repeat(300));
    let listing = (0..120).map(|i| format!("src/export/part{i}.rs\n")).collect::<String>();
    vec![
        rec("user", &u(1), None, json!(format!("look at {LINK} and find the bug"))),
        call(&u(2), &u(1), "ts1", "ToolSearch", json!({"query": "select:mcp__slack__slack_read_thread"})),
        result(&u(3), &u(2), "ts1", json!([{"type": "tool_reference", "tool_name": "mcp__slack__slack_read_thread"}])),
        call(&u(4), &u(3), "sr1", "Bash", json!({"command": format!("slack-read {LINK}")})),
        result(&u(5), &u(4), "sr1", json!(thread)),
        call(&u(6), &u(5), "ls1", "Bash", json!({"command": "ls src/export"})),
        result(&u(7), &u(6), "ls1", json!(listing)),
        rec("assistant", &u(8), Some(&u(7)), json!([{"type": "text", "text": "Found it: the cutoff is local time."}])),
        rec("user", &u(9), Some(&u(8)), json!("fix it")),
        rec("assistant", &u(10), Some(&u(9)), json!([{"type": "text", "text": "Fixed the cutoff."}])),
        rec("user", &u(11), Some(&u(10)), json!("thanks")),
        rec("assistant", &u(12), Some(&u(11)), json!([{"type": "text", "text": "You're welcome."}])),
    ]
}

fn root() -> PathBuf {
    let d = std::env::temp_dir().join(format!("recompact-phase28-{}", uuid_v4()));
    fs::create_dir_all(&d).unwrap();
    d
}

fn write(path: &Path, records: &[Value]) {
    let mut f = fs::File::create(path).unwrap();
    for r in records {
        writeln!(f, "{r}").unwrap();
    }
}

fn assemble(root: &Path, extra: &[&str]) -> Vec<Value> {
    let orig = root.join(format!("{S}.jsonl"));
    write(&orig, &session());
    let sums = root.join("sums.json");
    fs::write(&sums, json!({"0": "I read the thread and found the cutoff bug.", "1": "Fixed it."}).to_string()).unwrap();
    let out = root.join("twin.jsonl");
    let mut args = vec![orig.to_string_lossy().into_owned()];
    if !extra.contains(&"mask") {
        args.push(sums.to_string_lossy().into_owned());
    } else {
        args.extend(["--mode".to_string(), "mask".to_string()]);
    }
    args.extend(["--out".to_string(), out.to_string_lossy().into_owned()]);
    assert_eq!(cmd_assemble(&args), 0);
    assert_eq!(
        cmd_verify(&[out.to_string_lossy().into_owned(), "--source".into(), orig.to_string_lossy().into_owned()]),
        0,
        "the twin verifies"
    );
    fs::read_to_string(&out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn by_uuid<'a>(twin: &'a [Value], uuid: &str) -> Option<&'a Value> {
    twin.iter().find(|r| r["uuid"] == json!(uuid))
}

#[test]
fn a_summarized_stretch_keeps_its_tool_lookups_and_the_linked_thread() {
    let twin = assemble(&root(), &[]);
    // The lookup that loaded the Slack tool, call and result, word for word.
    assert!(by_uuid(&twin, &u(2)).is_some(), "the tool lookup call stays");
    let loaded = by_uuid(&twin, &u(3)).expect("the tool lookup result stays");
    assert_eq!(loaded["message"]["content"][0]["content"][0]["type"], json!("tool_reference"));
    // The thread the human linked: kept, cut to its head and tail.
    assert!(by_uuid(&twin, &u(4)).is_some(), "the fetch of the linked thread stays");
    let thread = by_uuid(&twin, &u(5)).expect("the linked thread stays");
    let text = thread["message"]["content"][0]["content"].as_str().unwrap();
    assert!(text.starts_with("THREAD-START") && text.ends_with("THREAD-END"), "head and tail kept");
    assert!(text.chars().count() <= SOURCE_MAX_CHARS + 200, "capped: {}", text.chars().count());
    assert_eq!(thread["recompactMasked"], json!(true));
    // The rest of the stretch is the summary's.
    assert!(by_uuid(&twin, &u(7)).is_none(), "the listing is summarized away");
}

#[test]
fn a_masked_stretch_keeps_the_linked_thread_and_names_what_it_hid() {
    let twin = assemble(&root(), &["mask"]);
    let thread = by_uuid(&twin, &u(5)).unwrap();
    assert!(
        thread["message"]["content"][0]["content"].as_str().unwrap().starts_with("THREAD-START"),
        "the linked thread is not hidden behind a marker"
    );
    let listing = by_uuid(&twin, &u(7)).unwrap()["message"]["content"][0]["content"].as_str().unwrap().to_string();
    assert!(listing.starts_with("[recompact: elided"), "{listing}");
    assert!(listing.contains("; names src/export/part"), "the marker lists the names: {listing}");
}
