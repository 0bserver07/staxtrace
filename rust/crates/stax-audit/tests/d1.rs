//! B2 — the D1 config scanner: one test per behavior class.
//!
//! Every test runs against a synthetic home directory; nothing reads the real
//! `$HOME` (CI must never depend on the machine it runs on).

use stax_audit::{Posture, ScanContext, Severity, catalog_from_str, run_d1};
use std::collections::BTreeMap;

mod util;
use util::TempHome;

fn sig(json: &str) -> Vec<stax_audit::AgentSignature> {
    vec![catalog_from_str(json).expect("test signature must be valid")]
}

const JSON_AGENT: &str = r#"{
  "agent": "testagent",
  "version": 1,
  "detect_dirs": ["~/.testagent"],
  "checks": [
    {
      "id": "testagent.telemetry",
      "file": "~/.testagent/settings.json",
      "format": "json",
      "key": "telemetry.enabled",
      "uploading_when": [true],
      "safe_when": [false],
      "at_risk_when_unset": false,
      "title": "testagent uploads telemetry",
      "veto": "set telemetry.enabled=false",
      "severity": "high"
    }
  ]
}"#;

#[test]
fn fires_on_uploading_value() {
    let home = TempHome::new();
    home.write(
        ".testagent/settings.json",
        r#"{"telemetry": {"enabled": true}}"#,
    );
    let report = run_d1(&sig(JSON_AGENT), &ScanContext::new(home.path()));
    assert_eq!(report.findings.len(), 1);
    let f = &report.findings[0];
    assert_eq!(f.severity, Severity::High);
    assert_eq!(f.posture, Posture::AtRisk);
    assert_eq!(
        f.remediation.as_deref(),
        Some("set telemetry.enabled=false")
    );
    let ev = f.evidence.as_ref().expect("at_risk carries evidence");
    assert!(
        ev.path.starts_with("~/"),
        "home is printed as ~: {}",
        ev.path
    );
    assert!(ev.snippet.contains("true"));
}

#[test]
fn silent_on_safe_value() {
    let home = TempHome::new();
    home.write(
        ".testagent/settings.json",
        r#"{"telemetry": {"enabled": false}}"#,
    );
    let report = run_d1(&sig(JSON_AGENT), &ScanContext::new(home.path()));
    assert!(
        report.findings.is_empty(),
        "the negative pass: {:?}",
        report.findings
    );
    let cov = &report.coverage[0];
    assert!(cov.detected);
    assert_eq!(cov.protected, 1);
}

#[test]
fn unset_key_is_silent_unless_signature_says_otherwise() {
    let home = TempHome::new();
    home.write(".testagent/settings.json", r#"{"other": 1}"#);
    let report = run_d1(&sig(JSON_AGENT), &ScanContext::new(home.path()));
    assert!(report.findings.is_empty());
}

#[test]
fn missing_veto_file_fires_when_unset_means_at_risk() {
    // The Grok shape (Spec 28 §0.3): an absent config file is an absent veto.
    let grok_like = JSON_AGENT.replace(
        r#""at_risk_when_unset": false"#,
        r#""at_risk_when_unset": true"#,
    );
    let home = TempHome::new();
    home.mkdir(".testagent"); // agent detected, config never written
    let report = run_d1(&sig(&grok_like), &ScanContext::new(home.path()));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].posture, Posture::AtRisk);
    assert!(
        report.findings[0]
            .evidence
            .as_ref()
            .unwrap()
            .snippet
            .contains("unset")
    );
}

#[test]
fn out_of_range_value_reports_unknown_never_safe() {
    let home = TempHome::new();
    home.write(
        ".testagent/settings.json",
        r#"{"telemetry": {"enabled": "sometimes"}}"#,
    );
    let report = run_d1(&sig(JSON_AGENT), &ScanContext::new(home.path()));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].posture, Posture::Unknown);
    assert_eq!(report.findings[0].severity, Severity::Info);
}

#[test]
fn unparseable_artifact_reports_unknown_and_is_listed_in_coverage() {
    let home = TempHome::new();
    home.write(".testagent/settings.json", "{ this is not json");
    let report = run_d1(&sig(JSON_AGENT), &ScanContext::new(home.path()));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].posture, Posture::Unknown);
    assert_eq!(report.coverage[0].skipped_artifacts.len(), 1);
}

#[test]
fn undetected_agent_runs_no_checks() {
    let home = TempHome::new(); // no .testagent dir at all
    let report = run_d1(&sig(JSON_AGENT), &ScanContext::new(home.path()));
    assert!(report.findings.is_empty());
    assert!(!report.coverage[0].detected);
}

#[test]
fn pending_agent_detected_reports_unknown_not_safe() {
    let pending = r#"{
      "agent": "mystery",
      "version": 1,
      "detect_dirs": ["~/.mystery"],
      "pending": "not yet verified"
    }"#;
    let home = TempHome::new();
    home.mkdir(".mystery");
    let report = run_d1(&sig(pending), &ScanContext::new(home.path()));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].posture, Posture::Unknown);
    assert!(report.findings[0].title.contains("unknown"));
}

#[test]
fn toml_lite_reads_the_grok_shape() {
    let grok = r#"{
      "agent": "grok",
      "version": 1,
      "detect_dirs": ["~/.grok"],
      "checks": [
        {
          "id": "grok.trace_upload",
          "file": "~/.grok/config.toml",
          "format": "toml-lite",
          "key": "telemetry.trace_upload",
          "uploading_when": [true],
          "safe_when": [false],
          "at_risk_when_unset": true,
          "title": "trace upload has no veto",
          "veto": "set trace_upload = false",
          "severity": "critical"
        }
      ]
    }"#;
    let home = TempHome::new();
    home.write(
        ".grok/config.toml",
        "# grok config\n[features]\ntelemetry = true\n\n[telemetry]\ntrace_upload = true\n",
    );
    let report = run_d1(&sig(grok), &ScanContext::new(home.path()));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].severity, Severity::Critical);
    assert_eq!(report.findings[0].posture, Posture::AtRisk);

    // And the veto flips it silent — the positive/negative pair in one place.
    home.write(".grok/config.toml", "[telemetry]\ntrace_upload = false\n");
    let report = run_d1(&sig(grok), &ScanContext::new(home.path()));
    assert!(report.findings.is_empty());
}

#[test]
fn env_format_reads_dotenv_style_files() {
    let envsig = JSON_AGENT
        .replace(
            r#""file": "~/.testagent/settings.json""#,
            r#""file": "~/.testagent/config.env""#,
        )
        .replace(r#""format": "json""#, r#""format": "env""#)
        .replace(r#""key": "telemetry.enabled""#, r#""key": "UPLOAD_TRACES""#)
        .replace(
            r#""uploading_when": [true]"#,
            r#""uploading_when": ["1", "true"]"#,
        )
        .replace(r#""safe_when": [false]"#, r#""safe_when": ["0", "false"]"#);
    let home = TempHome::new();
    home.write(
        ".testagent/config.env",
        "# comment\nexport UPLOAD_TRACES=1\nOTHER=x\n",
    );
    let report = run_d1(&sig(&envsig), &ScanContext::new(home.path()));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].posture, Posture::AtRisk);
}

#[test]
fn alternate_keys_find_a_setting_that_moved() {
    let moved = JSON_AGENT.replace(
        r#""key": "telemetry.enabled","#,
        r#""key": "privacy.telemetry", "alt_keys": ["telemetry.enabled"],"#,
    );
    let home = TempHome::new();
    home.write(
        ".testagent/settings.json",
        r#"{"telemetry": {"enabled": true}}"#,
    );
    let report = run_d1(&sig(&moved), &ScanContext::new(home.path()));
    assert_eq!(
        report.findings.len(),
        1,
        "the legacy spelling still answers: {:?}",
        report.findings
    );
    assert!(
        report.findings[0]
            .evidence
            .as_ref()
            .unwrap()
            .snippet
            .starts_with("telemetry.enabled =")
    );

    home.write(
        ".testagent/settings.json",
        r#"{"privacy": {"telemetry": false}, "telemetry": {"enabled": true}}"#,
    );
    let report = run_d1(&sig(&moved), &ScanContext::new(home.path()));
    assert!(
        report.findings.is_empty(),
        "the primary key wins: {:?}",
        report.findings
    );
}

#[test]
fn a_veto_exported_in_the_environment_counts_and_says_so() {
    let with_env = JSON_AGENT
        .replace(
            r#""key": "telemetry.enabled","#,
            r#""key": "env.UPLOAD_TRACES", "env_var": "UPLOAD_TRACES","#,
        )
        .replace(
            r#""uploading_when": [true]"#,
            r#""uploading_when": [true, "1"]"#,
        )
        .replace(r#""safe_when": [false]"#, r#""safe_when": [false, "0"]"#);
    let home = TempHome::new();
    home.write(".testagent/settings.json", r#"{"other": 1}"#);
    let env = |v: &str| -> BTreeMap<String, String> {
        [("UPLOAD_TRACES".to_string(), v.to_string())]
            .into_iter()
            .collect()
    };

    let report = run_d1(
        &sig(&with_env),
        &ScanContext::new(home.path()).with_env(env("1")),
    );
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    let ev = report.findings[0].evidence.as_ref().unwrap();
    assert_eq!(ev.path, "process environment");
    assert!(ev.snippet.contains("UPLOAD_TRACES=1"), "{ev:?}");

    let report = run_d1(
        &sig(&with_env),
        &ScanContext::new(home.path()).with_env(env("0")),
    );
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.coverage[0].protected, 1);

    // The artifact wins when both answer.
    home.write(
        ".testagent/settings.json",
        r#"{"env": {"UPLOAD_TRACES": "0"}}"#,
    );
    let report = run_d1(
        &sig(&with_env),
        &ScanContext::new(home.path()).with_env(env("1")),
    );
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

#[test]
fn presence_vetoes_and_umbrella_switches() {
    let presence = r#"{
      "agent": "testagent",
      "version": 1,
      "detect_dirs": ["~/.testagent"],
      "checks": [{
        "id": "testagent.metrics",
        "file": "~/.testagent/settings.json",
        "format": "json",
        "key": "env.DISABLE_METRICS",
        "env_var": "DISABLE_METRICS",
        "uploading_when": [],
        "safe_when_set": true,
        "at_risk_when_unset": true,
        "alt_vetoes": [{"key": "env.DISABLE_EVERYTHING", "env_var": "DISABLE_EVERYTHING", "safe_when_set": true}],
        "title": "metrics on by default",
        "veto": "set DISABLE_METRICS",
        "severity": "low"
      }]
    }"#;
    let home = TempHome::new();
    home.write(".testagent/settings.json", "{}");
    let report = run_d1(&sig(presence), &ScanContext::new(home.path()));
    assert_eq!(
        report.findings.len(),
        1,
        "unset means on: {:?}",
        report.findings
    );
    assert!(
        report.findings[0]
            .evidence
            .as_ref()
            .unwrap()
            .snippet
            .contains("unset")
    );

    for content in [
        r#"{"env": {"DISABLE_METRICS": "1"}}"#,
        r#"{"env": {"DISABLE_METRICS": "0"}}"#,
        r#"{"env": {"DISABLE_EVERYTHING": "yes"}}"#,
    ] {
        home.write(".testagent/settings.json", content);
        let report = run_d1(&sig(presence), &ScanContext::new(home.path()));
        assert!(
            report.findings.is_empty(),
            "{content}: {:?}",
            report.findings
        );
    }
    home.write(
        ".testagent/settings.json",
        r#"{"env": {"DISABLE_METRICS": ""}}"#,
    );
    let report = run_d1(&sig(presence), &ScanContext::new(home.path()));
    assert_eq!(
        report.findings.len(),
        1,
        "an empty value is not set: {:?}",
        report.findings
    );

    let umbrella: BTreeMap<String, String> = [("DISABLE_EVERYTHING".to_string(), "1".to_string())]
        .into_iter()
        .collect();
    let report = run_d1(
        &sig(presence),
        &ScanContext::new(home.path()).with_env(umbrella),
    );
    assert!(
        report.findings.is_empty(),
        "the umbrella in the shell: {:?}",
        report.findings
    );
}

#[test]
fn evidence_says_which_of_the_three_absences_it_is() {
    let grok_like = JSON_AGENT.replace(
        r#""at_risk_when_unset": false"#,
        r#""at_risk_when_unset": true"#,
    );
    let home = TempHome::new();
    home.mkdir(".testagent");
    let report = run_d1(&sig(&grok_like), &ScanContext::new(home.path()));
    let ev = report.findings[0].evidence.as_ref().unwrap();
    assert!(ev.snippet.starts_with("file not present"), "{ev:?}");
    assert_eq!(ev.path, "~/.testagent/settings.json");

    home.write(".testagent/settings.json", r#"{"other": 1}"#);
    let report = run_d1(&sig(&grok_like), &ScanContext::new(home.path()));
    let ev = report.findings[0].evidence.as_ref().unwrap();
    assert!(
        ev.snippet.starts_with("telemetry.enabled is unset"),
        "{ev:?}"
    );

    home.write(
        ".testagent/settings.json",
        r#"{"telemetry": {"enabled": true}}"#,
    );
    let report = run_d1(&sig(&grok_like), &ScanContext::new(home.path()));
    let ev = report.findings[0].evidence.as_ref().unwrap();
    assert_eq!(ev.snippet, "telemetry.enabled = true");
}
