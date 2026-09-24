//! The auto-compaction switch: `/recompact on`, `/recompact off`, `/recompact status`, handled
//! by the prompt hook without a model turn, persisted for every session.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use recompact::*;
use serde_json::{json, Value};

const SESSION: &str = "5e5517a0-0000-4000-8000-000000000001";

fn write_session(dir: &Path, prompt_tokens: u64) -> PathBuf {
    let p = dir.join(format!("{SESSION}.jsonl"));
    let mut f = fs::File::create(&p).unwrap();
    for r in [
        json!({"type": "user", "uuid": "u1", "parentUuid": null, "sessionId": SESSION,
               "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}),
        json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": SESSION,
               "message": {"id": "m1", "role": "assistant", "model": "claude-opus-5-5",
                           "content": [{"type": "text", "text": "hello"}],
                           "usage": {"input_tokens": 5, "cache_read_input_tokens": prompt_tokens - 5,
                                     "cache_creation_input_tokens": 0}}}),
    ] {
        writeln!(f, "{}", serde_json::to_string(&r).unwrap()).unwrap();
    }
    p
}

fn prompt(p: &str, transcript: &Path) -> Value {
    json!({"session_id": SESSION, "transcript_path": transcript, "prompt": p})
}

/// One test, run in order: the switch is process-wide state.
#[test]
fn the_switch_turns_auto_compaction_off_and_on_for_every_session() {
    let home = std::env::temp_dir().join(format!("recompact-switch-{}", uuid_v4()));
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("RECOMPACT_HOME", &home);
    let dir = home.join("proj");
    fs::create_dir_all(&dir).unwrap();
    let t = write_session(&dir, 450_000);

    // Handled by the hook, outside the launcher too, and never sent to the model.
    let off = on_prompt_in(None, &prompt("/recompact off", &t)).expect("handled");
    assert_eq!(off["decision"], "block");
    assert!(off["reason"].as_str().unwrap().contains("OFF"), "{off}");
    assert_eq!(user_settings()["auto"], false);

    // Off: a turn ending over the threshold under the launcher does nothing.
    let state = home.join("state");
    fs::create_dir_all(&state).unwrap();
    let config = json!({"launcher": std::process::id(), "child": 0, "auto": true});
    fs::write(state.join("config.json"), config.to_string()).unwrap();
    fs::write(
        state.join("session.json"),
        json!({"session": SESSION, "transcript": t}).to_string(),
    )
    .unwrap();
    let shell = || Shell {
        dir: state.clone(),
        config: config.clone(),
    };
    let stop = json!({"session_id": SESSION, "transcript_path": t, "background_tasks": [], "session_crons": []});
    assert!(on_stop_in(Some(shell()), &stop).is_none());
    assert!(!state.join("request.json").exists());

    // Status reports without changing anything.
    let st = on_prompt_in(Some(shell()), &prompt("/recompact status", &t)).unwrap();
    assert!(st["reason"].as_str().unwrap().contains("OFF"));
    assert!(st["reason"].as_str().unwrap().contains("450k"), "{st}");

    // On with a size: the next turn over it hands off.
    let on = on_prompt_in(Some(shell()), &prompt("/recompact on 300k", &t)).unwrap();
    let text = on["reason"].as_str().unwrap();
    assert!(text.contains("ON") && text.contains("300k"), "{text}");
    assert!(
        !text.contains("setup"),
        "this session runs under the launcher: {text}"
    );
    assert_eq!(user_settings()["at"], 300_000);
    assert!(on_stop_in(Some(shell()), &stop).is_some());
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(state.join("request.json")).unwrap())
            .unwrap()["reason"],
        "auto"
    );

    // Outside the launcher, turning it on says what else is needed.
    let bare = on_prompt_in(None, &prompt("/recompact on", &t)).unwrap();
    assert!(bare["reason"]
        .as_str()
        .unwrap()
        .contains("/recompact setup"));

    // Anything else is left to the skill.
    assert!(on_prompt_in(None, &prompt("/recompact onward", &t)).is_none());
    assert!(on_prompt_in(None, &prompt("/recompact on please", &t)).is_none());
    assert!(on_prompt_in(None, &prompt("turn /recompact off", &t)).is_none());
    assert_eq!(parse_size("1m"), Some(1_000_000));
    assert_eq!(parse_size("250K"), Some(250_000));
    assert_eq!(parse_size("12"), None);
}
