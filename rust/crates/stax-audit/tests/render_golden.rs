//! B3 — golden-output tests for the table. Regenerate with
//! `UPDATE_GOLDEN=1 cargo test -p stax-audit --test render_golden`, then
//! review the diff like any other code change.

use stax_audit::{
    AgentCoverage, AuditReport, Detector, EgressFinding, Evidence, Posture, Severity, render_table,
};

fn fixed_report() -> AuditReport {
    let f =
        |provider: &str, id: &str, sev, posture, title: &str, veto: Option<&str>| EgressFinding {
            provider: provider.into(),
            detector: Detector::Config,
            signature_id: id.into(),
            severity: sev,
            posture,
            title: title.into(),
            evidence: Some(Evidence {
                path: format!("~/.{provider}/config"),
                line: None,
                snippet: "k = v".into(),
            }),
            remediation: veto.map(Into::into),
            session_id: None,
        };
    let cov = |agent: &str, detected: bool, at_risk, protected, unknown| AgentCoverage {
        agent: agent.into(),
        detected,
        at_risk,
        protected,
        unknown,
        skipped_artifacts: Vec::new(),
        pending: None,
    };
    AuditReport {
        findings: vec![
            f(
                "grok",
                "grok.trace_upload",
                Severity::Critical,
                Posture::AtRisk,
                "Grok Build CLI can tarball this repo into xAI's trace bucket",
                Some("set [telemetry] trace_upload = false in ~/.grok/config.toml"),
            ),
            f(
                "gemini",
                "gemini.usage_statistics",
                Severity::Medium,
                Posture::AtRisk,
                "Gemini CLI usage-statistics collection is on (the default)",
                Some("set \"usageStatisticsEnabled\": false in ~/.gemini/settings.json"),
            ),
            f(
                "codex",
                "codex.pending",
                Severity::Info,
                Posture::Unknown,
                "codex detected, no verified signature yet — posture unknown, not safe",
                None,
            ),
        ],
        transcript_note: None,
        coverage: vec![
            cov("grok", true, 1, 1, 0),
            cov("gemini", true, 1, 0, 0),
            cov("codex", true, 0, 0, 1),
            cov("claude", true, 0, 1, 0),
            cov("cursor", false, 0, 0, 0),
        ],
    }
}

fn empty_report() -> AuditReport {
    AuditReport {
        findings: vec![],
        transcript_note: None,
        coverage: vec![AgentCoverage {
            agent: "claude".into(),
            detected: true,
            at_risk: 0,
            protected: 1,
            unknown: 0,
            skipped_artifacts: Vec::new(),
            pending: None,
        }],
    }
}

fn check_golden(name: &str, rendered: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, rendered).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("golden {name} missing — run with UPDATE_GOLDEN=1 and review the output")
    });
    assert_eq!(rendered, want, "golden {name} drifted");
}

#[test]
fn three_findings_at_80_columns() {
    check_golden("three_findings_80.txt", &render_table(&fixed_report(), 80));
}

#[test]
fn three_findings_at_200_columns() {
    check_golden(
        "three_findings_200.txt",
        &render_table(&fixed_report(), 200),
    );
}

#[test]
fn zero_findings_at_80_columns() {
    check_golden("zero_findings_80.txt", &render_table(&empty_report(), 80));
}

#[test]
fn header_counts_at_risk_agents_not_findings() {
    let table = render_table(&fixed_report(), 120);
    assert!(
        table.starts_with("EGRESS AUDIT — 2 of your 4 coding agents"),
        "grok+gemini are flagged; codex unknown, claude clean, cursor undetected:\n{table}"
    );
}
