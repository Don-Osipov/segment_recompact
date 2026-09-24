//! Setup that tells the user when it did not take: `/recompact setup` from inside claude, and a
//! once-a-day notice when a session cannot compact in place.

use std::fs;

use recompact::*;
use serde_json::json;

/// One test, run in order: HOME, SHELL, and RECOMPACT_HOME are process-wide.
#[test]
fn a_session_that_cannot_compact_in_place_says_why_and_setup_fixes_it() {
    let home = std::env::temp_dir().join(format!("recompact-setup-{}", uuid_v4()));
    fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("SHELL", "/bin/zsh");
    std::env::remove_var("ZDOTDIR");
    std::env::remove_var("RECOMPACT_SHELL");
    std::env::set_var("RECOMPACT_HOME", home.join(".claude/recompact"));
    let start = json!({"session_id": "s1", "source": "startup"});

    // Not set up: the notice names the fix, once a day.
    let n = setup_notice(&start).expect("notice");
    assert!(n.contains("/recompact setup"), "{n}");
    assert!(setup_notice(&start).is_none(), "once a day");
    assert!(setup_notice(&json!({"source": "clear"})).is_none());

    // /recompact setup, handled by the hook without the model.
    let out = on_prompt_in(
        None,
        &json!({"session_id": "s1", "prompt": "/recompact setup"}),
    )
    .expect("handled");
    assert_eq!(out["decision"], "block");
    assert!(
        out["reason"]
            .as_str()
            .unwrap()
            .contains("open a new terminal"),
        "{out}"
    );
    assert!(fs::read_to_string(home.join(".zshrc"))
        .unwrap()
        .contains(">>> recompact >>>"));

    // Set up, but this claude was not started through the wrapper.
    let gap = setup_gap(false).unwrap();
    assert!(gap.contains("Open a new terminal"), "{gap}");
    assert!(
        setup_gap(true).is_none(),
        "under the launcher: nothing to say"
    );

    // Removed on purpose: no more notices.
    assert_eq!(cmd_uninstall(&[]), 0);
    assert!(setup_gap(false).is_none());

    // ZDOTDIR moves zsh's startup file; macOS bash reads the first login file that exists.
    let zdot = home.join("zdot");
    fs::create_dir_all(&zdot).unwrap();
    std::env::set_var("ZDOTDIR", &zdot);
    let (ok, _) = install(None);
    assert!(
        ok && fs::read_to_string(zdot.join(".zshrc"))
            .unwrap()
            .contains(">>> recompact >>>")
    );
    std::env::remove_var("ZDOTDIR");
    if cfg!(target_os = "macos") {
        std::env::set_var("SHELL", "/bin/bash");
        fs::write(home.join(".profile"), "export A=1\n").unwrap();
        let (ok, lines) = install(None);
        assert!(ok, "{lines:?}");
        assert!(fs::read_to_string(home.join(".profile"))
            .unwrap()
            .contains(">>> recompact >>>"));
        assert!(
            !home.join(".bashrc").exists(),
            "macOS login shells never read .bashrc"
        );
    }
}
