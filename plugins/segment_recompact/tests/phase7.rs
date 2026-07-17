//! Growth-bound + handoff tests: epoch consolidation re-derives from RAW provenance (never from
//! summary text), unresolvable provenance stays verbatim, and the shell auto-cycles on an
//! agent-initiated SIGTERM exit while prompting on a human exit.

use recompact::*;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user(uuid: &str, parent: Option<&str>, text: &str, session: &str) -> Value {
    json!({
        "type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": session,
        "timestamp": "2026-07-17T00:00:00.000Z", "cwd": "/tmp/p", "version": "2.0.0",
        "userType": "external", "isSidechain": false,
        "message": {"role": "user", "content": text}
    })
}

fn assistant(uuid: &str, parent: &str, text: &str, session: &str) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": session,
        "timestamp": "2026-07-17T00:00:01.000Z", "userType": "external", "isSidechain": false,
        "message": {
            "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-4-7",
            "type": "message", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}]
        }
    })
}

fn synthetic(
    uuid: &str,
    parent: &str,
    text: &str,
    raw_path: &str,
    covered: &[&str],
    session: &str,
) -> Value {
    json!({
        "type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": session,
        "timestamp": "2026-07-17T00:00:02.000Z", "userType": "external", "isSidechain": false,
        "recompactSynthetic": true,
        "recompactProvenance": {
            "source": raw_path, "sourceSessionId": "gen0", "part": "1",
            "coveredUuids": covered.iter().map(|s| json!(s)).collect::<Vec<_>>()
        },
        "message": {
            "id": format!("msg_{uuid}"), "role": "assistant", "model": "claude-opus-4-7",
            "type": "message", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}]
        }
    })
}

fn last_prompt(leaf: &str, text: &str, session: &str) -> Value {
    json!({"type": "last-prompt", "leafUuid": leaf, "sessionId": session, "lastPrompt": text})
}

fn write_jsonl(path: &PathBuf, records: &[Value]) {
    let body: String = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap() + "\n")
        .collect();
    fs::write(path, body).unwrap();
}

fn write_stub(dir: &PathBuf, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

/// Summarizer stub that logs every prompt it receives, then answers every unit key.
fn logging_summarizer(dir: &PathBuf) -> String {
    write_stub(
        dir,
        "stub-claude.sh",
        r#"#!/bin/sh
D="$(dirname "$0")"
tmp="$D/prompt-$$.txt"
cat > "$tmp"
cat "$tmp" >> "$D/prompts.log"
keys=$(sed -n 's/^### UNIT //p' "$tmp")
printf '{'
first=1
for k in $keys; do
  [ $first -eq 1 ] || printf ','
  printf '"%s":"Epoch recap for unit %s."' "$k" "$k"
  first=0
done
printf '}\n'
"#,
    )
}

/// A twin session whose old segments hold synthetic summaries with provenance into `raw`, plus a
/// fat maskable tail turn so continue always has other material to work with.
fn twin_with_synthetics(dir: &PathBuf, session: &str, raw_path: &str) -> PathBuf {
    // Fat summaries: consolidation only fires when it actually saves tokens, so the fixture's
    // synthetics must cost more than an epoch recap (~400 tokens), like real ones do.
    let alpha = format!("OLD-SUMMARY-ALPHA about the widget. {}", "detail ".repeat(600));
    let beta = format!("OLD-SUMMARY-BETA about polish. {}", "nuance ".repeat(600));
    let records = vec![
        user("u1", None, "build the widget", session),
        synthetic("syn1", "u1", &alpha, raw_path, &["ra1", "ra2"], session),
        user("u2", Some("syn1"), "polish the widget", session),
        synthetic("syn2", "u2", &beta, raw_path, &["ra3"], session),
        user("u3", Some("syn2"), "now write the report", session),
        assistant("a3", "u3", &format!("REPORT {}", "verbose ".repeat(6000)), session),
        user("u4", Some("a3"), "thanks", session),
        assistant("a4", "u4", "done", session),
        last_prompt("a4", "thanks", session),
    ];
    let p = dir.join(format!("{session}.jsonl"));
    write_jsonl(&p, &records);
    p
}

fn gen0_raw(dir: &PathBuf) -> String {
    let session = "gen0";
    let records = vec![
        user("ru1", None, "build the widget", session),
        assistant("ra1", "ru1", "RAW-MARKER-ONE: I designed the widget frame.", session),
        assistant("ra2", "ra1", "RAW-MARKER-TWO: I welded the widget frame.", session),
        assistant("ra3", "ra2", "RAW-MARKER-THREE: I polished the widget.", session),
    ];
    let p = dir.join("gen0.jsonl");
    write_jsonl(&p, &records);
    p.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------------------- tests

#[test]
fn epoch_consolidation_rederives_from_raw_not_summaries() {
    let dir = tmp_dir();
    let raw = gen0_raw(&dir);
    twin_with_synthetics(&dir, "twin-a", &raw);
    let stub = logging_summarizer(&dir);

    let rc = cmd_continue(&[
        dir.join("twin-a.jsonl").to_string_lossy().into_owned(),
        "--threshold".into(),
        "1500".into(),
        "--summarize-with".into(),
        "stub-model".into(),
        "--claude-bin".into(),
        stub,
    ]);
    assert_eq!(rc, 0);

    let latest = lineage_latest(&dir, "twin-a");
    assert_ne!(latest, "twin-a");
    let out = fs::read_to_string(dir.join(format!("{latest}.jsonl"))).unwrap();
    assert!(
        !out.contains("OLD-SUMMARY-ALPHA") && !out.contains("OLD-SUMMARY-BETA"),
        "old synthetic summaries must be consolidated away"
    );
    assert!(out.contains("Epoch recap"), "epoch records assembled");

    let prompts = fs::read_to_string(dir.join("prompts.log")).unwrap();
    assert!(
        prompts.contains("RAW-MARKER-ONE") && prompts.contains("RAW-MARKER-THREE"),
        "epoch digests must be built from the RAW records"
    );
    assert!(
        !prompts.contains("OLD-SUMMARY-ALPHA"),
        "summary text must NEVER be fed to the summarizer"
    );
}

#[test]
fn unresolvable_provenance_stays_verbatim() {
    let dir = tmp_dir();
    twin_with_synthetics(&dir, "twin-b", "/nonexistent/gone.jsonl");
    let stub = logging_summarizer(&dir);

    let rc = cmd_continue(&[
        dir.join("twin-b.jsonl").to_string_lossy().into_owned(),
        "--threshold".into(),
        "1500".into(),
        "--summarize-with".into(),
        "stub-model".into(),
        "--claude-bin".into(),
        stub,
    ]);
    assert_eq!(rc, 0);

    let latest = lineage_latest(&dir, "twin-b");
    assert_ne!(latest, "twin-b", "the fat tail still compacts");
    let out = fs::read_to_string(dir.join(format!("{latest}.jsonl"))).unwrap();
    assert!(
        out.contains("OLD-SUMMARY-ALPHA") && out.contains("OLD-SUMMARY-BETA"),
        "synthetics with dead provenance must survive verbatim, never be re-summarized"
    );
}

#[test]
fn shell_sigterm_handoff_cycles_without_prompt() {
    let dir = tmp_dir();
    // Seed one session so newest_session finds something.
    let p = dir.join("seed.jsonl");
    write_jsonl(
        &p,
        &[
            user("u1", None, "hi", "seed"),
            assistant("a1", "u1", "hello", "seed"),
            last_prompt("a1", "hi", "seed"),
        ],
    );
    // First spawn exits 143 (agent handoff -> auto-cycle, no prompt read); second exits 0
    // (human exit -> prompt; stdin is at EOF in tests, so the shell quits instead of looping).
    let stub = write_stub(
        &dir,
        "stub-claude.sh",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "spawn $@" >> "$D/spawns.log"
n=$(wc -l < "$D/spawns.log")
[ "$n" -eq 1 ] && exit 143
exit 0
"#,
    );

    let rc = cmd_shell(&[
        "seed".into(),
        "--dir".into(),
        dir.to_string_lossy().into_owned(),
        "--threshold".into(),
        "999999999".into(),
        "--claude-bin".into(),
        stub,
    ]);
    assert_eq!(rc, 0);
    let spawns = fs::read_to_string(dir.join("spawns.log")).unwrap();
    assert_eq!(
        spawns.lines().count(),
        2,
        "143 must auto-cycle into a second spawn; exit 0 must stop at the prompt (EOF quits): {spawns}"
    );
}
