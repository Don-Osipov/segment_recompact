//! The auto-compaction switch, per session: `/recompact on|off|status` for this session,
//! `/recompact default on|off` for new ones. Answered by the prompt hook without a model turn.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use recompact::*;
use serde_json::{json, Value};

const A: &str = "5e5517a0-0000-4000-8000-00000000000a";
const B: &str = "5e5517a0-0000-4000-8000-00000000000b";

fn write_session(dir: &Path, id: &str, prompt_tokens: u64) -> PathBuf {
    let p = dir.join(format!("{id}.jsonl"));
    let mut f = fs::File::create(&p).unwrap();
    for r in [
        json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": id,
               "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}),
        json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": id,
               "message": {"id": "m1", "role": "assistant", "model": "claude-opus-5-5",
                           "content": [{"type": "text", "text": "hello"}],
                           "usage": {"input_tokens": 5, "cache_read_input_tokens": prompt_tokens - 5,
                                     "cache_creation_input_tokens": 0}}}),
    ] {
        writeln!(f, "{}", serde_json::to_string(&r).unwrap()).unwrap();
    }
    p
}

fn say(id: &str, p: &str, transcript: &Path) -> String {
    let out = on_prompt_in(
        None,
        &json!({"session_id": id, "transcript_path": transcript, "prompt": p}),
    )
    .unwrap_or_else(|| panic!("{p} is handled by the hook"));
    assert_eq!(out["decision"], "block", "never reaches the model");
    out["reason"].as_str().unwrap().to_string()
}

/// A launcher state dir tracking `id`, as its SessionStart hook would leave it.
fn launcher(home: &Path, id: &str, transcript: &Path) -> Shell {
    let dir = home.join(format!("state-{id}"));
    fs::create_dir_all(&dir).unwrap();
    let config = json!({"launcher": std::process::id(), "child": 0});
    fs::write(dir.join("config.json"), config.to_string()).unwrap();
    fs::write(
        dir.join("session.json"),
        json!({"session": id, "transcript": transcript}).to_string(),
    )
    .unwrap();
    Shell { dir, config }
}

fn stop(shell: &Shell, id: &str, transcript: &Path) -> Option<Value> {
    on_stop_in(
        Some(Shell {
            dir: shell.dir.clone(),
            config: shell.config.clone(),
        }),
        &json!({"session_id": id, "transcript_path": transcript, "background_tasks": [], "session_crons": []}),
    )
}

fn requested(shell: &Shell) -> bool {
    shell.dir.join("request.json").exists()
}

/// One test, run in order: the settings are process-wide state.
#[test]
fn auto_compaction_is_off_by_default_and_switched_per_session() {
    let home = std::env::temp_dir().join(format!("recompact-switch-{}", uuid_v4()));
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("RECOMPACT_HOME", &home);
    let ta = write_session(&home, A, 550_000);
    let tb = write_session(&home, B, 550_000);
    let (la, lb) = (launcher(&home, A, &ta), launcher(&home, B, &tb));

    // Off unless asked: a turn ending way over the size does nothing.
    assert!(stop(&la, A, &ta).is_none() && !requested(&la));
    assert!(say(A, "/recompact status", &ta).contains("OFF"));

    // On for session A only. Asked for explicitly, so no warning turn.
    let on = say(A, "/recompact on", &ta);
    assert!(on.contains("ON for this session"), "{on}");
    assert!(stop(&la, A, &ta).is_some() && requested(&la));
    assert!(
        stop(&lb, B, &tb).is_none() && !requested(&lb),
        "B is untouched"
    );

    // Off again for A.
    fs::remove_file(la.dir.join("request.json")).unwrap();
    assert!(say(A, "/recompact off", &ta).contains("OFF for this session"));
    assert!(stop(&la, A, &ta).is_none() && !requested(&la));

    // On with a size of its own.
    assert!(say(A, "/recompact on 600k", &ta).contains("600k"));
    assert!(stop(&la, A, &ta).is_none(), "550k is under 600k");

    // Default on: sessions without a setting follow it, with the warning turn first; A keeps
    // its own setting.
    let d = say(B, "/recompact default on", &tb);
    assert!(
        d.contains("new sessions now start with auto-compaction ON"),
        "{d}"
    );
    let warn = stop(&lb, B, &tb).expect("warned");
    assert!(warn["systemMessage"]
        .as_str()
        .unwrap()
        .contains("next turn"));
    assert!(!requested(&lb));
    assert!(stop(&lb, B, &tb).is_some() && requested(&lb));
    assert!(say(A, "/recompact status", &ta).contains("at 600k"));
    say(B, "/recompact default off", &tb);
    assert!(say(B, "/recompact status", &tb).contains("New sessions start OFF"));

    // Outside the launcher, turning a session on says what else it needs.
    assert!(say(B, "/recompact on", &tb).contains("/recompact setup"));

    // Anything else is the skill's.
    for p in [
        "/recompact onward",
        "/recompact on please",
        "turn /recompact off",
        "/recompact default",
    ] {
        assert!(
            on_prompt_in(None, &json!({"session_id": A, "prompt": p})).is_none(),
            "{p}"
        );
    }
    // A session switched on stays on through its handoffs: the launcher carries the setting to
    // the twin.
    let proj = home.join("proj");
    fs::create_dir_all(&proj).unwrap();
    let big = "x".repeat(600_000);
    let src = proj.join(format!("{A}.jsonl"));
    let mut f = fs::File::create(&src).unwrap();
    for r in [
        json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": A,
               "message": {"role": "user", "content": [{"type": "text", "text": "read it"}]}}),
        json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": A,
               "message": {"id": "m1", "role": "assistant", "model": "claude-opus-5-5", "stop_reason": "tool_use",
                   "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/big"}}]}}),
        json!({"type": "user", "uuid": "r1", "parentUuid": "a1", "sessionId": A, "sourceToolAssistantUUID": "a1",
               "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": big}]}}),
        json!({"type": "assistant", "uuid": "a2", "parentUuid": "r1", "sessionId": A,
               "message": {"id": "m2", "role": "assistant", "model": "claude-opus-5-5", "content": [{"type": "text", "text": "read"}]}}),
        json!({"type": "user", "uuid": "u2", "parentUuid": "a2", "sessionId": A,
               "message": {"role": "user", "content": [{"type": "text", "text": "thanks"}]}}),
        json!({"type": "assistant", "uuid": "a3", "parentUuid": "u2", "sessionId": A,
               "message": {"id": "m3", "role": "assistant", "model": "claude-opus-5-5", "content": [{"type": "text", "text": "done"}]}}),
        json!({"type": "last-prompt", "leafUuid": "a3", "sessionId": A, "lastPrompt": "thanks"}),
    ] {
        writeln!(f, "{}", serde_json::to_string(&r).unwrap()).unwrap();
    }
    drop(f);
    say(A, "/recompact on", &ta);
    let stub = proj.join("claude-stub.sh");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nD=\"$(dirname \"$0\")\"\necho x >> \"$D/spawns.log\"\n\
[ \"$(wc -l < \"$D/spawns.log\")\" -eq 1 ] || exit 0\n\
printf '{{\"session\":\"{A}\",\"transcript\":\"{}\",\"reason\":\"manual\",\"force\":true,\"ready\":true}}' > \"$RECOMPACT_SHELL/request.json\"\n\
trap 'exit 143' TERM\nsleep 30 & wait $!\n",
            src.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let args: Vec<String> = [
        "--interactive",
        "--mask",
        "--dir",
        proj.to_str().unwrap(),
        "--state-root",
        home.join("shells").to_str().unwrap(),
        "--claude-bin",
        stub.to_str().unwrap(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(cmd_shell(&args), 0);
    let twin = lineage_latest(&proj, A);
    assert_ne!(twin, A);
    let carried: Value = serde_json::from_str(
        &fs::read_to_string(home.join("sessions").join(format!("{twin}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(carried["auto"], true);

    assert_eq!(parse_size("1m"), Some(1_000_000));
    assert_eq!(parse_size("250K"), Some(250_000));
    assert_eq!(parse_size("12"), None);
}
