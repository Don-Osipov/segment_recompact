//! Claude Code's own compaction (near the window's end, or after ~54 idle minutes since 2.1.299)
//! must not replace a session recompact manages; a typed /compact still runs.
use recompact::*;
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const S: &str = "5e5517a0-0000-4000-8000-0000000000c1";

fn write_session(dir: &Path, prompt_tokens: u64) -> PathBuf {
    let p = dir.join(format!("{S}.jsonl"));
    let mut f = fs::File::create(&p).unwrap();
    for r in [
        json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": S,
               "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}),
        json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": S,
               "message": {"id": "m1", "role": "assistant", "model": "claude-opus-5-5",
                           "content": [{"type": "text", "text": "hello"}],
                           "usage": {"input_tokens": 5, "cache_read_input_tokens": prompt_tokens - 5,
                                     "cache_creation_input_tokens": 0}}}),
    ] {
        writeln!(f, "{}", serde_json::to_string(&r).unwrap()).unwrap();
    }
    p
}

fn launcher(home: &Path, transcript: &Path, auto: bool) -> Shell {
    let dir = home.join(format!("state-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    let config = json!({"launcher": std::process::id(), "child": 0, "auto": auto});
    fs::write(dir.join("config.json"), config.to_string()).unwrap();
    fs::write(dir.join("session.json"), json!({"session": S, "transcript": transcript}).to_string()).unwrap();
    Shell { dir, config }
}

fn pre_compact(shell: &Shell, trigger: &str, transcript: &Path) -> Option<String> {
    on_pre_compact_in(
        Some(Shell { dir: shell.dir.clone(), config: shell.config.clone() }),
        &json!({"session_id": S, "transcript_path": transcript, "trigger": trigger}),
    )
}

#[test]
fn claude_codes_own_compaction_yields_to_recompact_in_sessions_it_manages() {
    let home = std::env::temp_dir().join(format!("recompact-precompact-{}", uuid_v4()));
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("RECOMPACT_HOME", &home);
    std::env::remove_var("RECOMPACT_WINDOW");
    std::env::remove_var("CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS");

    // Idle at 300k, under the 500k size: blocked, nothing requested.
    let t = write_session(&home, 300_000);
    let on = launcher(&home, &t, true);
    let why = pre_compact(&on, "auto", &t).expect("blocked");
    assert!(why.contains("manages"), "{why}");
    assert!(!on.dir.join("request.json").exists());

    // A typed /compact is the user's call.
    assert!(pre_compact(&on, "manual", &t).is_none());

    // Over the size: blocked, and nothing requested here; the turn's end compacts (a request
    // from this hook could stop claude mid-turn).
    let t = write_session(&home, 650_000);
    let on = launcher(&home, &t, true);
    assert!(pre_compact(&on, "auto", &t).is_some());
    assert!(!on.dir.join("request.json").exists());

    // Near the window's end Claude Code's compaction runs rather than stall the session.
    let t = write_session(&home, 950_000);
    let on = launcher(&home, &t, true);
    assert!(pre_compact(&on, "auto", &t).is_none());

    // Under 200k it may be a small window filling up: let it run, even with the idle minimum lowered.
    std::env::set_var("CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS", "100000");
    let t = write_session(&home, 150_000);
    let on = launcher(&home, &t, true);
    assert!(pre_compact(&on, "auto", &t).is_none());

    // Sessions with auto-compaction off keep Claude Code's behavior.
    let t = write_session(&home, 300_000);
    let off = launcher(&home, &t, false);
    assert!(pre_compact(&off, "auto", &t).is_none());

    // Outside the launcher and background jobs, nothing changes.
    assert!(on_pre_compact_in(None, &json!({"session_id": S, "transcript_path": t, "trigger": "auto"})).is_none());
    fs::remove_dir_all(&home).ok();
}
