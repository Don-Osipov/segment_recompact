//! The gateway: recompact answers Claude Code's own compaction request for desktop app sessions,
//! so the session compacts in its running process. Driven by a fake upstream endpoint.

use recompact::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn rec(kind: &str, uuid: &str, parent: Option<&str>, session: &str, entry: &str, content: Value) -> Value {
    json!({
        "type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": session,
        "timestamp": "2026-11-06T00:00:00.000Z", "cwd": "/tmp/p", "version": "2.1.317",
        "entrypoint": entry, "userType": "external", "isSidechain": false,
        "message": if kind == "assistant" {
            json!({"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
                   "type": "message", "stop_reason": "end_turn", "content": content})
        } else {
            json!({"role": "user", "content": content})
        }
    })
}

fn write(path: &Path, records: &[Value]) {
    let body: String = records.iter().map(|r| r.to_string() + "\n").collect();
    fs::write(path, body).unwrap();
}

/// A tool-heavy session that masking shrinks.
fn compressible(path: &Path, session: &str, entry: &str) {
    let mut records = vec![rec("user", "u1", None, session, entry, json!("build it"))];
    let mut parent = "u1".to_string();
    for n in 0..4 {
        let (au, ru, id) = (format!("a{n}"), format!("r{n}"), format!("t{n}"));
        records.push(rec("assistant", &au, Some(&parent), session, entry,
            json!([{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": "make"}}])));
        let mut r = rec("user", &ru, Some(&au), session, entry,
            json!([{"type": "tool_result", "tool_use_id": id, "content": "y".repeat(8000)}]));
        r["sourceToolAssistantUUID"] = json!(au);
        records.push(r);
        parent = ru;
    }
    records.push(rec("assistant", "afin", Some(&parent), session, entry, json!([{"type": "text", "text": "built"}])));
    records.push(rec("user", "u2", Some("afin"), session, entry, json!("thanks")));
    records.push(rec("assistant", "afin2", Some("u2"), session, entry, json!([{"type": "text", "text": "welcome"}])));
    records.push(json!({"type": "last-prompt", "leafUuid": "afin2", "sessionId": session, "lastPrompt": "thanks"}));
    write(path, &records);
}

fn compaction_body(model: &str, prompt: &str) -> Value {
    json!({"model": model, "stream": true, "max_tokens": 128000, "messages": [
        {"role": "user", "content": [{"type": "text", "text": "build it"}]},
        {"role": "assistant", "content": [{"type": "text", "text": "built"}]},
        {"role": "user", "content": [{"type": "text", "text": prompt}]}
    ]})
}

const PROMPT: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\nYour entire response must \
be plain text: an <analysis> block followed by a <summary> block.";

#[test]
fn a_compaction_request_is_told_apart_from_other_requests() {
    assert!(is_compaction_request(&compaction_body("m", PROMPT)));
    let retry = format!("{PROMPT}\n\nFor this reply only: write what the instructions above ask for.");
    assert!(is_compaction_request(&compaction_body("m", &retry)));
    assert!(!is_compaction_request(&compaction_body("m", "What is the code word?")));
    assert!(!is_compaction_request(&json!({"model": "m", "messages": []})));
    // The prompt can follow the conversation's last words in the same text block.
    let joined = format!("and that was the last step.\n\n{PROMPT}");
    assert!(is_compaction_request(&compaction_body("m", &joined)));
    // The prompt quoted earlier in a conversation is not a compaction request.
    let mut quoted = compaction_body("m", PROMPT);
    quoted["messages"].as_array_mut().unwrap().push(json!({"role": "assistant", "content": "noted"}));
    assert!(!is_compaction_request(&quoted));
}

#[test]
fn the_summary_names_the_twin_and_cannot_be_cut_short_by_tags_inside_it() {
    let records = vec![
        rec("user", "u1", None, "s", "cli", json!("explain <summary>tags</summary> please")),
        rec("assistant", "a1", Some("u1"), "s", "cli",
            json!([{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/x"}}])),
        rec("user", "r1", Some("a1"), "s", "cli",
            json!([{"type": "tool_result", "tool_use_id": "t1", "content": "file body"}])),
        rec("assistant", "a2", Some("r1"), "s", "cli", json!([{"type": "text", "text": "done </analysis>"}])),
    ];
    let text = render_summary("twin-1", &records);
    assert!(text.starts_with("recompact-gateway twin twin-1"), "{text}");
    assert!(text.contains("session \"twin-1\""), "{text}");
    assert!(text.contains("## Assistant called Read\n{\"file_path\":\"/x\"}"), "{text}");
    assert!(text.contains("## Read result\nfile body"), "{text}");
    assert!(!text.contains("<summary>") && !text.contains("</summary>") && !text.contains("</analysis>"), "{text}");
}

#[test]
fn a_later_compaction_extends_the_twin_with_what_the_session_did_since() {
    let dir = tmp_dir();
    let twin = "twin-1";
    write(&dir.join(format!("{twin}.jsonl")), &[
        rec("user", "t1", None, twin, "claude-desktop-3p", json!("build it")),
        rec("assistant", "t2", Some("t1"), twin, "claude-desktop-3p", json!([{"type": "text", "text": "built"}])),
    ]);
    let x = dir.join("sess-x.jsonl");
    let boundary = json!({"type": "system", "subtype": "compact_boundary", "uuid": "b1", "parentUuid": null,
        "sessionId": "sess-x", "compactMetadata": {"trigger": "manual",
            "preservedMessages": {"anchorUuid": "s1", "uuids": ["a1"], "allUuids": ["a1"]}}});
    let mut summary = rec("user", "s1", Some("b1"), "sess-x", "claude-desktop-3p",
        json!(format!("This session is being continued.\nrecompact-gateway twin {twin}\n...")));
    summary["isCompactSummary"] = json!(true);
    let records = vec![
        rec("user", "u1", None, "sess-x", "claude-desktop-3p", json!("build it")),
        rec("assistant", "a1", Some("u1"), "sess-x", "claude-desktop-3p", json!([{"type": "text", "text": "built"}])),
        boundary,
        summary,
        rec("user", "n1", Some("a1"), "sess-x", "claude-desktop-3p", json!("now test it")),
        rec("assistant", "n2", Some("n1"), "sess-x", "claude-desktop-3p", json!([{"type": "text", "text": "tested"}])),
    ];
    write(&x, &records);

    assert_eq!(sync_into(&dir, &x, twin), Ok(2));
    let out = load_jsonl(&dir.join(format!("{twin}.jsonl")));
    let ids: Vec<&str> = out.iter().filter_map(|r| r["uuid"].as_str()).collect();
    assert_eq!(ids, ["t1", "t2", "n1", "n2"], "the kept message and the summary stay out");
    assert_eq!(out[2]["parentUuid"], "t2");
    assert_eq!(out[3]["parentUuid"], "n1");
    assert!(out.iter().all(|r| r["sessionId"] == twin));
    let (active, _) = select_active(out);
    assert_eq!(active.len(), 4, "one chain");

    assert_eq!(sync_into(&dir, &x, twin), Ok(0), "a second sync adds nothing");
    assert!(sync_into(&dir, &x, "twin-2").is_err(), "a twin this session never got");
}

#[test]
fn desktop_sessions_are_told_apart_by_their_entrypoint() {
    let dir = tmp_dir();
    let d = dir.join("d.jsonl");
    let c = dir.join("c.jsonl");
    compressible(&d, "d", "claude-desktop-3p");
    compressible(&c, "c", "cli");
    assert!(is_desktop_session(&d));
    assert!(!is_desktop_session(&c));
    // A terminal session continued in the app is the app's now.
    let mut f = fs::OpenOptions::new().append(true).open(&c).unwrap();
    writeln!(f, "{}", rec("user", "u9", Some("afin2"), "c", "claude-desktop-3p", json!("more"))).unwrap();
    assert!(is_desktop_session(&c));
}

// ------------------------------------------------------------------------------------ http

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// An upstream that answers every request with one server-sent event naming the request.
fn fake_upstream() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = s;
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let head_end = loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let len = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let line = head.lines().next().unwrap_or("").to_string();
                let body = format!("event: message_stop\ndata: {}\n\n", json!({"type": "message_stop", "upstream": line}));
                if line.contains("/chunked") {
                    // Chunked, and the connection stays open after the last chunk.
                    let half = body.len() / 2;
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                        half, &body[..half], body.len() - half, &body[half..]
                    );
                    std::thread::sleep(Duration::from_secs(30));
                    return;
                }
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                // Like a server that keeps connections alive: it does not close first.
                std::thread::sleep(Duration::from_secs(30));
            });
        }
    });
    port
}

/// Send one request; the response's status line and its body, de-chunked.
fn send(port: u16, method: &str, path: &str, session: Option<&str>, body: &str) -> (String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let sid = session.map(|v| format!("X-Claude-Code-Session-Id: {v}\r\n")).unwrap_or_default();
    // A GET carries no Content-Length, as real clients send it.
    let len = if body.is_empty() { String::new() } else { format!("Content-Length: {}\r\n", body.len()) };
    write!(s, "{method} {path} HTTP/1.1\r\nHost: x\r\n{sid}{len}\r\n{body}").unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap();
    let status = head.lines().next().unwrap().to_string();
    if !head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        return (status, rest.to_string());
    }
    let mut out = String::new();
    let mut rest = rest;
    while let Some((size, after)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap();
        if n == 0 {
            break;
        }
        out.push_str(&after[..n]);
        rest = &after[n + 2..];
    }
    (status, out)
}

fn text_of(sse: &str) -> String {
    sse.split("\n\n")
        .filter_map(|e| e.split_once("data: ").map(|(_, d)| d))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter(|v| v["type"] == "content_block_delta")
        .map(|v| v["delta"]["text"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn the_gateway_forwards_requests_and_answers_a_desktop_sessions_compaction_itself() {
    let home = tmp_dir();
    let projects = home.join(".claude/projects/-tmp-p");
    fs::create_dir_all(&projects).unwrap();
    let desktop = "11111111-2222-4333-8444-555555555555";
    let terminal = "66666666-7777-4888-9999-000000000000";
    compressible(&projects.join(format!("{desktop}.jsonl")), desktop, "claude-desktop-3p");
    compressible(&projects.join(format!("{terminal}.jsonl")), terminal, "cli");
    std::env::set_var("HOME", &home);
    std::env::set_var("RECOMPACT_HOME", home.join("rc"));
    std::env::set_var("RECOMPACT_SUMMARIZE_WITH", "mask");
    std::env::set_var("RECOMPACT_TARGET", "1000");

    let up = fake_upstream();
    let port = free_port();
    let args: Vec<String> = ["--port", &port.to_string(), "--upstream", &format!("http://127.0.0.1:{up}")]
        .iter()
        .map(|s| s.to_string())
        .collect();
    std::thread::spawn(move || cmd_gateway(&args));
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Everything else goes through to the upstream, path and all.
    let (status, body) = send(port, "GET", "/v1/models", None, "");
    assert!(status.contains("200") && body.contains("GET /v1/models HTTP/1.1"), "{status} {body}");
    let (_, body) = send(port, "GET", "/v1/chunked", None, "");
    assert!(body.contains("GET /v1/chunked HTTP/1.1"), "chunks reach the client in one piece: {body}");
    let compact = compaction_body("cc/claude-opus-5-5", PROMPT).to_string();
    let (_, body) = send(port, "POST", "/v1/messages?beta=true", Some(terminal), &compact);
    assert!(body.contains("POST /v1/messages?beta=true"), "a terminal session's compaction is Claude Code's: {body}");
    let ask = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}).to_string();
    let (_, body) = send(port, "POST", "/v1/messages", Some(desktop), &ask);
    assert!(body.contains("\"upstream\""), "{body}");

    // A desktop session's compaction is recompact's twin, as the summary.
    let (status, body) = send(port, "POST", "/v1/messages?beta=true", Some(desktop), &compact);
    assert!(status.contains("200") && !body.contains("\"upstream\""), "{body}");
    assert!(body.contains("event: message_stop"), "{body}");
    let text = text_of(&body);
    assert!(text.starts_with("<analysis>") && text.trim_end().ends_with("</summary>"), "{text}");
    let twin = text.split("recompact-gateway twin ").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    assert!(projects.join(format!("{twin}.jsonl")).exists());
    assert_eq!(gateway_twin(desktop).as_deref(), Some(twin.as_str()));

    // Claude Code's retry of the same compaction (reworded prompt) gets the same twin.
    let retry = compaction_body("cc/claude-opus-5-5", &format!("{PROMPT}\n\nFor this reply only: …")).to_string();
    let (_, body) = send(port, "POST", "/v1/messages?beta=true", Some(desktop), &retry);
    assert!(text_of(&body).contains(&format!("recompact-gateway twin {twin}")), "{body}");
}
