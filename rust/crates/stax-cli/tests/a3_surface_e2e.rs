//! A3 — the public verb surface: five verbs + `advanced`, one screen.
//! Legacy invocations stay callable (hidden, not removed) — the byte-parity
//! differs and the installed skills depend on that.

use std::process::Command;

fn stax(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_stax"))
        .args(args)
        .env("COLUMNS", "100")
        .output()
        .expect("spawn stax")
}

#[test]
fn help_shows_exactly_the_public_surface() {
    let out = stax(&["--help"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for verb in ["audit", "cost", "replay", "start", "doctor", "advanced"] {
        assert!(
            stdout.contains(&format!("\n  {verb}")),
            "{verb} missing from --help:\n{stdout}"
        );
    }
    for demoted in ["memory", "ingest", "benchmark", "report", "context-replay"] {
        assert!(
            !stdout.contains(&format!("\n  {demoted}")),
            "{demoted} should be hidden from --help:\n{stdout}"
        );
    }

    // Pin the exact visible set — a straggler variant is a test failure, not
    // a surprise on launch day (`docs` slipped the first sweep exactly this way).
    let visible: Vec<&str> = stdout
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(
        visible,
        [
            "audit", "cost", "replay", "doctor", "start", "advanced", "help"
        ],
        "the public surface drifted:\n{stdout}"
    );
}

#[test]
fn advanced_lists_everything_including_hidden_verbs() {
    let out = stax(&["advanced"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for verb in ["memory", "ingest", "report", "context-replay", "etl"] {
        assert!(
            stdout.contains(verb),
            "{verb} missing from the directory:\n{stdout}"
        );
    }
}

#[test]
fn advanced_executes_a_demoted_verb() {
    let out = stax(&["advanced", "report", "--help"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("date range"), "got:\n{stdout}");
}

#[test]
fn demoted_verbs_still_work_at_top_level() {
    // Hidden ≠ removed: the parity differs and skills invoke these directly.
    let out = stax(&["report", "--help"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn cost_is_report_and_replay_is_context_replay() {
    let cost = stax(&["cost", "--help"]);
    assert_eq!(cost.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&cost.stdout).contains("date range"));

    let replay = stax(&["replay", "--help"]);
    assert_eq!(replay.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&replay.stdout).contains("SESSION_ID"));
}
