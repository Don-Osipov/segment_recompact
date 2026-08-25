//! Recall over MCP: the compaction markers become callable instead of merely readable.
//!
//! The serve loop is driven in-process over Cursor/Vec<u8>, like every other test here — no
//! spawned binary, no pipes to deadlock on, and no process-global state for cargo's threads to
//! race over.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::{fs, io::Write};

use recompact::*;
use serde_json::{json, Value};

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": "s",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]}
    })
}

fn assistant(uuid: &str, parent: &str, text: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": "s",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
    })
}

fn assistant_image(uuid: &str, parent: &str, data: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": "s",
        "message": {"role": "assistant", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}}
        ]}
    })
}

fn last_prompt(leaf: &str, text: &str) -> Value {
    json!({"type": "last-prompt", "leafUuid": leaf, "prompt": text})
}

fn write_session(dir: &Path, name: &str, records: &[Value]) -> PathBuf {
    let p = dir.join(format!("{name}.jsonl"));
    let mut f = fs::File::create(&p).unwrap();
    for r in records {
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
    p
}

/// Drive the serve loop over a batch of requests and return one parsed response per reply.
fn serve(dir: &Path, requests: &[Value]) -> Vec<Value> {
    let input: String = requests
        .iter()
        .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
        .collect();
    let mut out: Vec<u8> = Vec::new();
    let rc = mcp_serve(Cursor::new(input.into_bytes()), &mut out, dir);
    assert_eq!(rc, 0, "serve loop should exit clean on EOF");
    String::from_utf8(out)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn call(dir: &Path, args: Value) -> Value {
    let resp = serve(
        dir,
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                 "params":{"name":"recall","arguments":args}})],
    );
    assert_eq!(resp.len(), 1);
    resp[0]["result"].clone()
}

fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["type"] == "text")
        .map(|c| c["text"].as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn initialize_echoes_the_client_protocol_version_and_advertises_recall() {
    let dir = tmp_dir();
    let resp = serve(
        &dir,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                   "params":{"protocolVersion":"2099-01-01","capabilities":{},
                             "clientInfo":{"name":"t","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        ],
    );
    // The notification gets no reply: two requests in, two responses out.
    assert_eq!(resp.len(), 2, "notifications must not be answered");

    // Echoed verbatim, so there is no protocol date to keep in sync.
    assert_eq!(resp[0]["result"]["protocolVersion"], "2099-01-01");
    assert_eq!(resp[0]["result"]["capabilities"]["tools"], json!({}));
    assert!(resp[0]["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("recall"));

    let tools = resp[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "recall");
    let props = &tools[0]["inputSchema"]["properties"];
    for k in ["selector", "chunk", "session"] {
        assert!(props.get(k).is_some(), "schema should document {k}");
    }
}

#[test]
fn unknown_method_and_unknown_tool_report_distinct_errors() {
    let dir = tmp_dir();
    let resp = serve(
        &dir,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"resources/list"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nope"}}),
        ],
    );
    assert_eq!(resp[0]["error"]["code"], -32601);
    assert_eq!(resp[1]["error"]["code"], -32602);
}

#[test]
fn a_uuid_prefix_resolves_from_a_different_session_than_the_one_queried() {
    let dir = tmp_dir();
    // Ground truth lives in one session...
    let secret = "the-load-bearing-detail-42";
    let origin = write_session(
        &dir,
        "origin",
        &[
            user("11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa", None, "go"),
            assistant(
                "22222222-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                secret,
            ),
            last_prompt("22222222-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "go"),
        ],
    );
    // ...and an unrelated session is the newest file in the dir, so "newest wins" would miss.
    write_session(
        &dir,
        "zz-unrelated",
        &[
            user("99999999-cccc-4ccc-8ccc-cccccccccccc", None, "other work"),
            last_prompt("99999999-cccc-4ccc-8ccc-cccccccccccc", "other work"),
        ],
    );
    assert!(origin.exists());

    // The selector is what a marker prints: the first 8 chars of the record uuid. No session arg.
    let result = call(&dir, json!({"selector": "22222222"}));
    assert_eq!(result["isError"], false);
    let text = text_of(&result);
    assert!(text.contains(secret), "should recover ground truth, got: {text}");
    assert!(text.contains("origin"), "should name the session it resolved");
}

#[test]
fn an_oversized_payload_chunks_and_the_chunks_reassemble_to_the_original() {
    let dir = tmp_dir();
    // Two full chunks plus a remainder, so boundaries are actually exercised.
    let big: String = std::iter::repeat_n('x', 8000 * 2 + 500).collect();
    write_session(
        &dir,
        "big",
        &[
            user("aaaaaaa1-aaaa-4aaa-8aaa-aaaaaaaaaaaa", None, "go"),
            assistant(
                "bbbbbbb2-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "aaaaaaa1-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                &big,
            ),
            last_prompt("bbbbbbb2-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "go"),
        ],
    );

    let first = call(&dir, json!({"selector": "bbbbbbb2"}));
    let head = text_of(&first);
    assert!(head.contains("chunk 1/3"), "should announce 3 chunks: {}", &head[..120.min(head.len())]);

    // Strip the header line and reassemble.
    let body = |t: &str| t.splitn(2, "\n\n").nth(1).unwrap_or("").to_string();
    let mut all = body(&head);
    for n in 2..=3 {
        all.push_str(&body(&text_of(&call(&dir, json!({"selector":"bbbbbbb2","chunk":n})))));
    }
    assert_eq!(all.chars().count(), big.chars().count());
    assert_eq!(all, big, "chunks must concatenate to the original");
}

#[test]
fn an_image_comes_back_as_an_image_block_not_as_base64_text() {
    let dir = tmp_dir();
    // 1x1 transparent PNG.
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    write_session(
        &dir,
        "shot",
        &[
            user("ccccccc1-cccc-4ccc-8ccc-cccccccccccc", None, "screenshot"),
            assistant_image(
                "ddddddd2-dddd-4ddd-8ddd-dddddddddddd",
                "ccccccc1-cccc-4ccc-8ccc-cccccccccccc",
                png,
            ),
            last_prompt("ddddddd2-dddd-4ddd-8ddd-dddddddddddd", "screenshot"),
        ],
    );

    let result = call(&dir, json!({"selector": "ddddddd2"}));
    let blocks = result["content"].as_array().unwrap();
    let image = blocks
        .iter()
        .find(|b| b["type"] == "image")
        .expect("an elided image must return as a real image block");
    assert_eq!(image["data"], png);
    assert_eq!(image["mimeType"], "image/png");
    // The whole point of decision (1): the base64 must not also arrive as prose.
    assert!(
        !text_of(&result).contains(png),
        "base64 must not be delivered as text — that is what image elision exists to prevent"
    );
}

#[test]
fn an_unresolvable_selector_reports_an_error_result_rather_than_a_protocol_error() {
    let dir = tmp_dir();
    write_session(
        &dir,
        "s1",
        &[
            user("eeeeeee1-eeee-4eee-8eee-eeeeeeeeeeee", None, "go"),
            last_prompt("eeeeeee1-eeee-4eee-8eee-eeeeeeeeeeee", "go"),
        ],
    );
    let result = call(&dir, json!({"selector": "deadbeef"}));
    assert_eq!(result["isError"], true, "a bad selector is a tool error, not a JSON-RPC error");
    assert!(text_of(&result).contains("deadbeef"));
}

#[test]
fn no_selector_lists_what_is_recallable() {
    let dir = tmp_dir();
    write_session(
        &dir,
        "plain",
        &[
            user("fffffff1-ffff-4fff-8fff-ffffffffffff", None, "go"),
            last_prompt("fffffff1-ffff-4fff-8fff-ffffffffffff", "go"),
        ],
    );
    let result = call(&dir, json!({}));
    assert_eq!(result["isError"], false);
    // No compaction has happened, so the honest answer names that and points at uuid prefixes.
    assert!(text_of(&result).contains("no compaction summaries"));
}

#[test]
fn a_malformed_transcript_in_the_project_dir_does_not_take_the_server_down() {
    let dir = tmp_dir();
    fs::write(dir.join("corrupt.jsonl"), "{not json at all\n").unwrap();
    write_session(
        &dir,
        "good",
        &[
            user("abcdef01-1111-4111-8111-111111111111", None, "go"),
            assistant(
                "abcdef02-2222-4222-8222-222222222222",
                "abcdef01-1111-4111-8111-111111111111",
                "still reachable",
            ),
            last_prompt("abcdef02-2222-4222-8222-222222222222", "go"),
        ],
    );
    let result = call(&dir, json!({"selector": "abcdef02"}));
    assert_eq!(result["isError"], false);
    assert!(text_of(&result).contains("still reachable"));
}

// --- Defects found by driving the real server, each pinned so it stays fixed -----------------

fn big_session(dir: &Path, name: &str, uuid_a: &str, uuid_b: &str, text: &str) {
    write_session(
        dir,
        name,
        &[
            user(uuid_a, None, "go"),
            assistant(uuid_b, uuid_a, text),
            last_prompt(uuid_b, "go"),
        ],
    );
}

#[test]
fn the_final_chunk_does_not_invite_another_call() {
    let dir = tmp_dir();
    let big: String = std::iter::repeat_n('x', 8000 * 2 + 500).collect();
    big_session(
        &dir,
        "s",
        "aaaaaaa1-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "bbbbbbb2-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        &big,
    );
    let last = text_of(&call(&dir, json!({"selector": "bbbbbbb2", "chunk": 3})));
    assert!(last.contains("chunk 3/3"), "got: {}", &last[..80.min(last.len())]);
    // Advertising "chunk=3" again on the last chunk is an invitation to loop forever.
    assert!(
        !last.contains("chunk=3 for the next"),
        "the final chunk must not point back at itself: {}",
        &last[..120.min(last.len())]
    );
    assert!(last.contains("final chunk"));
}

#[test]
fn a_chunk_past_the_end_is_an_error_not_a_silent_clamp() {
    let dir = tmp_dir();
    let big: String = std::iter::repeat_n('x', 8000 * 2 + 500).collect();
    big_session(
        &dir,
        "s",
        "aaaaaaa1-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "bbbbbbb2-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        &big,
    );
    let result = call(&dir, json!({"selector": "bbbbbbb2", "chunk": 99}));
    assert_eq!(
        result["isError"], true,
        "serving chunk 3 under the label 99 would tell the model it had read past the end"
    );
    assert!(text_of(&result).contains("3 chunk"));
}

#[test]
fn an_unknown_session_says_what_to_do_instead_of_leaking_an_os_error() {
    let dir = tmp_dir();
    write_session(
        &dir,
        "real",
        &[
            user("aaaaaaa1-aaaa-4aaa-8aaa-aaaaaaaaaaaa", None, "go"),
            last_prompt("aaaaaaa1-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "go"),
        ],
    );
    let text = text_of(&call(&dir, json!({"selector": "0", "session": "no-such-session"})));
    assert!(!text.contains("os error"), "raw errno is not actionable: {text}");
    assert!(text.contains("no session"));
    assert!(text.contains("Uuid prefixes need no session"));
}

#[test]
fn a_named_session_is_a_hint_and_does_not_block_project_wide_resolution() {
    let dir = tmp_dir();
    big_session(
        &dir,
        "holder",
        "aaaaaaa1-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "ccccccc9-cccc-4ccc-8ccc-cccccccccccc",
        "the detail",
    );
    write_session(
        &dir,
        "elsewhere",
        &[
            user("ddddddd1-dddd-4ddd-8ddd-dddddddddddd", None, "unrelated"),
            last_prompt("ddddddd1-dddd-4ddd-8ddd-dddddddddddd", "unrelated"),
        ],
    );
    // Session names a file that exists but does not hold the uuid: it should still resolve.
    let result = call(&dir, json!({"selector": "ccccccc9", "session": "elsewhere"}));
    assert_eq!(result["isError"], false, "got: {}", text_of(&result));
    assert!(text_of(&result).contains("the detail"));
}

#[test]
fn an_ambiguous_prefix_asks_for_more_characters_rather_than_guessing() {
    let dir = tmp_dir();
    // Two records sharing an 8-char prefix — the collision project-wide search makes possible.
    write_session(
        &dir,
        "a",
        &[
            user("beefbeef-1111-4111-8111-111111111111", None, "one"),
            last_prompt("beefbeef-1111-4111-8111-111111111111", "one"),
        ],
    );
    write_session(
        &dir,
        "b",
        &[
            user("beefbeef-2222-4222-8222-222222222222", None, "two"),
            last_prompt("beefbeef-2222-4222-8222-222222222222", "two"),
        ],
    );
    let result = call(&dir, json!({"selector": "beefbeef"}));
    assert_eq!(result["isError"], true, "guessing between two records is worse than refusing");
    let t = text_of(&result);
    assert!(t.contains("ambiguous"), "got: {t}");
    assert!(t.contains("more characters"));
}

#[test]
fn a_multi_record_expansion_too_big_to_serve_returns_an_index_not_a_wall_of_text() {
    let dir = tmp_dir();
    let chunk_sized: String = std::iter::repeat_n('y', 6000).collect();
    // A summary covering two fat records: concatenated they span chunks, so recall should index.
    let a = "eeeeeee1-eeee-4eee-8eee-eeeeeeeeeeee";
    let b = "eeeeeee2-eeee-4eee-8eee-eeeeeeeeeeee";
    let c = "eeeeeee3-eeee-4eee-8eee-eeeeeeeeeeee";
    let origin = dir.join("origin.jsonl");
    write_session(
        &dir,
        "origin",
        &[
            user(a, None, "go"),
            assistant(b, a, &chunk_sized),
            assistant(c, b, &chunk_sized),
            last_prompt(c, "go"),
        ],
    );
    let summary = json!({
        "type": "assistant", "uuid": "fffffff1-ffff-4fff-8fff-ffffffffffff",
        "parentUuid": a, "sessionId": "t", "recompactSynthetic": true,
        "message": {"role": "assistant", "content": [{"type": "text", "text": "did some work"}]},
        "recompactProvenance": {
            "source": origin.to_string_lossy(), "sourceSessionId": "origin",
            "part": "0", "coveredUuids": [b, c]
        }
    });
    write_session(
        &dir,
        "twin",
        &[user(a, None, "go"), summary, last_prompt("fffffff1-ffff-4fff-8fff-ffffffffffff", "go")],
    );

    let result = call(&dir, json!({"selector": "0", "session": "twin"}));
    assert_eq!(result["isError"], false, "got: {}", text_of(&result));
    let t = text_of(&result);
    assert!(t.contains("Recall any single one by its uuid prefix"), "got: {}", &t[..200.min(t.len())]);
    for u in [b, c] {
        assert!(t.contains(&u[..8]), "index should name every covered record");
    }
    // The index has to fit where the concatenation would not.
    assert!(t.chars().count() < 8000, "an index that needs chunking defeats the purpose");
}

#[test]
fn a_transcript_written_with_spaced_json_still_resolves() {
    // Not hypothetical: writers differ, and an anchored `"uuid":"` pre-filter made this return
    // "not in any transcript" rather than an error — a silent miss, the worst failure mode here.
    let dir = tmp_dir();
    let spaced = concat!(
        "{\"type\": \"user\", \"uuid\": \"12345678-aaaa-4aaa-8aaa-aaaaaaaaaaaa\", ",
        "\"parentUuid\": null, \"message\": {\"role\": \"user\", \"content\": ",
        "[{\"type\": \"text\", \"text\": \"spaced serialization\"}]}}\n",
        "{\"type\": \"last-prompt\", \"leafUuid\": \"12345678-aaaa-4aaa-8aaa-aaaaaaaaaaaa\"}\n"
    );
    fs::write(dir.join("spaced.jsonl"), spaced).unwrap();
    let result = call(&dir, json!({"selector": "12345678"}));
    assert_eq!(result["isError"], false, "got: {}", text_of(&result));
    assert!(text_of(&result).contains("spaced serialization"));
}
