//! Every shipped check, both ways, against the REAL catalog.
//!
//! `signatures/README.md` rule 2 says "no signature without its negative".
//! This file is where that promise is kept: one positive fixture (the check
//! fires) and one negative fixture (it stays silent) per shipped check, in
//! the exact file shapes the vendors write — dotted TOML keys, quoted
//! sections, arrays of tables, JSONC, the nested Gemini schema. The last
//! test fails the build for any check that ships without its pair.
//!
//! The first build had none of these: all its scanner tests used a synthetic
//! `testagent`, and the parser bugs behind three wrong-direction findings
//! went unseen.

use stax_audit::{Posture, ScanContext, embedded_catalog, run_d1};
use std::collections::BTreeMap;
use std::collections::BTreeSet;

mod util;
use util::TempHome;

struct Fixture {
    check: &'static str,
    /// The agent's detect dir, created so the agent counts as present.
    detect: &'static str,
    file: &'static str,
    /// Artifact content on which the check fires `at_risk`.
    fires: &'static str,
    /// Artifact content on which the check is silent (protected).
    quiet: &'static str,
}

/// The maintainer's real `~/.grok/config.toml` shape, vetoes set. The first
/// reader refused the whole file at `[[marketplace.sources]]` and reported
/// every grok veto on that machine as "unreadable".
const GROK_VETOED: &str = "[cli]\nversion = 1\n\n[ui]\ntheme = \"dark\"\n\n[[marketplace.sources]]\nname = \"official\"\nurl = \"https://example.invalid/marketplace\"\n\n[models]\ndefault = \"grok-build\"\n\n[features]\ntelemetry = false\n\n[telemetry]\ntrace_upload = false\ndisable_codebase_upload = true\n";

/// The real Codex config shape: quoted project sections, and the veto in
/// dotted-key form — both beyond the first reader.
const CODEX_VETOED: &str = "analytics.enabled = false\notel = { log_user_prompt = false }\ncheck_for_update_on_startup = false\n\n[projects.\"/Users/x/repo\"]\ntrust_level = \"trusted\"\n";

const FIXTURES: &[Fixture] = &[
    Fixture {
        check: "grok.trace_upload",
        detect: ".grok",
        file: ".grok/config.toml",
        fires: "[features]\ntelemetry = true\n",
        quiet: GROK_VETOED,
    },
    Fixture {
        check: "grok.disable_codebase_upload",
        detect: ".grok",
        file: ".grok/config.toml",
        fires: "[telemetry]\ntrace_upload = false\n",
        quiet: GROK_VETOED,
    },
    Fixture {
        check: "grok.features_telemetry",
        detect: ".grok",
        file: ".grok/config.toml",
        fires: "[features]\ntelemetry = true\n[telemetry]\ntrace_upload = false\ndisable_codebase_upload = true\n",
        quiet: GROK_VETOED,
    },
    Fixture {
        check: "claude.usage_metrics",
        detect: ".claude",
        file: ".claude/settings.json",
        fires: "{\"env\": {\"DISABLE_ERROR_REPORTING\": \"1\"}}",
        quiet: "{\"env\": {\"DISABLE_TELEMETRY\": \"1\"}}",
    },
    Fixture {
        check: "claude.error_reports",
        detect: ".claude",
        file: ".claude/settings.json",
        fires: "{\"model\": \"opus\"}",
        quiet: "{\"env\": {\"DISABLE_ERROR_REPORTING\": \"1\"}}",
    },
    Fixture {
        check: "claude.transcript_survey",
        detect: ".claude",
        file: ".claude/settings.json",
        fires: "{}",
        quiet: "{\"env\": {\"CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY\": \"1\"}}",
    },
    Fixture {
        check: "claude.otel_telemetry",
        detect: ".claude",
        file: ".claude/settings.json",
        fires: "{\"env\": {\"CLAUDE_CODE_ENABLE_TELEMETRY\": \"1\"}}",
        quiet: "{\"env\": {\"DISABLE_TELEMETRY\": \"1\"}}",
    },
    Fixture {
        check: "claude.otel_user_prompts",
        detect: ".claude",
        file: ".claude/settings.json",
        fires: "{\"env\": {\"OTEL_LOG_USER_PROMPTS\": \"1\"}}",
        quiet: "{\"env\": {\"OTEL_LOG_USER_PROMPTS\": \"0\"}}",
    },
    Fixture {
        check: "gemini.usage_statistics",
        detect: ".gemini",
        file: ".gemini/settings.json",
        fires: "{\"ui\": {\"theme\": \"dark\"}, \"general\": {\"previewFeatures\": false}}",
        quiet: "{\"privacy\": {\"usageStatisticsEnabled\": false}}",
    },
    Fixture {
        check: "codex.analytics_enabled",
        detect: ".codex",
        file: ".codex/config.toml",
        fires: "[projects.\"/Users/x/repo\"]\ntrust_level = \"trusted\"\n",
        quiet: CODEX_VETOED,
    },
    Fixture {
        check: "codex.otel_log_user_prompt",
        detect: ".codex",
        file: ".codex/config.toml",
        fires: "[analytics]\nenabled = false\n[otel]\nlog_user_prompt = true\n",
        quiet: CODEX_VETOED,
    },
    Fixture {
        check: "codex.update_check",
        detect: ".codex",
        file: ".codex/config.toml",
        fires: "analytics.enabled = false\ncheck_for_update_on_startup = true\n",
        quiet: CODEX_VETOED,
    },
    Fixture {
        check: "copilot.remote_export",
        detect: ".copilot",
        file: ".copilot/settings.json",
        fires: "// Copilot CLI user settings\n{\n  \"remote\": \"off\",\n}\n",
        quiet: "{\"remoteExport\": false, \"remote\": \"off\"}",
    },
    Fixture {
        check: "copilot.remote",
        detect: ".copilot",
        file: ".copilot/settings.json",
        fires: "{\"remoteExport\": false, \"remote\": \"on\"}",
        quiet: "// settings\n{\"remoteExport\": false, \"remote\": \"off\",}",
    },
    Fixture {
        check: "copilot.legacy_analytics",
        detect: ".copilot",
        file: ".config/gh-copilot/config.yml",
        fires: "# gh-copilot\noptional_analytics: true\n",
        quiet: "optional_analytics: false\n",
    },
];

fn audit(detect: &str, file: &str, content: &str) -> stax_audit::AuditReport {
    let home = TempHome::new();
    home.mkdir(detect);
    home.write(file, content);
    run_d1(
        &embedded_catalog().expect("shipped catalog"),
        &ScanContext::new(home.path()),
    )
}

#[test]
fn every_check_fires_on_its_positive_fixture() {
    for fx in FIXTURES {
        let report = audit(fx.detect, fx.file, fx.fires);
        let hit = report
            .findings
            .iter()
            .find(|f| f.signature_id == fx.check)
            .unwrap_or_else(|| {
                panic!(
                    "{}: positive fixture produced no finding: {:?}",
                    fx.check, report.findings
                )
            });
        assert_eq!(hit.posture, Posture::AtRisk, "{}: {hit:?}", fx.check);
        assert!(
            hit.evidence.as_ref().is_some_and(|e| !e.snippet.is_empty()),
            "{}: an at-risk row carries its basis: {hit:?}",
            fx.check
        );
    }
}

#[test]
fn every_check_is_silent_on_its_negative_fixture() {
    for fx in FIXTURES {
        let report = audit(fx.detect, fx.file, fx.quiet);
        let leak: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.signature_id == fx.check)
            .collect();
        assert!(
            leak.is_empty(),
            "{}: the veto is set and the check still fired — the wrong-direction finding: {leak:?}",
            fx.check
        );
    }
}

#[test]
fn every_shipped_check_has_a_fixture_pair() {
    let catalog = embedded_catalog().unwrap();
    let covered: BTreeSet<&str> = FIXTURES.iter().map(|f| f.check).collect();
    for sig in &catalog {
        for check in &sig.checks {
            assert!(
                covered.contains(check.id.as_str()),
                "{} ships without a positive/negative fixture pair — add one here (signatures/README.md rule 2)",
                check.id
            );
        }
    }
    let shipped: BTreeSet<String> = catalog
        .iter()
        .flat_map(|s| s.checks.iter().map(|c| c.id.clone()))
        .collect();
    for fx in FIXTURES {
        assert!(
            shipped.contains(fx.check),
            "{} is not a shipped check",
            fx.check
        );
    }
}

#[test]
fn the_maintainers_real_grok_config_is_protected_not_unknown() {
    let report = audit(".grok", ".grok/config.toml", GROK_VETOED);
    let grok: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.provider == "grok")
        .collect();
    assert!(grok.is_empty(), "{grok:?}");
    let cov = report.coverage.iter().find(|c| c.agent == "grok").unwrap();
    assert_eq!((cov.protected, cov.unknown, cov.at_risk), (3, 0, 0));
}

#[test]
fn the_real_codex_config_shape_is_protected_not_unknown() {
    let report = audit(".codex", ".codex/config.toml", CODEX_VETOED);
    let codex: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.provider == "codex")
        .collect();
    assert!(codex.is_empty(), "{codex:?}");
    let cov = report.coverage.iter().find(|c| c.agent == "codex").unwrap();
    assert_eq!((cov.protected, cov.unknown), (3, 0));
}

#[test]
fn an_empty_agent_dir_says_why_it_is_at_risk() {
    // `mkdir ~/.grok` alone produces two critical findings — and each must
    // say the config file does not exist, or the screenshot is a lie.
    let home = TempHome::new();
    home.mkdir(".grok");
    let report = run_d1(&embedded_catalog().unwrap(), &ScanContext::new(home.path()));
    let critical: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.provider == "grok" && f.posture == Posture::AtRisk)
        .collect();
    assert_eq!(critical.len(), 2, "{critical:?}");
    for f in critical {
        let ev = f.evidence.as_ref().unwrap();
        assert!(ev.snippet.contains("not present"), "{ev:?}");
        assert_eq!(ev.path, "~/.grok/config.toml");
    }
}

#[test]
fn claude_vetoes_exported_in_the_shell_count() {
    // The reviewer's own machine: nothing in settings.json's env block, the
    // opt-outs exported from .zshrc. The first build reported at-risk.
    let home = TempHome::new();
    home.mkdir(".claude");
    home.write(".claude/settings.json", "{\"model\": \"opus\"}");
    let env: BTreeMap<String, String> = [
        ("DISABLE_TELEMETRY".to_string(), "1".to_string()),
        ("DISABLE_ERROR_REPORTING".to_string(), "1".to_string()),
        (
            "CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY".to_string(),
            "1".to_string(),
        ),
    ]
    .into_iter()
    .collect();
    let report = run_d1(
        &embedded_catalog().unwrap(),
        &ScanContext::new(home.path()).with_env(env),
    );
    let claude: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.provider == "claude")
        .collect();
    assert!(claude.is_empty(), "{claude:?}");
    let cov = report
        .coverage
        .iter()
        .find(|c| c.agent == "claude")
        .unwrap();
    assert_eq!(cov.protected, 3);
}

#[test]
fn the_nonessential_traffic_umbrella_vetoes_every_default_on_check() {
    let home = TempHome::new();
    home.mkdir(".claude");
    home.write(
        ".claude/settings.json",
        "{\"env\": {\"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC\": \"1\"}}",
    );
    let report = run_d1(&embedded_catalog().unwrap(), &ScanContext::new(home.path()));
    let claude: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.provider == "claude")
        .collect();
    assert!(claude.is_empty(), "{claude:?}");
}

#[test]
fn the_maintainers_real_claude_settings_audit_as_documented() {
    // DISABLE_ERROR_REPORTING and the survey opt-out set, usage metrics
    // still on: one low row, two vetoes counted, nothing invented.
    let home = TempHome::new();
    home.mkdir(".claude");
    home.write(
        ".claude/settings.json",
        "{\"env\": {\"DISABLE_ERROR_REPORTING\": \"1\", \"CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY\": \"1\"}, \"model\": \"opus\"}",
    );
    let report = run_d1(&embedded_catalog().unwrap(), &ScanContext::new(home.path()));
    let claude: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.provider == "claude")
        .collect();
    assert_eq!(claude.len(), 1, "{claude:?}");
    assert_eq!(claude[0].signature_id, "claude.usage_metrics");
    assert_eq!(claude[0].severity, stax_audit::Severity::Low);
    let cov = report
        .coverage
        .iter()
        .find(|c| c.agent == "claude")
        .unwrap();
    assert_eq!((cov.at_risk, cov.protected), (1, 2));
}

#[test]
fn the_nested_gemini_schema_and_the_legacy_flat_key_both_read() {
    let nested = audit(
        ".gemini",
        ".gemini/settings.json",
        "{\"privacy\": {\"usageStatisticsEnabled\": false}}",
    );
    assert!(
        nested.findings.iter().all(|f| f.provider != "gemini"),
        "{:?}",
        nested.findings
    );
    let flat = audit(
        ".gemini",
        ".gemini/settings.json",
        "{\"usageStatisticsEnabled\": false}",
    );
    assert!(
        flat.findings.iter().all(|f| f.provider != "gemini"),
        "{:?}",
        flat.findings
    );
    let on = audit(
        ".gemini",
        ".gemini/settings.json",
        "{\"privacy\": {\"usageStatisticsEnabled\": true}}",
    );
    assert!(
        on.findings
            .iter()
            .any(|f| f.signature_id == "gemini.usage_statistics")
    );
}
