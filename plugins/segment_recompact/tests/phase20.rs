//! Compaction progress in claude's status line: the launcher's progress feed, the row it
//! becomes, the user's own status line kept above it (and not re-run on every timer tick), and
//! the settings edit that sets it up.

use std::fs;

use recompact::*;
use serde_json::{json, Value};

fn tmp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("recompact-test-{}", uuid_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The row as it reads, without its colors.
fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        match (esc, c) {
            (false, '\x1b') => esc = true,
            (true, 'm') => esc = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    out
}

#[test]
fn the_feed_fills_in_the_percentage_and_keeps_what_the_launcher_set() {
    let dir = tmp_dir();
    let feed = dir.join("progress.json");
    progress_update(json!({"phase": "reading"}));
    assert!(
        !feed.exists(),
        "nothing is written unless the launcher asks"
    );
    set_progress_file(Some(feed.clone()));
    progress_update(json!({"phase": "reading", "live": 612_000, "started": 100}));
    progress("summarizing", 4, 8);
    let v: Value = serde_json::from_str(&fs::read_to_string(&feed).unwrap()).unwrap();
    set_progress_file(None);
    assert_eq!(v["phase"], "summarizing");
    assert_eq!(v["pct"], 45);
    assert_eq!(v["live"], 612_000, "kept");
    assert_eq!(v["started"], 100);
    assert!(v["at"].as_i64().unwrap() > 0);
    let pcts: Vec<usize> = [
        ("reading", 0, 0),
        ("summarizing", 0, 10),
        ("summarizing", 10, 10),
        ("assembling", 0, 0),
        ("verifying", 0, 0),
        ("waiting", 0, 0),
        ("switching", 0, 0),
        ("done", 0, 0),
    ]
    .iter()
    .map(|(p, d, t)| progress_pct(p, *d, *t))
    .collect();
    assert!(
        pcts.windows(2).all(|w| w[0] < w[1]),
        "only ever forward: {pcts:?}"
    );
    assert_eq!(*pcts.last().unwrap(), 100);
}

#[test]
fn the_row_says_where_the_compaction_is() {
    let now = 1_000;
    let row = |v: Value| render_progress(&v, now, 120).map(|r| plain(&r));
    let summarizing = row(
        json!({"phase": "summarizing", "done": 8, "total": 13, "pct": 54,
                                 "live": 612_000, "started": now - 12, "at": now}),
    )
    .unwrap();
    assert!(summarizing.contains("recompact"), "{summarizing}");
    assert!(summarizing.contains(" 54%"), "{summarizing}");
    assert!(
        summarizing.contains("summarizing 8 of 13 · 612k · 12s"),
        "{summarizing}"
    );
    assert!(
        summarizing.contains('━') && summarizing.contains('─'),
        "a bar: {summarizing}"
    );
    let waiting = row(
        json!({"phase": "waiting", "wait": "turn", "pct": 96, "live": 612_000,
                             "est": 95_000, "at": now}),
    )
    .unwrap();
    assert!(
        waiting.contains("ready · switches when this turn ends · 612k → ~95k"),
        "{waiting}"
    );
    let typing = row(json!({"phase": "waiting", "wait": "typing", "pct": 96, "at": now})).unwrap();
    assert!(typing.contains("once the input box is empty"), "{typing}");
    let done =
        row(json!({"phase": "done", "live": 612_000, "est": 95_000, "at": now - 3})).unwrap();
    assert_eq!(done, "✓ recompact · compacted in place · 612k → ~95k");
    assert!(
        row(json!({"phase": "done", "at": now - 30})).is_none(),
        "gone after a while"
    );
    assert!(row(json!({"phase": "noop", "at": now}))
        .unwrap()
        .contains("nothing to compact"));
    assert!(row(json!({"phase": "something else", "at": now})).is_none());
}

#[test]
fn the_users_status_line_stays_and_is_not_rerun_on_every_tick() {
    let dir = tmp_dir();
    let counter = dir.join("runs");
    let command = format!("echo run >> '{}'; echo 'my line'", counter.display());
    let input = |cost: f64, ms: u64| {
        json!({"session_id": "s1", "cost": {"total_cost_usd": cost, "total_duration_ms": ms}})
            .to_string()
    };
    let runs = || {
        fs::read_to_string(&counter)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let cache = dir.join("cache");

    let idle = status_line(Some(&command), &input(1.0, 1000), &cache, None, 50, 120);
    assert_eq!(idle, "my line\n", "nothing added while no compaction runs");
    status_line(Some(&command), &input(1.0, 2000), &cache, None, 51, 120);
    assert_eq!(runs(), 1, "a timer tick reuses the last output");
    status_line(Some(&command), &input(1.5, 3000), &cache, None, 52, 120);
    assert_eq!(runs(), 2, "new input runs it again");

    let p = json!({"phase": "assembling", "pct": 88, "at": 52});
    let busy = status_line(Some(&command), &input(1.5, 4000), &cache, Some(&p), 52, 120);
    let lines: Vec<String> = busy.lines().map(plain).collect();
    assert_eq!(lines[0], "my line");
    assert!(lines[1].contains("88%") && lines[1].contains("building the compacted copy"));
    assert_eq!(runs(), 2);
    assert!(status_line(None, "{}", &cache, None, 52, 120).is_empty());
}

#[test]
fn setup_wraps_the_users_status_line_without_touching_the_rest_and_undoes_it() {
    let text = r#"{
  "model": "opus",
  "statusLine": {
    "type": "command",
    "command": "bash ~/.claude/status line.sh 'x'",
    "padding": 1
  },
  "hooks": {"Stop": []}
}
"#;
    let bin = "$HOME/.claude/recompact/bin/recompact";
    let (wrapped, original) = with_statusline(text, bin).unwrap().expect("wrapped");
    let v: Value = serde_json::from_str(&wrapped).unwrap();
    let sl = &v["statusLine"];
    assert_eq!(sl["refreshInterval"], 1);
    assert_eq!(sl["padding"], 1);
    assert_eq!(
        sl["command"],
        wrapped_command(bin, "bash ~/.claude/status line.sh 'x'")
    );
    assert!(wrapped.starts_with("{\n  \"model\": \"opus\",\n  \"statusLine\": "));
    assert!(
        wrapped.ends_with(",\n  \"hooks\": {\"Stop\": []}\n}\n"),
        "{wrapped}"
    );
    assert!(with_statusline(&wrapped, bin).unwrap().is_none(), "once");
    let restored = without_statusline(&wrapped, &original).unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&restored).unwrap(),
        serde_json::from_str::<Value>(text).unwrap()
    );
    assert!(without_statusline(text, &original).unwrap().is_none());
    assert!(
        with_statusline("{\"model\": \"opus\"}", bin)
            .unwrap()
            .is_none(),
        "none to wrap"
    );
}

#[test]
fn the_wrapped_command_runs_the_original_when_recompact_is_gone() {
    let dir = tmp_dir();
    let cmd = wrapped_command(
        &dir.join("missing").display().to_string(),
        "cat > /dev/null; echo 'it is me'",
    );
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "it is me\n");
}

#[test]
fn the_bar_eases_forward_while_a_wave_of_summaries_runs() {
    let at = |secs: i64| {
        let p = json!({"phase": "summarizing", "done": 0, "next": 5, "total": 5, "pct": 5,
                       "step_at": 1_000, "at": 1_000});
        let row = plain(&render_progress(&p, 1_000 + secs, 120).unwrap());
        row.split('%')
            .next()
            .unwrap()
            .split_whitespace()
            .last()
            .unwrap()
            .parse::<usize>()
            .unwrap()
    };
    let seen: Vec<usize> = [0, 2, 5, 10, 20, 60].iter().map(|&s| at(s)).collect();
    assert_eq!(seen[0], 5);
    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "never backward: {seen:?}"
    );
    assert!(seen[3] > 40, "well along after 10s: {seen:?}");
    assert!(
        *seen.last().unwrap() < progress_pct("summarizing", 5, 5),
        "short of the landing: {seen:?}"
    );
}
