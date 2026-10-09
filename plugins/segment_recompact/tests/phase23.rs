//! Background-session mode: `RECOMPACT_JOBS=1` starts claude as a Claude Code background job and
//! attaches the terminal to it. Driven by stub claude binaries.

use recompact::*;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn write_stub(dir: &Path, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("claude-stub.sh");
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

/// Logs every call; `--bg` keeps a copy of its settings file and prints what the CLI prints;
/// `agents --json` prints `$AGENTS` from a file; `attach` exits 7.
fn job_stub(dir: &Path, agents: &str) -> String {
    fs::write(dir.join("agents.json"), agents).unwrap();
    write_stub(
        dir,
        r#"#!/bin/sh
D="$(dirname "$0")"
printf '%s\n' "$*" >> "$D/calls.log"
case "$1" in
  --bg) cp "$3" "$D/settings-copy.json"; echo "backgrounded · abcd1234 · test"; exit 0;;
  agents) cat "$D/agents.json"; exit 0;;
  attach) exit 7;;
esac
exit 3
"#,
    )
}

fn shell_args(dir: &Path, stub: &str, claude: &[&str]) -> Vec<String> {
    let mut a = s(&[
        "--jobs",
        "--interactive",
        "--state-root",
        dir.join("shell").to_str().unwrap(),
        "--claude-bin",
        stub,
    ]);
    a.extend(s(claude));
    a
}

#[test]
fn a_session_takes_the_terminals_variables_but_not_its_identity_or_settings_keys() {
    let vars = [
        ("HOME", "/h"),
        ("PATH", "/p"),
        ("MY_SECRET", "s"),
        ("TERM", "xterm"),
        ("PWD", "/x"),
        ("CLAUDE_CODE_SESSION_ID", "x"),
        ("GHOSTTY_RESOURCES_DIR", "/g"),
        ("RECOMPACT_SHELL", "/state"),
        ("ANTHROPIC_BASE_URL", "u"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()));
    let settings: HashSet<String> = ["ANTHROPIC_BASE_URL".to_string()].into();
    let env = job_env(vars, &settings);
    let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["HOME", "MY_SECRET", "PATH"]);
}

#[test]
fn a_picker_a_second_settings_file_and_tmux_stay_in_the_terminal() {
    let un = |a: &[&str]| job_unsupported(&parse_claude_args(&s(a)));
    assert_eq!(un(&["-r"]), Some("the session picker"));
    assert_eq!(un(&["--resume", "--model", "haiku"]), Some("the session picker"));
    assert_eq!(un(&["--resume", "abc"]), None);
    assert_eq!(un(&["--settings", "x.json"]), Some("--settings"));
    assert_eq!(un(&["--tmux"]), Some("--tmux"));
    assert_eq!(un(&["--model", "haiku", "fix the bug"]), None);
}

#[test]
fn claude_starts_in_the_background_and_this_terminal_attaches_to_it() {
    let dir = tmp_dir();
    let stub = job_stub(&dir, "[]");
    let mut args = shell_args(&dir, &stub, &["--model", "haiku"]);
    args.splice(0..0, s(&["--at", "123"]));
    assert_eq!(cmd_shell(&args), 7, "the attach's exit code");

    let calls = fs::read_to_string(dir.join("calls.log")).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    let env_dir = dir.join("job-env");
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].starts_with(&format!("--bg --settings {}", env_dir.display()))
            && calls[0].ends_with(" --model haiku"),
        "{calls:?}"
    );
    assert_eq!(calls[1], "attach abcd1234");

    let copy: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("settings-copy.json")).unwrap()).unwrap();
    let env = copy["env"].as_object().unwrap();
    assert!(env.contains_key("HOME") && env.contains_key("PATH"), "{env:?}");
    assert!(!env.contains_key("TERM") && !env.contains_key("PWD"), "{env:?}");
    assert_eq!(env["RECOMPACT_AT"], "123");

    use std::os::unix::fs::PermissionsExt;
    let file = fs::read_dir(&env_dir).unwrap().next().unwrap().unwrap().path();
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&env_dir).unwrap().permissions().mode() & 0o777, 0o700);
}

#[test]
fn resuming_a_session_that_already_runs_in_the_background_attaches_to_it() {
    let dir = tmp_dir();
    let stub = job_stub(
        &dir,
        r#"[{"id":"feedbeef","sessionId":"S1","kind":"background","pid":4242}]"#,
    );
    assert_eq!(cmd_shell(&shell_args(&dir, &stub, &["--resume", "S1"])), 7);
    let calls = fs::read_to_string(dir.join("calls.log")).unwrap();
    assert_eq!(calls, "agents --json\nattach feedbeef\n");
    assert!(!dir.join("job-env").exists(), "no new session, no settings file");
}
