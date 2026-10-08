//! What a background job's claude hears from recompact: never "run setup" or "open a new
//! terminal" (neither changes how the daemon starts jobs), the doctor's line for the session,
//! and compaction progress in the job's status line.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use recompact::*;
use serde_json::json;

const SESSION: &str = "5e5516a0-0000-4000-8000-000000000022";

/// One test, run in order: HOME and the job's variables are process-wide.
#[test]
fn a_background_job_hears_what_it_can_do_never_to_run_setup() {
    let home = std::env::temp_dir().join(format!("recompact-test-home-{}", uuid_v4()));
    let short = uuid_v4()[..8].to_string();
    let job_dir = home.join(".claude/jobs").join(&short);
    fs::create_dir_all(&job_dir).unwrap();
    fs::write(
        job_dir.join("state.json"),
        json!({"backend": "daemon", "daemonShort": short, "state": "done", "tempo": "idle",
               "cwd": "/tmp", "respawnFlags": ["--model", "opus"]})
        .to_string(),
    )
    .unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("SHELL", "/bin/zsh");
    std::env::set_var("RECOMPACT_HOME", home.join(".claude/recompact"));
    std::env::set_var("CLAUDE_JOB_DIR", &job_dir);
    std::env::set_var("CLAUDE_PID", "4242");
    std::env::set_var("CLAUDE_CODE_SESSION_ID", SESSION);
    std::env::remove_var("RECOMPACT_SHELL");
    let rc = home.join(".zshrc");
    let on = |shell| {
        on_prompt_in(
            shell,
            &json!({"session_id": SESSION, "prompt": "/recompact on"}),
        )
        .expect("answered")["reason"]
            .as_str()
            .unwrap()
            .to_string()
    };

    // Claude Code records sessions in a way recompact cannot read: it says so, and nothing about
    // setup.
    assert!(Shell::from_env().is_none());
    let gap = setup_gap(false).expect("a gap");
    assert!(gap.contains(&format!("background job {short}")), "{gap}");
    for text in [gap.clone(), on(None)] {
        assert!(
            !text.contains("setup") && !text.contains("new terminal"),
            "{text}"
        );
    }
    let (lines, _) = doctor_report(&home, Some(&rc));
    assert!(
        lines.iter().any(|l| l.starts_with("note  background job")),
        "{lines:?}"
    );

    // The job's own claude, as Claude Code records it: in place, through `claude attach`.
    fs::create_dir_all(home.join(".claude/sessions")).unwrap();
    fs::write(
        home.join(".claude/sessions/4242.json"),
        json!({"pid": 4242, "kind": "bg", "jobId": short, "sessionId": SESSION}).to_string(),
    )
    .unwrap();
    let shell = Shell::from_env().expect("the job");
    assert_eq!(shell.job(), Some(short.as_str()));
    assert!(setup_gap(true).is_none());
    assert!(setup_notice(&json!({"source": "startup", "session_id": SESSION})).is_none());
    let text = on(Shell::from_env());
    assert!(text.contains("compacts in place and carries on"), "{text}");
    assert!(
        !text.contains("setup") && !text.contains("terminal"),
        "{text}"
    );
    let (lines, _) = doctor_report(&home, Some(&rc));
    assert!(
        lines.contains(&format!(
            "ok    this claude session is background job {short}: it compacts in place through \
`claude attach`"
        )),
        "{lines:?}"
    );
    let setup = on_prompt_in(
        Shell::from_env(),
        &json!({"session_id": SESSION, "prompt": "/recompact setup"}),
    )
    .unwrap();
    assert!(
        setup["reason"]
            .as_str()
            .unwrap()
            .contains(&format!("Background job {short} needs none of this")),
        "{setup}"
    );

    // Compaction progress shows in the job's status line.
    let state = job_state_dir(&short);
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("progress.json"),
        json!({"phase": "summarizing", "done": 1, "total": 4, "pct": 25,
               "at": now_unix(), "started": now_unix()})
        .to_string(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_recompact"))
        .arg("statusline")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    assert!(out.contains("summarizing 1 of 4"), "{out}");
}
