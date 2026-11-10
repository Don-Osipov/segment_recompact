//! Summarizer input: a unit too large for the digest budget is thinned from the middle by
//! priority, and its ask and its ending always reach the summarizer.

use recompact::*;
use serde_json::{json, Value};

const SESSION: &str = "sess-digest";

fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-11-09T00:00:00.000Z", "cwd": "/tmp/p", "userType": "external",
        "isSidechain": false, "message": {"role": "user", "content": text}
    })
}

fn assistant_text(uuid: &str, parent: &str, text: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-11-09T00:00:01.000Z", "userType": "external", "isSidechain": false,
        "message": {"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
            "type": "message", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}]}
    })
}

fn bash(uuid: &str, parent: &str, id: &str, cmd: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-11-09T00:00:02.000Z", "userType": "external", "isSidechain": false,
        "message": {"id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-5-5",
            "type": "message", "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": cmd}}]}
    })
}

fn result(uuid: &str, parent: &str, id: &str, text: &str, is_error: bool) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-11-09T00:00:03.000Z", "userType": "external", "isSidechain": false,
        "sourceToolAssistantUUID": parent,
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": text, "is_error": is_error}
        ]}
    })
}

/// One turn with `steps` command/result pairs, an error early on, and a closing report.
fn long_turn(steps: usize) -> Vec<Value> {
    let mut v = vec![user("u1", None, "find why the nightly export drops rows")];
    let mut parent = "u1".to_string();
    v.push(assistant_text("a0", &parent, "I will start with the export job's logs."));
    parent = "a0".into();
    for i in 0..steps {
        let (a, r, id) = (format!("a{}", i + 1), format!("r{}", i + 1), format!("t{i}"));
        v.push(bash(&a, &parent, &id, &format!("grep -n step{i} export.log")));
        let text = if i == 2 {
            "ERROR: relation \"export_rows_v2\" does not exist".to_string()
        } else {
            format!("step{i} ok ").repeat(40)
        };
        v.push(result(&r, &a, &id, &text, i == 2));
        parent = r;
    }
    v.push(assistant_text(
        "close",
        &parent,
        "Root cause: the export reads export_rows_v2, which the 11-02 migration renamed. \
         Fixed the query; the rerun exported 18,204 rows. Left unfinished: the backfill for 11-03.",
    ));
    v
}

fn digest_of(records: &[Value]) -> String {
    let u = build_units(records, 0, 1_000_000);
    unit_digest(records, &u.segs[0], &u.seg_parts[0][0])
}

#[test]
fn small_unit_digest_is_whole() {
    let recs = long_turn(5);
    let d = digest_of(&recs);
    assert!(d.starts_with("USER ASKED: find why the nightly export drops rows"));
    assert!(d.contains("grep -n step4 export.log"));
    assert!(!d.contains("elided to fit"));
    assert!(d.trim_end().ends_with("Left unfinished: the backfill for 11-03."));
}

#[test]
fn oversized_unit_keeps_its_ask_its_error_and_its_ending() {
    let recs = long_turn(400);
    let d = digest_of(&recs);
    assert!(d.chars().count() <= DIGEST_CAP, "digest {} chars", d.chars().count());
    assert!(d.starts_with("USER ASKED: find why the nightly export drops rows"));
    assert!(d.contains("thinned to fit") && d.contains(" elided]"), "the thinning is marked");
    assert!(d.contains("export_rows_v2\\\" does not exist") || d.contains("export_rows_v2\" does not exist"),
        "errors outlast routine results");
    assert!(d.contains("Left unfinished: the backfill for 11-03."), "the ending survives");
    assert!(d.contains("grep -n step399 export.log"), "the newest steps stay");
    assert!(!d.contains("step10 ok"), "old routine results go first");
}

#[test]
fn fit_digest_drops_by_priority_and_counts_markers() {
    let mut lines = vec![(LINE_KEEP, "USER ASKED: x".to_string())];
    for i in 0..50 {
        lines.push((LINE_PROSE, format!("ASSISTANT: reasoning {i}")));
        lines.push((LINE_CALL, format!("TOOL Bash {}", "c".repeat(60))));
        lines.push((LINE_ROUTINE, format!("RESULT[ok,9]: {}", "r".repeat(120))));
    }
    lines.push((LINE_KEEP, "ASSISTANT: done".to_string()));
    let d = fit_digest(&lines, 3000);
    assert!(d.chars().count() <= 3000, "{} chars", d.chars().count());
    assert!(d.starts_with("USER ASKED: x"));
    assert!(d.ends_with("ASSISTANT: done"));
    assert!(d.contains("ASSISTANT: reasoning 0"), "prose outlasts calls and results");
    assert!(!d.contains("RESULT[ok,9]: rrrr") || d.matches("RESULT[ok,9]").count() <= 3);
}

// ------------------------------------------------------------------ orientation brief fixes

#[test]
fn recent_asks_quote_the_person_not_the_harness() {
    let recs = vec![user(
        "u1",
        None,
        "<system-reminder> You are operating in a git worktree. Worktree path: /x </system-reminder>\nCheck the Strix claim on PR #9216 and fix it if real",
    )];
    let brief = state_brief(&recs).join("\n");
    assert!(brief.contains("\"Check the Strix claim on PR #9216 and fix it if real\""), "{brief}");
    assert!(!brief.contains("system-reminder"));
}

#[test]
fn standing_instructions_keep_file_names_whole() {
    let recs = vec![user(
        "u1",
        None,
        "Never change `lib/api/core/team-request-access.ts` without a test. Do not merge.",
    )];
    let lane = constraint_lane(&recs);
    assert!(
        lane.iter().any(|q| q.contains("team-request-access.ts` without a test")),
        "{lane:?}"
    );
    assert!(lane.iter().any(|q| q == "\"Do not merge\""), "{lane:?}");
}

#[test]
fn last_check_ignores_test_runners_named_in_heredocs() {
    let recs = vec![
        user("u1", None, "ship it"),
        bash("a1", "u1", "t1", "npm run test:run -- lib/x.test.ts 2>&1 | tail -5"),
        result("r1", "a1", "t1", "Test Files  1 passed (1)", false),
        bash(
            "a2",
            "r1",
            "t2",
            "cat > /tmp/pr.md <<'EOF'\n## Test plan\n- npx vitest run lib/x.test.ts\nEOF",
        ),
        result("r2", "a2", "t2", "", false),
        bash("a3", "r2", "t3", "git commit -m \"run cargo test before merging\""),
        result("r3", "a3", "t3", "[main 1234567] run cargo test", false),
        bash("a4", "r3", "t4", "gh pr checks 7886 --json name --jq '.[]|select(.name|test(\"lint|eslint\"))'"),
        result("r4", "a4", "t4", "lint", false),
        bash("a5", "r4", "t5", "echo \"- opened #7995; npm test passes\" >> board.md"),
        result("r5", "a5", "t5", "", false),
    ];
    let c = last_check(&recs).expect("a check ran");
    assert!(c.starts_with("`npm run test:run"), "{c}");
}

#[test]
fn last_check_sees_runners_behind_cd_env_and_npx() {
    let recs = vec![
        user("u1", None, "check"),
        bash("a1", "u1", "t1", "cd plugins/x && RUST_LOG=warn cargo test --release 2>&1 | tail -3"),
        result("r1", "a1", "t1", "test result: ok. 3 passed", false),
        bash("a2", "r1", "t2", "npx tsc --noEmit -p ."),
        result("r2", "a2", "t2", "error TS2322: Type 'string' is not assignable", true),
    ];
    let c = last_check(&recs).expect("a check ran");
    assert!(c.starts_with("`npx tsc --noEmit"), "{c}");
    assert!(c.contains("FAILED"), "{c}");
}

#[test]
fn scope_and_process_instructions_are_standing_instructions() {
    let recs = vec![user(
        "u1",
        None,
        "Fix the payout rounding. Leave the iOS side alone. Ask me before you merge anything. \
         Wait for CI before pushing a second commit. Should we skip the backfill?",
    )];
    let lane = constraint_lane(&recs);
    for want in [
        "\"Leave the iOS side alone\"",
        "\"Ask me before you merge anything\"",
        "\"Wait for CI before pushing a second commit\"",
    ] {
        assert!(lane.iter().any(|q| q == want), "missing {want}: {lane:?}");
    }
    assert!(!lane.iter().any(|q| q.contains("payout rounding")), "a task is not a rule: {lane:?}");
    assert!(!lane.iter().any(|q| q.contains("backfill")), "a question is not a rule: {lane:?}");
}

// ------------------------------------------------------------------ request-replay records

fn replay_blob(id: &str) -> Value {
    // Claude Code 2.1.300+ stores each request's messages by hash: no uuid, copies of tool calls.
    json!({"type": "api-request-blob", "sessionId": SESSION, "hash": format!("h-{id}"),
        "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": "ls"}}]}})
}

#[test]
fn request_replay_records_neither_block_splits_nor_reach_the_twin() {
    let mut recs = vec![user("u1", None, "audit the export")];
    let mut parent = "u1".to_string();
    for i in 0..40 {
        let (a, r, id) = (format!("a{i}"), format!("r{i}"), format!("t{i}"));
        recs.push(bash(&a, &parent, &id, "cat export.log"));
        recs.push(result(&r, &a, &id, &"x".repeat(4000), false));
        recs.push(replay_blob(&id));
        parent = r;
    }
    recs.push(json!({"type": "api-request-shape", "sessionId": SESSION, "shapeHash": "s1",
        "shape": {"system": [{"type": "text", "text": "sys"}], "tools": []}}));
    let (active, dropped) = select_active(recs);
    assert_eq!(dropped, 0, "replay records are not abandoned branches");
    assert!(active.iter().all(|r| !rec_type(r).starts_with("api-request")));
    let u = build_units(&active, 0, 5_000);
    assert!(u.seg_parts[0].len() > 4, "split points survive: {} part(s)", u.seg_parts[0].len());
}

// ------------------------------------------------------------------ parallel tool results

#[test]
fn parallel_call_results_off_the_parent_chain_are_kept() {
    // Claude Code chains the second call of a batch off the first call record, so the first
    // call's result is a childless sibling of the path.
    let mut a1 = bash("a1", "u1", "t1", "cat a.txt");
    a1["message"]["id"] = json!("msg_batch");
    let mut a2 = bash("a2", "a1", "t2", "cat b.txt");
    a2["message"]["id"] = json!("msg_batch");
    let recs = vec![
        user("u1", None, "read both files"),
        a1,
        a2,
        result("r1", "a1", "t1", "contents of a", false),
        result("r2", "a2", "t2", "contents of b", false),
        assistant_text("a3", "r2", "Both files read."),
    ];
    let (active, dropped) = select_active(recs);
    let ids: Vec<&str> = active.iter().filter_map(|r| rec_uuid(r)).collect();
    assert_eq!(ids, ["u1", "a1", "a2", "r1", "r2", "a3"]);
    assert_eq!(dropped, 0);
}

#[test]
fn an_abandoned_branch_result_is_not_reattached() {
    // A rewound retry: the call on the path is answered on the path; an older result for a
    // call that is not on the path stays off it.
    let recs = vec![
        user("u1", None, "run it"),
        bash("a0", "u1", "t0", "old attempt"),
        result("r0", "a0", "t0", "old output", false),
        bash("a1", "u1", "t1", "make test"),
        result("r1", "a1", "t1", "ok", false),
        assistant_text("a2", "r1", "Done."),
    ];
    let (active, _) = select_active(recs);
    let ids: Vec<&str> = active.iter().filter_map(|r| rec_uuid(r)).collect();
    assert_eq!(ids, ["u1", "a1", "r1", "a2"]);
}

// ------------------------------------------------------------------ recall after a switch

#[test]
fn recall_follows_a_switch_in_place_to_the_live_twin() {
    let dir = std::env::temp_dir().join(format!("recompact-live-{}", uuid_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let parent = "11111111-1111-4111-8111-111111111111";
    let twin = "22222222-2222-4222-8222-222222222222";
    std::fs::write(dir.join(format!("{parent}.jsonl")), "{}\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out = dir.join(format!("{twin}.jsonl"));
    std::fs::write(&out, "{}\n").unwrap();
    lineage_record(&dir, parent, twin, &out);
    let ctx = RecallCtx { dir: dir.clone(), session: Some(parent.to_string()) };
    assert_eq!(ctx.live().session.as_deref(), Some(twin));
    let none = RecallCtx { dir: dir.clone(), session: None };
    assert_eq!(none.live().session, None);
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------------------------ images in mid-turn messages

fn queued_with_image(uuid: &str, parent: &str) -> Value {
    json!({"type": "attachment", "uuid": uuid, "parentUuid": parent, "sessionId": SESSION,
        "timestamp": "2026-11-09T00:00:04.000Z", "isSidechain": false,
        "attachment": {"type": "queued_command", "commandMode": "prompt", "prompt": [
            {"type": "text", "text": "look at this screenshot"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A".repeat(600_000)}}
        ]}})
}

#[test]
fn a_screenshot_typed_mid_turn_counts_as_an_image_and_is_elided_outside_the_tail() {
    let q = queued_with_image("q1", "a1");
    let chars = visible_chars(&q);
    assert!(chars < 10_000, "base64 must not count as text: {chars}");
    let elided = match trim_record(&q, true) {
        Masked::Replaced(v) => v,
        _ => panic!("outside the tail the image goes"),
    };
    assert_eq!(human_queued_text(&elided), "look at this screenshot");
    assert!(serde_json::to_string(&elided).unwrap().len() < 2_000);
    assert!(matches!(trim_record(&q, false), Masked::Unchanged), "the tail keeps it");
}

// ------------------------------------------------------------------ replies that answer by reference

#[test]
fn short_replies_that_answer_by_reference_are_recognised() {
    for r in ["yes", "yeah go ahead", "option 2", "#4 please",
              "let's go with B", "what do I need to know for items 2 and 7"] {
        assert!(refers_back(r), "{r}");
    }
    for r in ["check PR #9216 for the Strix claim", "use v1.2 of the schema",
              "ok so now write the migration for the payouts table, then run the census and open a PR with the results, and make sure the backfill is idempotent",
              "fix the login bug", "write section 3", "do 1 and 3"] {
        assert!(!refers_back(r), "{r}");
    }
}

#[test]
fn the_closing_message_is_carried_when_the_next_turn_answers_it() {
    let options = "Three ways to fix the export:\n1. Rename the table back (fast, risky)\n2. Point the query at export_rows_v3 (safe)\n3. Add a view named export_rows_v2 (no code change)\nWhich one?";
    let recs = vec![
        user("u1", None, "how should we fix the export"),
        assistant_text("a1", "u1", options),
        user("u2", Some("a1"), "option 2"),
    ];
    let (_, segs) = segment(&recs);
    let carried = closing_for_reply(&recs, &segs[0].activity, "option 2").expect("carried");
    assert!(carried.contains("2. Point the query at export_rows_v3 (safe)"));
    assert!(closing_for_reply(&recs, &segs[0].activity, "fix the login bug").is_none());
    // Bare numbers count when the message lists those items, and only then.
    assert!(closing_for_reply(&recs, &segs[0].activity, "do 1 and 3").is_some());
    assert!(closing_for_reply(&recs, &segs[0].activity, "2").is_some());
    assert!(closing_for_reply(&recs, &segs[0].activity, "write section 7").is_none());
}

// ------------------------------------------------------------------ review follow-ups

#[test]
fn last_check_sees_runners_behind_launchers_and_survives_comments_and_dashed_heredocs() {
    for (cmd, want) in [
        ("uv run pytest -x tests/", true),
        ("python3 -m pytest -q", true),
        ("yarn vitest run", true),
        ("if cargo test; then echo ok; fi", true),
        ("# don't run this twice\ncargo test --release", true),
        ("cat > /tmp/n.md <<END-OF-NOTE\nrun pytest later\nEND-OF-NOTE\ngit status", false),
        ("echo done # cargo test later", false),
    ] {
        let recs = vec![
            user("u1", None, "check"),
            bash("a1", "u1", "t1", cmd),
            result("r1", "a1", "t1", "ok", false),
        ];
        assert_eq!(last_check(&recs).is_some(), want, "{cmd}");
    }
}

#[test]
fn closing_marks_after_a_period_still_end_the_sentence() {
    let recs = vec![user("u1", None, "**Do not merge.** Run the tests first, then tell me.")];
    let lane = constraint_lane(&recs);
    assert!(lane.iter().any(|q| q == "\"**Do not merge\""), "{lane:?}");
}

#[test]
fn a_slash_commands_arguments_are_the_persons_words() {
    let t = "<command-message>ship-it is running…</command-message>\n<command-name>/ship-it</command-name>\n<command-args>build the payouts export, never merge without CI</command-args>";
    let w = human_words(t);
    assert!(w.contains("build the payouts export, never merge without CI"), "{w}");
    assert!(!w.contains("ship-it is running"));
}

#[test]
fn an_earlier_summary_is_never_carried_as_a_closing_message() {
    let mut syn = assistant_text("s1", "u1", &"I summarized the earlier work. ".repeat(20));
    syn["recompactSynthetic"] = json!(true);
    let recs = vec![user("u1", None, "keep going"), syn, user("u2", Some("s1"), "yes")];
    let (_, segs) = segment(&recs);
    assert!(closing_for_reply(&recs, &segs[0].activity, "yes").is_none());
}

#[test]
fn under_epochs_only_this_tools_summaries_leave_the_tail_pin() {
    let mut ledger = assistant_text("l1", "u1", "LEDGER: decisions so far");
    ledger["recompactLedger"] = json!(true);
    let mut syn = assistant_text("s1", "r0", &"SUMMARY ".repeat(200));
    syn["recompactSynthetic"] = json!(true);
    let raw = assistant_text("r0", "l1", &"raw ".repeat(3000));
    let mut recs = vec![user("u1", None, "keep going"), ledger, raw, syn];
    let mut parent = "s1".to_string();
    for i in 0..6 {
        let id = format!("a{i}");
        recs.push(assistant_text(&id, &parent, &"work ".repeat(4000)));
        parent = id;
    }
    let (_, segs) = segment(&recs);
    let mut plans = plan_ex(&recs, &segs, 1, true);
    let parts: Vec<Vec<Vec<usize>>> = segs.iter().map(|s| split_parts(&recs, s, 4_000)).collect();
    apply_tail_budget(&recs, &segs, &mut plans, &parts, 3_000, true);
    let pinned = &plans[0].pinned_parts;
    let holds = |want: &str| parts[0].iter().position(|p| p.iter().any(|&i| rec_uuid(&recs[i]) == Some(want))).unwrap();
    assert!(pinned.contains(&holds("l1")), "the ledger stays: {pinned:?}");
    assert!(!pinned.contains(&holds("s1")), "own summaries may consolidate: {pinned:?}");
}
