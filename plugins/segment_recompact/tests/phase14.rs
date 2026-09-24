//! Recall v2: every call a resumed session actually made against v0.9 failed. These pin down the
//! five causes: footers that named file-local keys, a guessed session, a single project dir,
//! absolute provenance paths that go stale when a session relocates, and no way to search.

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use recompact::*;
use serde_json::{json, Value};

fn tmp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user(uuid: &str, parent: Option<&str>, text: &str, session: &str) -> Value {
    json!({"type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": session,
           "timestamp": "2026-09-20T10:00:00.000Z", "userType": "external", "isSidechain": false,
           "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
}

fn assistant(uuid: &str, parent: &str, text: &str, session: &str) -> Value {
    json!({"type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": session,
           "timestamp": "2026-09-20T10:00:01.000Z", "userType": "external", "isSidechain": false,
           "message": {"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
                       "type": "message", "stop_reason": "end_turn",
                       "content": [{"type": "text", "text": text}]}})
}

fn last_prompt(leaf: &str, session: &str) -> Value {
    json!({"type": "last-prompt", "leafUuid": leaf, "sessionId": session, "lastPrompt": "x"})
}

fn write(path: &Path, records: &[Value]) {
    let mut f = fs::File::create(path).unwrap();
    for r in records {
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
}

fn load(p: &Path) -> Vec<Value> {
    fs::read_to_string(p).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

const ORIG: &str = "0a000000-0000-4000-8000-000000000001";

/// A project dir with an original session and its assembled twin. Returns (dir, twin id, twin).
fn compacted_project(root: &Path) -> (PathBuf, String, Vec<Value>) {
    let dir = root.join("-proj-main");
    fs::create_dir_all(&dir).unwrap();
    let orig = dir.join(format!("{ORIG}.jsonl"));
    write(
        &orig,
        &[
            user("b0000000-0000-4000-8000-000000000001", None, "investigate the outage", ORIG),
            assistant("b0000000-0000-4000-8000-000000000002", "b0000000-0000-4000-8000-000000000001",
                "Root cause: the pgbouncer pool hit max_client_conn=400 at 09:14; error PGRST-9001 in the logs.", ORIG),
            user("b0000000-0000-4000-8000-000000000003", Some("b0000000-0000-4000-8000-000000000002"), "fix it", ORIG),
            assistant("b0000000-0000-4000-8000-000000000004", "b0000000-0000-4000-8000-000000000003", "Raised the pool.", ORIG),
            last_prompt("b0000000-0000-4000-8000-000000000004", ORIG),
        ],
    );
    let sums = root.join("sums.json");
    fs::write(&sums, json!({"0": "I found the outage cause."}).to_string()).unwrap();
    let out = dir.join("twin-out.jsonl");
    assert_eq!(
        cmd_assemble(&[orig.to_string_lossy().into_owned(), sums.to_string_lossy().into_owned(),
            "--out".into(), out.to_string_lossy().into_owned()]),
        0
    );
    let twin = load(&out);
    let twin_id = twin.iter().find_map(|r| r["sessionId"].as_str()).unwrap().to_string();
    let twin_path = dir.join(format!("{twin_id}.jsonl"));
    fs::rename(&out, &twin_path).unwrap();
    (dir, twin_id, twin)
}

fn serve(ctx: RecallCtx, args: Value) -> Value {
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":args}});
    let mut out: Vec<u8> = Vec::new();
    assert_eq!(mcp_serve(Cursor::new(format!("{req}\n").into_bytes()), &mut out, ctx), 0);
    let resp: Value = serde_json::from_str(String::from_utf8(out).unwrap().lines().next().unwrap()).unwrap();
    resp["result"].clone()
}

fn text_of(result: &Value) -> String {
    result["content"].as_array().unwrap().iter()
        .filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n")
}

#[test]
fn the_footer_id_resolves_with_no_session_known_at_all() {
    let root = tmp_root();
    let (dir, _twin_id, twin) = compacted_project(&root);
    let synth = twin.iter().find(|r| r["recompactSynthetic"] == true).unwrap();
    let footer_id = &synth["uuid"].as_str().unwrap()[..8];
    let footer = synth["message"]["content"][0]["text"].as_str().unwrap();
    assert!(footer.contains(&format!("recall {footer_id}")));
    let r = serve(RecallCtx::from(dir.as_path()), json!({"selector": footer_id}));
    assert_eq!(r["isError"], false, "{}", text_of(&r));
    assert!(text_of(&r).contains("max_client_conn=400"), "{}", text_of(&r));
}

#[test]
fn a_bound_session_makes_keys_and_listing_exact_and_an_unbound_one_never_guesses() {
    let root = tmp_root();
    let (dir, twin_id, _) = compacted_project(&root);
    // A newer, unrelated session in the same dir: v0.9 would have resolved keys against it.
    write(&dir.join("ffffffff-0000-4000-8000-00000000000f.jsonl"),
        &[user("f1000000-0000-4000-8000-000000000001", None, "other work", "ffff"),
          last_prompt("f1000000-0000-4000-8000-000000000001", "ffff")]);
    let bound = || RecallCtx { dir: dir.clone(), session: Some(twin_id.clone()) };
    let r = serve(bound(), json!({"selector": "0"}));
    assert_eq!(r["isError"], false, "{}", text_of(&r));
    assert!(text_of(&r).contains("max_client_conn=400"));
    let listing = serve(bound(), json!({}));
    assert!(text_of(&listing).contains("(part 0,"), "{}", text_of(&listing));

    let unbound = serve(RecallCtx::from(dir.as_path()), json!({"selector": "0"}));
    assert_eq!(unbound["isError"], true);
    assert!(text_of(&unbound).contains("does not know which session"), "{}", text_of(&unbound));
}

#[test]
fn ids_and_sessions_resolve_across_project_dirs() {
    let root = tmp_root();
    let (dir, twin_id, twin) = compacted_project(&root);
    // The server was started for a worktree's project dir; the session lives in the main one.
    let worktree_dir = root.join("-proj-main--claude-worktrees-feature");
    fs::create_dir_all(&worktree_dir).unwrap();
    let synth_id = &twin.iter().find(|r| r["recompactSynthetic"] == true).unwrap()["uuid"].as_str().unwrap()[..8];
    let r = serve(RecallCtx::from(worktree_dir.as_path()), json!({"selector": synth_id}));
    assert_eq!(r["isError"], false, "{}", text_of(&r));
    let r = serve(RecallCtx { dir: worktree_dir.clone(), session: Some(twin_id.clone()) }, json!({"selector": "0"}));
    assert_eq!(r["isError"], false, "{}", text_of(&r));
    let _ = dir;
}

#[test]
fn provenance_survives_the_source_moving_to_another_project_dir() {
    let root = tmp_root();
    let (dir, twin_id, _) = compacted_project(&root);
    // Claude Code relocated the original when it was resumed from a worktree.
    let moved = root.join("-proj-main--claude-worktrees-moved");
    fs::create_dir_all(&moved).unwrap();
    fs::rename(dir.join(format!("{ORIG}.jsonl")), moved.join(format!("{ORIG}.jsonl"))).unwrap();
    let twin = load(&dir.join(format!("{twin_id}.jsonl")));
    let got = rehydrate_select(&twin, "0").expect("provenance follows the session id");
    assert!(got.iter().any(|r| r.to_string().contains("max_client_conn=400")));
}

#[test]
fn query_searches_what_compaction_removed_and_skips_what_is_still_visible() {
    let root = tmp_root();
    let (dir, twin_id, _) = compacted_project(&root);
    let ctx = || RecallCtx { dir: dir.clone(), session: Some(twin_id.clone()) };
    let r = serve(ctx(), json!({"query": "max_client_conn"}));
    assert_eq!(r["isError"], false, "{}", text_of(&r));
    let t = text_of(&r);
    assert!(t.contains("1 match") && t.contains("b0000000") && t.contains("max_client_conn=400"), "{t}");
    // An exact error code finds it too; a phrase that only appears in visible turns does not.
    assert!(text_of(&serve(ctx(), json!({"query": "PGRST-9001"}))).contains("b0000000"));
    let visible = text_of(&serve(ctx(), json!({"query": "\"Raised the pool\""})));
    assert!(visible.starts_with("no match"), "visible records are not search results: {visible}");
    // Without a session, query explains what it needs instead of searching everything.
    let unbound = serve(RecallCtx::from(dir.as_path()), json!({"query": "pool"}));
    assert_eq!(unbound["isError"], true);
}

#[test]
fn a_key_carried_by_several_generations_is_refused_with_its_ids() {
    let mk = |uuid: &str| {
        let mut s = assistant(uuid, "u1", "summary", "t");
        s["recompactSynthetic"] = json!(true);
        s["recompactProvenance"] = json!({"source": "/gone", "sourceSessionId": "gone", "part": "3.1", "coveredUuids": ["x"]});
        s
    };
    let twin = vec![user("u1", None, "hi", "t"), mk("11111111-0000-4000-8000-000000000000"), mk("22222222-0000-4000-8000-000000000000")];
    let err = rehydrate_select(&twin, "3.1").unwrap_err();
    assert!(err.contains("11111111") && err.contains("22222222"), "{err}");
}
