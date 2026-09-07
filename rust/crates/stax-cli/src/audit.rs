//! `stax audit` — Spec 28's D1 config audit, the preventive detector: which
//! coding agents on this machine are configured (or lack the veto) to send
//! your code or telemetry off-box, and the exact line that stops each one.

use crate::click::Output;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct AuditArgs {
    /// Emit findings + coverage as JSON (machines, CI).
    #[arg(long)]
    pub json: bool,

    /// Read signatures from a directory instead of the compiled-in catalog.
    #[arg(long, value_name = "DIR")]
    pub signatures: Option<PathBuf>,

    /// Exit 2 when any finding is at risk (CI gate).
    #[arg(long)]
    pub strict: bool,

    /// Skip the transcript scan (D3) and audit configs only.
    #[arg(long)]
    pub configs_only: bool,

    /// How many recent tool-carrying messages D3 reads.
    #[arg(long, value_name = "N")]
    pub window: Option<i64>,

    /// A host that is yours (exact, or `*.suffix`), so a transcript that
    /// copied to it is not an exfiltration finding. Repeatable; also read
    /// from STAXTRACE_AUDIT_ALLOW_HOSTS (comma-separated) and the
    /// `audit_allow_hosts` list in config.json. Private networks, the
    /// tailnet and localhost never need listing.
    #[arg(long = "allow-host", value_name = "HOST")]
    pub allow_hosts: Vec<String>,
}

/// The `config.json` key holding the standing allow-list.
pub const ALLOW_HOSTS_KEY: &str = "audit_allow_hosts";

pub fn run_audit(args: &AuditArgs) -> Result<Output> {
    let catalog = match &args.signatures {
        Some(dir) => stax_audit::catalog_from_dir(dir)?,
        None => stax_audit::embedded_catalog()?,
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate agent configs"))?;
    // The shell's environment is evidence too: a veto exported in .zshrc is
    // a veto, and Claude Code reads it there as readily as in settings.json.
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let ctx = stax_audit::ScanContext::new(home).with_env(env);
    let mut report = stax_audit::run_d1(&catalog, &ctx);

    // D3 rides the transcripts the store already ingested, so it covers every
    // provider at once — including agents with no config signature.
    if !args.configs_only {
        let window = args
            .window
            .unwrap_or(crate::audit_transcripts::DEFAULT_WINDOW);
        let scan = crate::audit_transcripts::collect(window);
        match scan.unavailable {
            Some(reason) => report.transcript_note = Some(reason),
            None => {
                let mut rules = stax_audit::transcript_rules()?;
                let allowed = allow_hosts(args);
                rules.allow_hosts.extend(allowed.iter().cloned());
                report
                    .findings
                    .extend(stax_audit::run_d3(&rules, &scan.invocations));
                let allow_note = match allowed.len() {
                    0 => String::new(),
                    1 => ", 1 host allow-listed".to_string(),
                    n => format!(", {n} hosts allow-listed"),
                };
                report.transcript_note = Some(format!(
                    "{} sessions scanned ({} tool calls, newest {window} messages{allow_note})",
                    scan.sessions,
                    scan.invocations.len()
                ));
            }
        }
    }

    let at_risk = report.findings.iter().any(|f| {
        matches!(
            f.posture,
            stax_audit::Posture::AtRisk | stax_audit::Posture::Occurred
        )
    });

    let body = if args.json {
        let mut json = serde_json::to_string_pretty(&report)?;
        json.push('\n');
        json
    } else {
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(100);
        stax_audit::render_table(&report, width)
    };

    Ok(Output {
        stdout: body,
        stderr: String::new(),
        code: if args.strict && at_risk { 2 } else { 0 },
    })
}

/// The standing allow-list: `--allow-host` flags, then the environment, then
/// `config.json` — all three merge, so a CI job can pass hosts on the
/// command line while a workstation keeps its deploy boxes in the file.
fn allow_hosts(args: &AuditArgs) -> Vec<String> {
    let mut hosts: Vec<String> = args
        .allow_hosts
        .iter()
        .flat_map(|h| h.split(','))
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();
    if let Some(raw) = stax_core::settings::env_var("AUDIT_ALLOW_HOSTS") {
        hosts.extend(
            raw.split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty()),
        );
    }
    let config = crate::settings::load();
    match config.get(ALLOW_HOSTS_KEY) {
        Some(stax_core::queries::pyjson::Value::Array(items)) => hosts.extend(
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string),
        ),
        Some(stax_core::queries::pyjson::Value::Str(raw)) => hosts.extend(
            raw.split(',')
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty()),
        ),
        _ => {}
    }
    hosts.sort();
    hosts.dedup();
    hosts
}
