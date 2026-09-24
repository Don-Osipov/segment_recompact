//! Regressions from the independent review of v1.0: each test reproduces a defect the review
//! demonstrated, and pins the fix.

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use recompact::*;
use serde_json::{json, Value};

const S: &str = "7e000000-0000-4000-8000-000000000001";

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn env(uuid: &str, parent: Option<&str>, session: &str) -> Value {
    json!({"uuid": uuid, "parentUuid": parent, "sessionId": session, "cwd": "/nonexistent/recompact-test",
           "timestamp": "2026-09-20T10:00:00.000Z", "userType": "external", "isSidechain": false})
}

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    let mut r = env(uuid, parent, S);
    r["type"] = json!("user");
    r["message"] = json!({"role": "user", "content": [{"type": "text", "text": text}]});
    r
}

fn assistant_m(uuid: &str, parent: &str, text: &str, model: &str) -> Value {
    let mut r = env(uuid, Some(parent), S);
    r["type"] = json!("assistant");
    r["message"] = json!({"id": format!("msg_{uuid}"), "role": "assistant", "model": model,
        "type": "message", "stop_reason": "end_turn", "content": [{"type": "text", "text": text}]});
    r
}

fn assistant(uuid: &str, parent: &str, text: &str) -> Value {
    assistant_m(uuid, parent, text, "claude-opus-5-5")
}

fn tool_use(uuid: &str, parent: &str, id: &str) -> Value {
    let mut r = env(uuid, Some(parent), S);
    r["type"] = json!("assistant");
    r["message"] = json!({"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
        "type": "message", "stop_reason": "tool_use",
        "content": [{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": format!("run {id}")}}]});
    r
}

fn tool_result(uuid: &str, parent: &str, id: &str, text: &str) -> Value {
    let mut r = env(uuid, Some(parent), S);
    r["type"] = json!("user");
    r["sourceToolAssistantUUID"] = json!(parent);
    r["message"] = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": text}]});
    r
}

fn last_prompt(leaf: &str) -> Value {
    json!({"type": "last-prompt", "leafUuid": leaf, "sessionId": S, "lastPrompt": "x"})
}

fn write(path: &Path, records: &[Value]) {
    let mut f = fs::File::create(path).unwrap();
    for r in records {
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
}

fn load(p: &Path) -> Vec<Value> {
    fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn assemble(src: &Path, summaries: &Value, extra: &[&str]) -> PathBuf {
    let dir = src.parent().unwrap();
    let sp = dir.join(format!("sums-{}.json", uuid_v4()));
    fs::write(&sp, summaries.to_string()).unwrap();
    let mut args = vec![
        src.to_string_lossy().into_owned(),
        sp.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    assert_eq!(cmd_assemble(&args), 0, "assemble must succeed");
    let id = recompact::lineage_latest(dir, &src.file_stem().unwrap().to_string_lossy());
    dir.join(format!("{id}.jsonl"))
}

fn stub(dir: &Path, summary_chars: usize) -> String {
    let p = dir.join("stub.sh");
    let body = "S".repeat(summary_chars);
    fs::write(
        &p,
        format!(
            r#"#!/bin/sh
keys=$(sed -n 's/^### UNIT //p')
printf '{{'
first=1
for k in $keys; do
  [ $first -eq 1 ] || printf ','
  printf '"%s":"Summary %s {body}"' "$k" "$k"
  first=0
done
printf '}}\n'
"#
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    p.to_string_lossy().into_owned()
}

// ------------------------------------------------------------------------------------ 1

#[test]
fn continue_converges_when_real_summaries_cost_more_than_the_estimate() {
    let dir = tmp_dir();
    let mut recs = Vec::new();
    let mut parent: Option<String> = None;
    for t in 0..8 {
        let (u, a) = (format!("u{t}"), format!("a{t}"));
        recs.push(user(&u, parent.as_deref(), &format!("write section {t}")));
        recs.push(assistant(
            &a,
            &u,
            &format!("SECTION-{t} {}", "prose words ".repeat(250)),
        ));
        parent = Some(a);
    }
    recs.push(last_prompt(parent.as_deref().unwrap()));
    let src = dir.join(format!("{S}.jsonl"));
    write(&src, &recs);
    let conv = calibrate(&recs).conv_tokens(&recs);
    for chars in [200, 900, 1500] {
        let d = tmp_dir();
        let src2 = d.join(format!("{S}.jsonl"));
        fs::copy(&src, &src2).unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_recompact"))
            .args([
                "continue",
                &src2.to_string_lossy(),
                "--overhead",
                "0",
                "--threshold",
                &(conv - 1200).to_string(),
                "--summarize-with",
                "stub",
                "--claude-bin",
                &stub(&d, chars),
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "continue must converge with {chars}-char summaries: {err}"
        );
        assert_ne!(
            lineage_latest(&d, S),
            S,
            "a twin was written ({chars}-char summaries)"
        );
        // Planning with the lengths assemble will see means assemble never refuses: the old single
        // pass priced unwritten summaries at 400 tokens, so assemble's re-plan asked for different
        // units, failed the attempt, and blamed it on the source changing.
        assert!(
            !err.contains("re-syncing") && !err.contains("needs summaries"),
            "no failed attempt ({chars} chars): {err}"
        );
    }
}

// ------------------------------------------------------------------------------------ 2

#[test]
fn a_twin_resumes_on_the_model_the_session_last_ran_with_its_window_and_effort() {
    let dir = tmp_dir();
    let mut effort = user(
        "e1",
        Some("a1"),
        "<local-command-stdout>Set effort level to max (this session only)</local-command-stdout>",
    );
    effort["isMeta"] = json!(true);
    let mut a2 = assistant_m("a2", "u2", "done on opus", "claude-opus-5-5");
    a2["message"]["usage"] = json!({"input_tokens": 10, "cache_read_input_tokens": 310_000, "cache_creation_input_tokens": 0});
    let recs = vec![
        user("u1", None, "start"),
        assistant_m("a1", "u1", "started on sonnet", "claude-sonnet-4-5"),
        effort,
        user("u2", Some("e1"), "continue"),
        a2,
        user("u3", Some("a2"), "more"),
        assistant_m("a3", "u3", "ok", "claude-opus-5-5"),
        last_prompt("a3"),
    ];
    let src = dir.join(format!("{S}.jsonl"));
    write(&src, &recs);
    let twin = load(&assemble(
        &src,
        &json!({"0": "Started.", "1": "Continued."}),
        &[],
    ));
    let pre = twin
        .iter()
        .find(|r| r["recompactPreamble"] == true)
        .unwrap();
    assert_eq!(
        pre["message"]["model"], "claude-opus-5-5",
        "minted records carry the last model"
    );
    assert_eq!(
        resume_flags(&twin),
        vec!["--model", "claude-opus-5-5[1m]", "--effort", "max"]
    );
}

// ------------------------------------------------------------------------------------ 3, 4

fn long_final_turn() -> Vec<Value> {
    let mut recs = vec![
        user("u1", None, "first"),
        assistant("a1", "u1", "ok"),
        user("u2", Some("a1"), "long job"),
    ];
    let mut parent = "u2".to_string();
    for i in 0..12 {
        let (t, r) = (format!("t{i}"), format!("r{i}"));
        recs.push(tool_use(&t, &parent, &format!("id{i}")));
        recs.push(tool_result(
            &r,
            &t,
            &format!("id{i}"),
            &format!("RAW-{i:02} {}", "x".repeat(4000)),
        ));
        parent = r;
    }
    recs.push(assistant("fin", &parent, "all steps ran"));
    recs.push(last_prompt("fin"));
    recs
}

fn needed_keys(src: &Path, extra: &[&str]) -> Vec<String> {
    let ws = src.parent().unwrap().join(format!("ws-{}.json", uuid_v4()));
    let mut args = vec![
        src.to_string_lossy().into_owned(),
        "--out".into(),
        ws.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    assert_eq!(cmd_extract(&args), 0);
    let doc: Value = serde_json::from_str(&fs::read_to_string(&ws).unwrap()).unwrap();
    doc["segments_needing_summary"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_tail_the_budget_cut_compacts_in_the_next_generation_and_its_summaries_stay_pinned() {
    let dir = tmp_dir();
    let src = dir.join(format!("{S}.jsonl"));
    write(&src, &long_final_turn());
    let opts = ["--tail-budget", "8000", "--split", "4000"];
    let keys = needed_keys(&src, &opts);
    assert!(
        !keys.is_empty(),
        "the oversized tail has older parts to summarize"
    );
    let sums: serde_json::Map<String, Value> = keys
        .iter()
        .map(|k| (k.clone(), json!(format!("Earlier steps {k} ran."))))
        .collect();
    let gen1 = assemble(&src, &Value::Object(sums), &opts);
    let g1 = load(&gen1);
    let pinned_raw: Vec<String> = (0..12)
        .map(|i| format!("RAW-{i:02} "))
        .filter(|m| {
            g1.iter().any(|r| {
                r.to_string().contains(m.as_str()) && r.to_string().contains(&"x".repeat(4000))
            })
        })
        .collect();
    assert!(
        !pinned_raw.is_empty(),
        "generation 1 keeps the newest parts raw"
    );

    // The session goes on: the cut turn is no longer the tail.
    let leaf = g1
        .iter()
        .rev()
        .find_map(|r| r["uuid"].as_str().map(str::to_string))
        .unwrap();
    let mut grown: Vec<Value> = g1
        .iter()
        .filter(|r| r["type"] != "last-prompt")
        .cloned()
        .collect();
    let sid = g1[0]["sessionId"].as_str().unwrap().to_string();
    let mut nu = user("n1", Some(&leaf), "next");
    nu["sessionId"] = json!(sid);
    let mut na = assistant("n2", "n1", "fine");
    na["sessionId"] = json!(sid);
    grown.extend([
        nu,
        na,
        json!({"type": "last-prompt", "leafUuid": "n2", "sessionId": sid}),
    ]);
    write(&gen1, &grown);

    // #4: even when the cut turn is back in a keep-2 tail, its summaries are never re-summarized.
    let ws = dir.join("ws2.json");
    assert_eq!(
        cmd_extract(&[
            gen1.to_string_lossy().into_owned(),
            "--out".into(),
            ws.to_string_lossy().into_owned(),
            "--keep".into(),
            "2".into(),
            "--tail-budget".into(),
            "8000".into(),
            "--split".into(),
            "4000".into()
        ]),
        0
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&ws).unwrap()).unwrap();
    let synth: Vec<String> = grown
        .iter()
        .filter(|r| r["recompactSynthetic"] == true)
        .map(|r| r["uuid"].as_str().unwrap().to_string())
        .collect();
    let needed: Vec<String> = doc["segments_needing_summary"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_string())
        .collect();
    for seg in doc["segments"].as_array().unwrap() {
        for part in seg["parts"].as_array().cloned().unwrap_or_default() {
            if needed.contains(&part["key"].as_str().unwrap().to_string()) {
                let covered = part["covered_uuids"].to_string();
                assert!(
                    synth.iter().all(|u| !covered.contains(u.as_str())),
                    "part {} would re-summarize a summary",
                    part["key"]
                );
            }
        }
    }

    // #3: generation 2 compacts the raw remainder; the summaries ride along untouched.
    let out = dir.join("gen2.jsonl");
    assert_eq!(
        cmd_assemble(&[
            gen1.to_string_lossy().into_owned(),
            "--mode".into(),
            "mask".into(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
            "--tail-budget".into(),
            "8000".into(),
            "--split".into(),
            "4000".into()
        ]),
        0
    );
    let g2 = fs::read_to_string(&out).unwrap();
    assert!(
        !g2.contains(&"x".repeat(4000)),
        "the formerly pinned raw parts are masked now"
    );
    for u in &synth {
        assert!(g2.contains(u.as_str()), "summary {u} kept verbatim");
    }
}

// ------------------------------------------------------------------------------------ 5

#[test]
fn query_never_returns_turns_from_outside_this_sessions_history() {
    let root = tmp_dir();
    let dir = root.join("-proj");
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join(format!("{S}.jsonl"));
    write(
        &src,
        &[
            user("aa000001-0000-4000-8000-000000000000", None, "investigate"),
            assistant(
                "aa000002-0000-4000-8000-000000000000",
                "aa000001-0000-4000-8000-000000000000",
                "Found SECRETWORD-A in the logs.",
            ),
            user(
                "aa000003-0000-4000-8000-000000000000",
                Some("aa000002-0000-4000-8000-000000000000"),
                "ok",
            ),
            assistant(
                "aa000004-0000-4000-8000-000000000000",
                "aa000003-0000-4000-8000-000000000000",
                "noted",
            ),
            last_prompt("aa000004-0000-4000-8000-000000000000"),
        ],
    );
    let twin_path = assemble(&src, &json!({"0": "I investigated."}), &[]);
    let twin_id = twin_path
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // The source keeps going after the cut.
    let mut grown = load(&src);
    grown.pop();
    grown.push(user(
        "aa000005-0000-4000-8000-000000000000",
        Some("aa000004-0000-4000-8000-000000000000"),
        "then",
    ));
    grown.push(assistant(
        "aa000006-0000-4000-8000-000000000000",
        "aa000005-0000-4000-8000-000000000000",
        "Dropped table LATERWORD in prod.",
    ));
    grown.push(last_prompt("aa000006-0000-4000-8000-000000000000"));
    write(&src, &grown);

    let ctx = RecallCtx {
        dir: dir.clone(),
        session: Some(twin_id),
    };
    let hit = recall_query(&ctx, "SECRETWORD-A", None, 8).unwrap();
    assert!(hit.contains("aa000002"), "{hit}");
    let miss = recall_query(&ctx, "LATERWORD", None, 8).unwrap();
    assert!(
        miss.starts_with("no match"),
        "post-cut growth is not this session's history: {miss}"
    );

    // A branch does not see its parent's turns after the branch point.
    let branch = "7e0000bb-0000-4000-8000-000000000002";
    let mut b1 = user("aa000001-0000-4000-8000-000000000000", None, "investigate");
    b1["sessionId"] = json!(branch);
    b1["forkedFrom"] =
        json!({"sessionId": S, "messageUuid": "aa000004-0000-4000-8000-000000000000"});
    write(
        &dir.join(format!("{branch}.jsonl")),
        &[
            b1,
            json!({"type": "last-prompt", "leafUuid": "aa000001-0000-4000-8000-000000000000", "sessionId": branch}),
        ],
    );
    let bctx = RecallCtx {
        dir: dir.clone(),
        session: Some(branch.to_string()),
    };
    assert!(recall_query(&bctx, "LATERWORD", None, 8)
        .unwrap()
        .starts_with("no match"));
    // And a twin never looks like a branch of its source's parent.
    assert!(load(&twin_path)
        .iter()
        .all(|r| r.get("forkedFrom").is_none()));
}

// ------------------------------------------------------------------------------------ 6

#[test]
fn a_half_written_last_line_does_not_take_recall_down() {
    let root = tmp_dir();
    let dir = root.join("-proj");
    fs::create_dir_all(&dir).unwrap();
    let src = dir.join(format!("{S}.jsonl"));
    write(
        &src,
        &[
            user("cc000001-0000-4000-8000-000000000000", None, "go"),
            assistant(
                "cc000002-0000-4000-8000-000000000000",
                "cc000001-0000-4000-8000-000000000000",
                "DETAIL-42 found",
            ),
            user(
                "cc000003-0000-4000-8000-000000000000",
                Some("cc000002-0000-4000-8000-000000000000"),
                "next",
            ),
            assistant(
                "cc000004-0000-4000-8000-000000000000",
                "cc000003-0000-4000-8000-000000000000",
                "ok",
            ),
            last_prompt("cc000004-0000-4000-8000-000000000000"),
        ],
    );
    let twin = load(&assemble(&src, &json!({"0": "Found it."}), &[]));
    // The live source is mid-write.
    let mut f = fs::OpenOptions::new().append(true).open(&src).unwrap();
    write!(f, "{{\"type\":\"assistant\",\"uuid\":\"half").unwrap();
    drop(f);
    let footer = &twin
        .iter()
        .find(|r| r["recompactSynthetic"] == true)
        .unwrap()["uuid"]
        .as_str()
        .unwrap()[..8];
    let reqs = format!(
        "{}\n{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"selector": footer}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"ping"})
    );
    let mut out: Vec<u8> = Vec::new();
    assert_eq!(
        mcp_serve(Cursor::new(reqs.into_bytes()), &mut out, dir.as_path()),
        0
    );
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "the server answered both requests");
    assert!(lines[0].to_string().contains("DETAIL-42"));

    assert!(
        parse_jsonl("{\"a\":1}\n{\"b\":").unwrap().len() == 1,
        "a torn last line is skipped"
    );
    assert!(
        parse_jsonl("{\"a\":1}\n{\"b\":\n{\"c\":3}").is_err(),
        "a torn middle line is corruption"
    );
}

// ------------------------------------------------------------------------------------ 7

#[test]
fn recall_prefers_originals_over_trimmed_copies_and_query_skips_visible_text() {
    let root = tmp_dir();
    let dir = root.join("-proj");
    fs::create_dir_all(&dir).unwrap();
    let mut thought = assistant(
        "dd000002-0000-4000-8000-000000000000",
        "dd000001-0000-4000-8000-000000000000",
        "the answer",
    );
    thought["message"]["content"] = json!([{"type": "thinking", "thinking": "DEEP-REASONING-XYZ", "signature": "sig"},
                                          {"type": "text", "text": "the answer"}]);
    let mut shot = user(
        "dd000003-0000-4000-8000-000000000000",
        Some("dd000002-0000-4000-8000-000000000000"),
        "LOOKHERE at this",
    );
    shot["message"]["content"] = json!([{"type": "text", "text": "LOOKHERE at this"},
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(20_000)}}]);
    let src = dir.join(format!("{S}.jsonl"));
    write(
        &src,
        &[
            user("dd000001-0000-4000-8000-000000000000", None, "think"),
            thought,
            shot,
            assistant(
                "dd000004-0000-4000-8000-000000000000",
                "dd000003-0000-4000-8000-000000000000",
                "seen",
            ),
            user(
                "dd000005-0000-4000-8000-000000000000",
                Some("dd000004-0000-4000-8000-000000000000"),
                "done",
            ),
            assistant(
                "dd000006-0000-4000-8000-000000000000",
                "dd000005-0000-4000-8000-000000000000",
                "ok",
            ),
            last_prompt("dd000006-0000-4000-8000-000000000000"),
        ],
    );
    let out = dir.join("masked.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            "--mode".into(),
            "mask".into(),
            "--out".into(),
            out.to_string_lossy().into_owned()
        ]),
        0
    );
    let twin = load(&out);
    let copy = twin
        .iter()
        .find(|r| r["uuid"] == "dd000002-0000-4000-8000-000000000000")
        .unwrap();
    assert_eq!(copy["recompactThinkingStripped"], true);
    let got = rehydrate_select(&twin, "dd000002").unwrap();
    assert!(
        got[0].to_string().contains("DEEP-REASONING-XYZ"),
        "the original, with its thinking"
    );

    let twin_id = twin[0]["sessionId"].as_str().unwrap().to_string();
    fs::rename(&out, dir.join(format!("{twin_id}.jsonl"))).unwrap();
    let ctx = RecallCtx {
        dir: dir.clone(),
        session: Some(twin_id),
    };
    let q = recall_query(&ctx, "LOOKHERE", None, 8).unwrap();
    assert!(
        q.starts_with("no match"),
        "a user turn's text is visible even with its image elided: {q}"
    );
}

// ------------------------------------------------------------------------------------ 8, 9

#[test]
fn relative_paths_cut_at_directory_boundaries() {
    assert_eq!(
        relative_to("/repo/app/src/x.rs", Some("/repo/app/src")),
        "x.rs"
    );
    assert_eq!(
        relative_to("/repo/app/src-gen/x.rs", Some("/repo/app/src")),
        "/repo/app/src-gen/x.rs"
    );
    assert_eq!(
        relative_to("/repo/app/src/x.rs", None),
        "/repo/app/src/x.rs"
    );
}

#[test]
fn a_truncated_summarizer_reply_is_a_miss_not_a_panic() {
    assert!(extract_json_object("} partial {\"3\": \"trunc").is_none());
    assert!(extract_json_object("noise {\"3\": \"ok\"} tail").is_some());
}
