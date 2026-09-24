//! v1.0: accounting that matches what the model is sent, twins that orient the session resumed
//! into them, and summaries whose load-bearing specifics are carried by code.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use recompact::*;
use serde_json::{json, Value};

const SESSION: &str = "5e5510a0-0000-4000-8000-000000000001";

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn env(uuid: &str, parent: Option<&str>) -> Value {
    json!({"uuid": uuid, "parentUuid": parent, "sessionId": SESSION, "cwd": "/nonexistent/recompact-test",
           "timestamp": "2026-09-20T10:00:00.000Z", "userType": "external", "isSidechain": false})
}

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    let mut r = env(uuid, parent);
    r["type"] = json!("user");
    r["message"] = json!({"role": "user", "content": [{"type": "text", "text": text}]});
    r
}

fn assistant(uuid: &str, parent: &str, text: &str) -> Value {
    let mut r = env(uuid, Some(parent));
    r["type"] = json!("assistant");
    r["message"] = json!({"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
        "type": "message", "stop_reason": "end_turn", "content": [{"type": "text", "text": text}]});
    r
}

fn tool_use(uuid: &str, parent: &str, id: &str, name: &str, input: Value) -> Value {
    let mut r = env(uuid, Some(parent));
    r["type"] = json!("assistant");
    r["message"] = json!({"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
        "type": "message", "stop_reason": "tool_use",
        "content": [{"type": "tool_use", "id": id, "name": name, "input": input}]});
    r
}

fn tool_result(uuid: &str, parent: &str, id: &str, text: &str, is_error: bool) -> Value {
    let mut r = env(uuid, Some(parent));
    r["type"] = json!("user");
    r["sourceToolAssistantUUID"] = json!(parent);
    r["message"] = json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": id, "content": text, "is_error": is_error}]});
    r
}

fn attachment(uuid: &str, parent: &str, kind: &str, rendered: Option<&str>) -> Value {
    let mut r = env(uuid, Some(parent));
    r["type"] = json!("attachment");
    r["attachment"] = json!({"type": kind});
    if let Some(t) = rendered {
        r["rendered"] = json!([{"content": t}]);
    }
    r
}

fn queued_human(uuid: &str, parent: &str, text: &str) -> Value {
    let mut r = env(uuid, Some(parent));
    r["type"] = json!("attachment");
    r["attachment"] = json!({"type": "queued_command", "commandMode": "prompt", "prompt": text,
        "origin": {"kind": "human"}});
    r["rendered"] = json!([{"content": format!("<system-reminder>\nThe user sent a new message while you were working:\n{text}\n</system-reminder>")}]);
    r
}

fn last_prompt(leaf: &str, text: &str) -> Value {
    json!({"type": "last-prompt", "leafUuid": leaf, "sessionId": SESSION, "lastPrompt": text})
}

fn write_session(dir: &Path, name: &str, records: &[Value]) -> PathBuf {
    let p = dir.join(format!("{name}.jsonl"));
    let mut f = fs::File::create(&p).unwrap();
    for r in records {
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
    p
}

fn load(p: &Path) -> Vec<Value> {
    fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn assemble(dir: &Path, src: &Path, summaries: &Value, extra: &[&str]) -> PathBuf {
    let sp = dir.join(format!("sums-{}.json", uuid_v4()));
    fs::write(&sp, summaries.to_string()).unwrap();
    let out = dir.join(format!("{}.jsonl", uuid_v4()));
    let mut args = vec![
        src.to_string_lossy().into_owned(),
        sp.to_string_lossy().into_owned(),
        "--out".into(),
        out.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    assert_eq!(cmd_assemble(&args), 0, "assemble must succeed");
    out
}

fn text(r: &Value) -> String {
    match r.pointer("/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ------------------------------------------------------------------------------------ accounting

#[test]
fn visible_chars_count_what_the_model_is_sent_and_nothing_else() {
    let mut a = assistant("a1", "u1", "0123456789");
    // Envelope bulk the model never sees.
    a["recompactProvenance"] = json!({"coveredUuids": vec!["x"; 500]});
    a["toolUseResult"] = json!("z".repeat(50_000));
    assert_eq!(visible_chars(&a), 10);

    // Prior-turn thinking is stripped before sending: zero.
    let mut t = assistant("a2", "a1", "ok");
    t["message"]["content"] = json!([{"type": "thinking", "thinking": "long reasoning ".repeat(500), "signature": "s".repeat(4000)},
                                     {"type": "text", "text": "ok"}]);
    assert_eq!(visible_chars(&t), 2);

    // Attachments count their rendered text only; unrendered ones (prompt snapshots) are free.
    assert_eq!(
        visible_chars(&attachment("x1", "a2", "output_style", Some("0123456789"))),
        10
    );
    let mut snap = attachment("x2", "x1", "prompt_snapshot", None);
    snap["attachment"]["systemPrompt"] = json!("p".repeat(400_000));
    assert_eq!(visible_chars(&snap), 0);
}

#[test]
fn calibration_takes_the_ratio_from_the_model_and_the_live_size_from_the_last_usage() {
    // Later prompts grow much faster than the visible transcript (preserved thinking, tool
    // schemas loading mid-session), so usage must not set the ratio — it reports the live size.
    let mut recs = vec![user("u0", None, "start")];
    let mut parent = "u0".to_string();
    let mut last = 0;
    for i in 0..6 {
        let uid = format!("a{i}");
        let mut a = assistant(&uid, &parent, &"w".repeat(4000));
        let prompt = 150_000 + 50_000 * i as u64;
        a["message"]["usage"] = json!({"input_tokens": 10, "cache_read_input_tokens": prompt - 10, "cache_creation_input_tokens": 0});
        recs.push(a);
        parent = uid;
        last = prompt as usize;
    }
    let c = calibrate(&recs);
    assert!(
        (1.0 / c.tokens_per_char - 2.4).abs() < 1e-9,
        "opus ratio from the table: {c:?}"
    );
    assert_eq!(
        c.overhead, DEFAULT_OVERHEAD,
        "history cannot tell the resume environment's overhead"
    );
    assert_eq!(c.live, Some(last));
    assert_eq!(
        c.current_tokens(&recs),
        last,
        "the compaction decision uses the live size"
    );
    assert_eq!(
        c.context_tokens(&recs),
        DEFAULT_OVERHEAD + c.conv_tokens(&recs)
    );

    // No usage: no live size.
    let bare: Vec<Value> = recs
        .iter()
        .map(|r| {
            let mut r = r.clone();
            if let Some(m) = r.get_mut("message").and_then(|m| m.as_object_mut()) {
                m.remove("usage");
            }
            r
        })
        .collect();
    assert!(calibrate(&bare).live.is_none());
    assert!((chars_per_token_for("claude-haiku-4-5-20251001") - 3.1).abs() < 1e-9);

    // Older transcripts lack `rendered`; their attachments still reached the model.
    let mut old = attachment("x9", "a5", "skill_listing", None);
    old["attachment"]["content"] = json!("s".repeat(900));
    assert!(visible_chars(&old) >= 900);
    let mut snap = attachment("x8", "a5", "prompt_snapshot", None);
    snap["attachment"]["systemPrompt"] = json!("p".repeat(9000));
    assert_eq!(visible_chars(&snap), 0);
}

// ------------------------------------------------------------------------------------ trimming

/// Old segment with ceremony + thinking + a mid-turn human message, then a tail segment.
fn ceremony_session() -> Vec<Value> {
    let mut think = assistant("a1", "u1", "reading");
    think["message"]["content"] = json!([{"type": "thinking", "thinking": "hmm", "signature": "sig"},
                                         {"type": "text", "text": "reading"}]);
    vec![
        user("u1", None, "look at the config"),
        attachment(
            "c1",
            "u1",
            "output_style",
            Some("<system-reminder>be concise</system-reminder>"),
        ),
        attachment(
            "c2",
            "c1",
            "instructions",
            Some(&"CLAUDE.md contents ".repeat(200)),
        ),
        think,
        tool_use(
            "a2",
            "a1",
            "t1",
            "Read",
            json!({"file_path": "/repo/config.toml"}),
        ),
        tool_result("r1", "a2", "t1", &"key = value\n".repeat(300), false),
        queued_human("q1", "r1", "lets not land anything yet, keep unmerged"),
        assistant("a3", "q1", "Understood, nothing lands yet."),
        user("u2", Some("a3"), "what next?"),
        attachment(
            "c3",
            "u2",
            "output_style",
            Some("<system-reminder>be concise</system-reminder>"),
        ),
        assistant("a4", "c3", "Next is the review."),
        last_prompt("a4", "what next?"),
    ]
}

#[test]
fn ceremony_drops_outside_the_tail_thinking_everywhere_and_mid_turn_human_messages_never() {
    let dir = tmp_dir();
    let src = write_session(&dir, SESSION, &ceremony_session());
    let out = load(&assemble(
        &dir,
        &src,
        &json!({"0": "I read /repo/config.toml."}),
        &[],
    ));
    let has = |u: &str| out.iter().any(|r| r["uuid"] == u);
    assert!(!has("c1") && !has("c2"), "old ceremony attachments dropped");
    assert!(has("c3"), "the tail keeps its attachments");
    assert!(
        has("q1"),
        "a message the human typed mid-turn survives summarization verbatim"
    );
    assert!(
        out.iter().all(|r| !serde_json::to_string(r)
            .unwrap()
            .contains("\"type\":\"thinking\"")),
        "persisted thinking is stripped from the twin"
    );
    // verify knows about mid-turn messages and passes when they survive.
    let twin = dir.join("twin-ok.jsonl");
    fs::write(
        &twin,
        out.iter().map(|r| r.to_string() + "\n").collect::<String>(),
    )
    .unwrap();
    assert_eq!(
        cmd_verify(&[
            twin.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned()
        ]),
        0
    );
    // ...and fails when one is lost.
    let lost: Vec<Value> = out.iter().filter(|r| r["uuid"] != "q1").cloned().collect();
    let mut prev: Option<String> = None;
    let rechained: Vec<Value> = lost
        .into_iter()
        .map(|mut r| {
            if let Some(u) = r.get("uuid").and_then(|v| v.as_str()).map(String::from) {
                r["parentUuid"] = prev.clone().map(Value::String).unwrap_or(Value::Null);
                prev = Some(u);
            }
            if r["type"] == "last-prompt" {
                r["leafUuid"] = json!(prev.clone());
            }
            r
        })
        .collect();
    let bad = dir.join("twin-bad.jsonl");
    fs::write(
        &bad,
        rechained
            .iter()
            .map(|r| r.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    assert_eq!(
        cmd_verify(&[
            bad.to_string_lossy().into_owned(),
            "--source".into(),
            src.to_string_lossy().into_owned()
        ]),
        1
    );
}

#[test]
fn own_skill_body_is_dropped_but_other_meta_records_stay() {
    let mut body = user("m1", Some("u1"), "Base directory for this skill: /x/plugins/segment_recompact/skills/recompact\n\n# recompact ...");
    body["isMeta"] = json!(true);
    let mut other = user(
        "m2",
        Some("m1"),
        "Base directory for this skill: /x/skills/figma-to-code\n\n# figma",
    );
    other["isMeta"] = json!(true);
    let recs = vec![
        user("u1", None, "/segment-recompact:recompact"),
        body,
        other,
        assistant("a1", "m2", "compacting"),
        last_prompt("a1", "/segment-recompact:recompact"),
    ];
    let dir = tmp_dir();
    let src = write_session(&dir, SESSION, &recs);
    let out = load(&assemble(&dir, &src, &json!({}), &[]));
    assert!(
        !out.iter().any(|r| r["uuid"] == "m1"),
        "the recompact procedure itself is dead weight"
    );
    assert!(
        out.iter().any(|r| r["uuid"] == "m2"),
        "other skills' instructions are left alone"
    );
}

// ------------------------------------------------------------------------------------ tail budget

#[test]
fn an_oversized_final_turn_keeps_only_its_newest_parts_verbatim() {
    let mut recs = vec![
        user("u1", None, "first ask"),
        assistant("a0", "u1", "done"),
        user("u2", Some("a0"), "continue with the long job"),
    ];
    let mut parent = "u2".to_string();
    for i in 0..40 {
        let (tu, tr) = (format!("t{i}"), format!("r{i}"));
        recs.push(tool_use(
            &tu,
            &parent,
            &format!("id{i}"),
            "Bash",
            json!({"command": format!("step {i}")}),
        ));
        recs.push(tool_result(
            &tr,
            &tu,
            &format!("id{i}"),
            &format!("OUTPUT-{i} {}", "x".repeat(6000)),
            false,
        ));
        parent = tr;
    }
    recs.push(assistant("final", &parent, "All forty steps ran."));
    recs.push(last_prompt("final", "continue with the long job"));
    let dir = tmp_dir();
    let src = write_session(&dir, SESSION, &recs);

    let (_, segs) = segment(&recs);
    let mut plans = plan(&recs, &segs, 1);
    let parts: Vec<Vec<Vec<usize>>> = segs
        .iter()
        .map(|s| split_parts(&recs, s, DEFAULT_SPLIT_THRESHOLD))
        .collect();
    apply_tail_budget(&recs, &segs, &mut plans, &parts, 20_000);
    let tail = plans.last().unwrap();
    assert!(
        !tail.kept_verbatim && !tail.pinned_parts.is_empty(),
        "tail over budget is split"
    );
    assert!(
        tail.pinned_parts
            .contains(&(parts.last().unwrap().len() - 1)),
        "the newest part is always kept"
    );

    let out_path = dir.join("masked.jsonl");
    assert_eq!(
        cmd_assemble(&[
            src.to_string_lossy().into_owned(),
            "--mode".into(),
            "mask".into(),
            "--tail-budget".into(),
            "20000".into(),
            "--out".into(),
            out_path.to_string_lossy().into_owned()
        ]),
        0
    );
    let out = load(&out_path);
    let blob = out.iter().map(|r| r.to_string()).collect::<String>();
    assert!(
        blob.contains("All forty steps ran."),
        "newest material verbatim"
    );
    assert!(blob.contains(&"x".repeat(6000)), "newest results verbatim");
    assert!(
        blob.contains("OUTPUT-0") && !blob.contains(&format!("OUTPUT-0 {}", "x".repeat(6000))),
        "the oldest part of the same turn is masked, not kept whole"
    );
    assert!(
        out.iter().any(|r| text(r) == "continue with the long job"),
        "the human turn itself is untouched"
    );
}

// ------------------------------------------------------------------------------------ carried evidence

#[test]
fn summaries_carry_files_errors_and_identifiers_the_future_uses() {
    let recs = vec![
        user("u1", None, "fix the deploy script"),
        tool_use(
            "a1",
            "u1",
            "e1",
            "Edit",
            json!({"file_path": "/repo/scripts/deploy.sh", "old_string": "a", "new_string": "b"}),
        ),
        tool_result("r1", "a1", "e1", "ok", false),
        tool_use(
            "a2",
            "r1",
            "b1",
            "Bash",
            json!({"command": "psql -c 'select contract_id from t'"}),
        ),
        tool_result(
            "r2",
            "a2",
            "b1",
            "ERROR: column \"contract_id\" does not exist (SQLSTATE 42703)",
            true,
        ),
        assistant(
            "a3",
            "r2",
            "The flag is `CROSSPOST_MAX_GAP_DAYS` and the revision `crosspost-worker-00011-ch4`.",
        ),
        user(
            "u2",
            Some("a3"),
            "now raise CROSSPOST_MAX_GAP_DAYS and redeploy crosspost-worker-00011-ch4",
        ),
        assistant("a4", "u2", "Raised."),
        last_prompt("a4", "now raise it"),
    ];
    let dir = tmp_dir();
    let src = write_session(&dir, SESSION, &recs);
    let out = load(&assemble(
        &dir,
        &src,
        &json!({"0": "I fixed the script and found the flag."}),
        &[],
    ));
    let synth = out
        .iter()
        .find(|r| r["recompactSynthetic"] == true)
        .expect("summary");
    let t = text(synth);
    assert!(
        t.contains("⟨carried⟩ files changed: `/repo/scripts/deploy.sh`"),
        "{t}"
    );
    assert!(t.contains("SQLSTATE 42703"), "error text verbatim: {t}");
    assert!(
        t.contains("`CROSSPOST_MAX_GAP_DAYS`") && t.contains("`crosspost-worker-00011-ch4`"),
        "identifiers a later turn uses are carried: {t}"
    );
    let uuid = synth["uuid"].as_str().unwrap();
    assert!(
        t.ends_with(&format!("[recompact summary 0 · recall {}]", &uuid[..8])),
        "{t}"
    );
}

#[test]
fn inherited_footers_are_upgraded_to_the_project_wide_selector() {
    let mut old = assistant(
        "5a5a5a5a-0000-4000-8000-000000000000",
        "u1",
        "Earlier work.\n[recompact summary 3.1 — rehydratable]",
    );
    old["recompactSynthetic"] = json!(true);
    old["recompactProvenance"] = json!({"source": "/gone.jsonl", "sourceSessionId": "gone", "part": "3.1", "coveredUuids": ["zz"]});
    let recs = vec![
        user("u1", None, "hi"),
        old,
        user("u2", Some("5a5a5a5a-0000-4000-8000-000000000000"), "next"),
        assistant("a2", "u2", "ok"),
        last_prompt("a2", "next"),
    ];
    let dir = tmp_dir();
    let src = write_session(&dir, SESSION, &recs);
    let out = load(&assemble(&dir, &src, &json!({}), &[]));
    let t = text(
        out.iter()
            .find(|r| r["recompactSynthetic"] == true)
            .unwrap(),
    );
    assert!(
        t.ends_with("Earlier work.\n[recompact summary 3.1 · recall 5a5a5a5a]"),
        "{t}"
    );
    assert!(
        !t.contains("rehydratable"),
        "old footer replaced, not stacked: {t}"
    );
}

#[test]
fn a_unit_without_agent_activity_is_summarized_mechanically() {
    let recs = vec![
        user("u1", None, "hello"),
        {
            let mut s = env("s1", Some("u1"));
            s["type"] = json!("system");
            s["content"] = json!("resume scaffolding");
            s
        },
        user("u2", Some("s1"), "now work"),
        assistant("a2", "u2", "working"),
        last_prompt("a2", "now work"),
    ];
    let dir = tmp_dir();
    let src = write_session(&dir, SESSION, &recs);
    // No summary supplied for unit 0: it has nothing an agent did, so none is required.
    let out = load(&assemble(&dir, &src, &json!({}), &[]));
    let t = text(
        out.iter()
            .find(|r| r["recompactSynthetic"] == true)
            .expect("mechanical summary"),
    );
    assert!(t.starts_with(EMPTY_UNIT_SUMMARY), "{t}");
}

// ------------------------------------------------------------------------------------ orientation

fn work_session() -> Vec<Value> {
    let mut recs = vec![
        user(
            "u1",
            None,
            "ship the fix. never push to main directly, always open a PR",
        ),
        tool_use(
            "a1",
            "u1",
            "w1",
            "Write",
            json!({"file_path": "/repo/src/fix.rs", "content": "fn fix() {}"}),
        ),
        tool_result("r1", "a1", "w1", "ok", false),
        tool_use(
            "a2",
            "r1",
            "b1",
            "Bash",
            json!({"command": "cargo test --release"}),
        ),
        tool_result(
            "r2",
            "a2",
            "b1",
            "test result: ok. 12 passed; 0 failed",
            false,
        ),
        tool_use(
            "a3",
            "r2",
            "b2",
            "Bash",
            json!({"command": "git checkout -b fix-branch && git commit -am 'fix it'"}),
        ),
        tool_result(
            "r3",
            "a3",
            "b2",
            "[fix-branch 1a2b3c4] fix it\n 1 file changed",
            false,
        ),
        tool_use(
            "a4",
            "r3",
            "b3",
            "Bash",
            json!({"command": "gh pr create --fill"}),
        ),
        tool_result("r4", "a4", "b3", "https://github.com/o/r/pull/5074", false),
        tool_use(
            "a5",
            "r4",
            "b4",
            "Bash",
            json!({"command": "gh pr view 5074 --json state"}),
        ),
        tool_result("r5", "a5", "b4", "{\"state\":\"OPEN\"}", false),
        assistant("a6", "r5", "PR #5074 is open."),
        user("u2", Some("a6"), "ok, wait for review"),
        assistant("a7", "u2", "Waiting."),
        last_prompt("a7", "ok, wait for review"),
    ];
    for r in recs.iter_mut() {
        if r["type"] == "assistant" {
            r["message"]["usage"] = json!({"input_tokens": 250000, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0});
        }
    }
    let mut effort = user("e1", Some("a7"), "<local-command-stdout>Set effort level to max (this session only): deepest</local-command-stdout>");
    effort["isMeta"] = json!(true);
    recs.insert(recs.len() - 1, effort);
    recs.push(json!({"type": "custom-title", "customTitle": "Deploy fix", "sessionId": SESSION}));
    recs
}

#[test]
fn the_preamble_briefs_state_and_pins_standing_instructions_verbatim() {
    let dir = tmp_dir();
    let src = write_session(&dir, SESSION, &work_session());
    let out = load(&assemble(
        &dir,
        &src,
        &json!({"0": "I shipped the fix."}),
        &[],
    ));
    let pre = out.iter().find(|r| r["recompactPreamble"] == true).unwrap();
    let t = text(pre);
    assert!(t.contains("State when compacted"), "{t}");
    assert!(
        t.contains("`/repo/src/fix.rs` (written)"),
        "files from code, not prose: {t}"
    );
    assert!(t.contains("1a2b3c4 \"fix it\""), "commits: {t}");
    assert!(t.contains("`fix-branch`"), "branches: {t}");
    assert!(t.contains("#5074 open"), "PR state as last observed: {t}");
    assert!(
        t.contains("`cargo test --release` → exit 0"),
        "last check: {t}"
    );
    assert!(
        t.contains("\"never push to main directly, always open a PR\""),
        "standing instruction verbatim: {t}"
    );
    assert!(pre["recompactGeneration"] == 1 && pre["recompactAssembledAt"].is_i64());
    // The title rides along with a generation suffix.
    assert!(out
        .iter()
        .any(|r| r["type"] == "custom-title" && r["customTitle"] == "Deploy fix (recompact 1)"));
    // Resume flags: the model (1M: usage ran past 200k) and the session-only effort.
    let flags = resume_flags(&work_session());
    assert_eq!(
        flags,
        vec!["--model", "claude-opus-5-5[1m]", "--effort", "max"]
    );
    assert_eq!(
        resume_command("abc", &flags),
        "claude --resume abc --model 'claude-opus-5-5[1m]' --effort max"
    );
}

#[test]
fn session_start_hook_is_silent_for_plain_sessions_and_orients_twins() {
    let dir = tmp_dir();
    let plain = write_session(&dir, SESSION, &work_session());
    let input = |p: &Path, source: &str| json!({"source": source, "transcript_path": p.to_string_lossy(), "session_id": "x"});
    assert!(
        session_start_context(&input(&plain, "resume")).is_none(),
        "not a twin: say nothing"
    );

    let twin = assemble(&dir, &plain, &json!({"0": "I shipped the fix."}), &[]);
    assert!(
        session_start_context(&input(&twin, "startup")).is_none(),
        "only resumes are oriented"
    );
    let ctx = session_start_context(&input(&twin, "resume")).expect("a twin is oriented");
    assert!(ctx.starts_with(ORIENT_MARKER), "{ctx}");
    assert!(ctx.contains("ago") && ctx.contains("generation 1"), "{ctx}");
    assert!(ctx.contains("recall"), "points at recall: {ctx}");
    // A stale orientation from an earlier resume is ceremony at the next compaction.
    let mut stale = attachment("h1", "u1", "hook_additional_context", None);
    stale["attachment"]["content"] = json!([ctx]);
    assert!(is_ceremony(&stale));
}

#[test]
fn identifier_extraction_is_conservative() {
    let mut ids = Vec::new();
    identifiers("see `lib/payouts/cycle.ts` and PR #5074 (sha 9f8e7d6c), env AUTHZ_PG_READ, https://x.io/a?b=1. The end is near.", &mut ids);
    for want in [
        "lib/payouts/cycle.ts",
        "#5074",
        "9f8e7d6c",
        "AUTHZ_PG_READ",
        "https://x.io/a?b=1",
    ] {
        assert!(ids.iter().any(|i| i == want), "missing {want}: {ids:?}");
    }
    for noise in ["The", "end", "near", "and"] {
        assert!(!ids.iter().any(|i| i == noise), "noise {noise}: {ids:?}");
    }
}
