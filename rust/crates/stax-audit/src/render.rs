//! B3 — the findings table. The output IS the feature: designed to be
//! screenshotted and posted. Hand-rolled (no table dependency), plain text —
//! the CLI layers color on top only when stdout is a terminal.
//!
//! Every finding prints its evidence on a second line, because a row that
//! reads "critical" with no basis is a row nobody can check: two CRITICAL
//! grok rows from an empty `~/.grok` are honest only when the line under
//! them says the config file does not exist.

use crate::{AuditReport, Posture, Severity};

/// Render the report as the terminal table, wrapped to `width` columns.
pub fn render_table(report: &AuditReport, width: usize) -> String {
    let width = width.clamp(60, 400);
    let detected: Vec<_> = report.coverage.iter().filter(|c| c.detected).collect();
    // An agent counts as at-risk if EITHER detector implicates it — a config
    // veto that is present does not excuse an upload the transcript recorded.
    // Low-severity rows (usage metrics that carry no code) still print, but
    // they do not put an agent in the headline: "can upload your data" has to
    // mean something a reader would call their data.
    let flagged: std::collections::BTreeSet<&str> = report
        .findings
        .iter()
        .filter(|f| matches!(f.posture, Posture::AtRisk | Posture::Occurred))
        .filter(|f| f.severity >= Severity::Medium)
        .map(|f| f.provider.as_str())
        .collect();
    let denominator = detected
        .iter()
        .map(|c| c.agent.as_str())
        .chain(flagged.iter().copied())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let agents_at_risk = flagged.len();

    let mut out = String::new();
    out.push_str(&format!(
        "EGRESS AUDIT — {agents_at_risk} of your {denominator} coding agents can upload your data\n\n"
    ));

    let mut rows: Vec<&crate::EgressFinding> = report.findings.iter().collect();
    rows.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.signature_id.cmp(&b.signature_id))
    });

    if rows.is_empty() {
        let protected: usize = detected.iter().map(|c| c.protected).sum();
        out.push_str(&format!(
            "  nothing configured to upload — {} agents detected, {protected} vetoes verified present\n",
            detected.len()
        ));
    } else {
        let agent_w = rows
            .iter()
            .map(|f| f.provider.len())
            .chain(std::iter::once("AGENT".len()))
            .max()
            .unwrap_or(5);
        let sev_w = "SEVERITY".len();
        // 2-space gutter + agent + 2 + finding + 2 + severity + 2 + veto
        let fixed = 2 + agent_w + 2 + 2 + sev_w + 2;
        let avail = width.saturating_sub(fixed).max(24);
        let finding_w = (avail * 45 / 100).max(18);
        let veto_w = avail - finding_w;
        let evidence_indent = 2 + agent_w + 2;

        out.push_str(&format!(
            "  {:agent_w$}  {:finding_w$}  {:sev_w$}  {}\n",
            "AGENT", "FINDING", "SEVERITY", "VETO"
        ));
        for f in &rows {
            let veto = match (&f.posture, &f.remediation) {
                (Posture::Unknown, None) => "—".to_string(),
                (_, Some(v)) => v.clone(),
                (_, None) => "—".to_string(),
            };
            out.push_str(&format!(
                "  {:agent_w$}  {:finding_w$}  {:sev_w$}  {}\n",
                f.provider,
                clip(&f.title, finding_w),
                severity_label(f.severity),
                clip(&veto, veto_w),
            ));
            if let Some(ev) = &f.evidence {
                let line = format!("↳ {}: {}", ev.path, ev.snippet);
                out.push_str(&format!(
                    "{:evidence_indent$}{}\n",
                    "",
                    clip(&line, width.saturating_sub(evidence_indent))
                ));
            }
        }
    }

    if let Some(note) = &report.transcript_note {
        out.push_str(&format!("\n  transcripts: {note}\n"));
    }

    let unknown: usize = detected.iter().map(|c| c.unknown).sum();
    let skipped: usize = detected.iter().map(|c| c.skipped_artifacts.len()).sum();
    out.push('\n');
    out.push_str(&format!(
        "  {} findings · unknown ≠ safe ({unknown} unknown, {skipped} artifacts not read) · vetoes are suggestions — staxtrace never edits your configs\n",
        report.findings.len()
    ));
    out
}

fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Critical => "critical",
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low => "low",
        Severity::Info => "info",
    }
}

/// Truncate to `w` display columns with an ellipsis. Titles and vetoes are
/// ASCII-leaning; char-count is close enough for a v0 table.
fn clip(s: &str, w: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= w {
        return s.to_string();
    }
    let mut clipped: String = chars[..w.saturating_sub(1)].iter().collect();
    clipped.push('…');
    clipped
}
