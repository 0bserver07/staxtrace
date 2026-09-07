//! D1 — the config detector: evaluate every signature against a home
//! directory, emit findings + coverage. Preventive: needs no session (§4.1).

use crate::{AgentSignature, Check, Detector, EgressFinding, Evidence, Posture, Severity};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// All `~/` in signatures resolve against this home, and env-keyed checks
/// read this environment. Tests hand a synthetic pair; the CLI hands the real
/// ones.
pub struct ScanContext {
    pub home: PathBuf,
    /// The process environment the audit runs in. A veto exported from the
    /// shell is a veto: Claude Code reads `DISABLE_TELEMETRY` from either
    /// place, so an audit that only read settings.json told users who had
    /// opted out in `.zshrc` that they were at risk.
    pub env: BTreeMap<String, String>,
}

impl ScanContext {
    /// A context with an empty environment — what the hermetic tests use.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            env: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }
}

/// Per-agent audit accounting — the header line's denominator and the honest
/// tail ("2 artifacts unreadable") both come from here.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentCoverage {
    pub agent: String,
    pub detected: bool,
    pub at_risk: usize,
    pub protected: usize,
    pub unknown: usize,
    pub skipped_artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
}

/// What one D1 run produces.
#[derive(Debug, serde::Serialize)]
pub struct AuditReport {
    pub findings: Vec<EgressFinding>,
    pub coverage: Vec<AgentCoverage>,
    /// What the transcript pass (D3) did or could not do — printed verbatim,
    /// because "no store yet" is coverage information, not a clean result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_note: Option<String>,
}

/// What one check concluded. `Silent` is the only outcome with no finding:
/// a check whose key is unset and whose signature says unset is fine.
enum Verdict {
    AtRisk(Evidence),
    Protected,
    Unknown {
        title: String,
        evidence: Evidence,
        skipped: bool,
    },
    Silent {
        skipped: Option<String>,
    },
}

pub fn run_d1(catalog: &[AgentSignature], ctx: &ScanContext) -> AuditReport {
    let mut findings = Vec::new();
    let mut coverage = Vec::new();

    for sig in catalog {
        let detected = sig.detect_dirs.iter().any(|d| resolve(ctx, d).exists());
        let mut cov = AgentCoverage {
            agent: sig.agent.clone(),
            detected,
            at_risk: 0,
            protected: 0,
            unknown: 0,
            skipped_artifacts: Vec::new(),
            pending: sig.pending.clone(),
        };
        if !detected {
            coverage.push(cov);
            continue;
        }

        if let Some(reason) = &sig.pending {
            findings.push(EgressFinding {
                provider: sig.agent.clone(),
                detector: Detector::Config,
                signature_id: format!("{}.pending", sig.agent),
                severity: Severity::Info,
                posture: Posture::Unknown,
                title: format!("{} detected — {reason} (unknown, not safe)", sig.agent),
                evidence: None,
                remediation: None,
                session_id: None,
            });
            cov.unknown += 1;
            coverage.push(cov);
            continue;
        }

        for check in &sig.checks {
            match evaluate(check, ctx) {
                Verdict::AtRisk(evidence) => {
                    cov.at_risk += 1;
                    findings.push(finding(
                        sig,
                        check,
                        Posture::AtRisk,
                        check.severity,
                        check.title.clone(),
                        Some(evidence),
                    ));
                }
                Verdict::Protected => cov.protected += 1,
                Verdict::Unknown {
                    title,
                    evidence,
                    skipped,
                } => {
                    cov.unknown += 1;
                    if skipped {
                        cov.skipped_artifacts.push(check.file.clone());
                    }
                    findings.push(finding(
                        sig,
                        check,
                        Posture::Unknown,
                        Severity::Info,
                        title,
                        Some(evidence),
                    ));
                }
                Verdict::Silent { skipped } => {
                    if let Some(artifact) = skipped {
                        cov.skipped_artifacts.push(artifact);
                    }
                }
            }
        }
        coverage.push(cov);
    }

    AuditReport {
        findings,
        coverage,
        transcript_note: None,
    }
}

/// One check, in the order the evidence has to be weighed:
///
/// 1. an umbrella veto (artifact, then environment) settles it as protected;
/// 2. the key itself — or a moved spelling of it — in the artifact;
/// 3. the same setting exported in the process environment;
/// 4. nothing answered: at risk only if the signature says unset means so.
fn evaluate(check: &Check, ctx: &ScanContext) -> Verdict {
    let path = resolve(ctx, &check.file);
    let tree = if path.is_file() {
        let read = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| crate::parse::read_artifact(&text, check.format));
        match read {
            Ok(tree) => Some(tree),
            Err(err) => {
                // An unreadable artifact cannot prove anything — except that a
                // veto exported in the environment still stands.
                if let Some(verdict) = environment_veto(check, ctx) {
                    return verdict;
                }
                return Verdict::Unknown {
                    title: format!("{} — artifact cannot be read", check.title),
                    evidence: Evidence {
                        path: check.file.clone(),
                        line: None,
                        snippet: format!("{err}"),
                    },
                    skipped: true,
                };
            }
        }
    } else {
        None
    };

    for alt in &check.alt_vetoes {
        if let Some(tree) = &tree
            && let Some(value) = crate::parse::lookup(tree, &alt.key)
            && (alt.safe_when.iter().any(|s| s == value) || (alt.safe_when_set && is_set(value)))
        {
            return Verdict::Protected;
        }
        if let Some(var) = &alt.env_var
            && let Some(raw) = ctx.env.get(var)
            && (alt.safe_when.iter().any(|s| same_as_env(s, raw))
                || (alt.safe_when_set && !raw.is_empty()))
        {
            return Verdict::Protected;
        }
    }

    if let Some(tree) = &tree {
        let keys =
            std::iter::once(check.key.as_str()).chain(check.alt_keys.iter().map(String::as_str));
        for key in keys {
            if let Some(value) = crate::parse::lookup(tree, key) {
                return classify(check, value, |_| Evidence {
                    path: check.file.clone(),
                    line: None,
                    snippet: format!("{key} = {value}"),
                });
            }
        }
    }

    if let Some(var) = &check.env_var
        && let Some(raw) = ctx.env.get(var)
    {
        let value = Value::String(raw.clone());
        return classify(check, &value, |_| Evidence {
            path: "process environment".into(),
            line: None,
            snippet: format!("{var}={raw} (exported in your shell)"),
        });
    }

    if check.at_risk_when_unset {
        let snippet = if tree.is_some() {
            format!("{} is unset in {} (no local veto)", check.key, check.file)
        } else {
            format!("file not present — {} is unset (no local veto)", check.key)
        };
        return Verdict::AtRisk(Evidence {
            path: check.file.clone(),
            line: None,
            snippet,
        });
    }
    Verdict::Silent {
        skipped: tree.is_none().then(|| check.file.clone()),
    }
}

/// The environment alone, for an artifact that could not be read: only a
/// veto counts, because "uploading" from a shell variable would be a claim
/// about a file we just failed to read.
fn environment_veto(check: &Check, ctx: &ScanContext) -> Option<Verdict> {
    for alt in &check.alt_vetoes {
        if let Some(var) = &alt.env_var
            && let Some(raw) = ctx.env.get(var)
            && (alt.safe_when.iter().any(|s| same_as_env(s, raw))
                || (alt.safe_when_set && !raw.is_empty()))
        {
            return Some(Verdict::Protected);
        }
    }
    let var = check.env_var.as_ref()?;
    let raw = ctx.env.get(var)?;
    (check.safe_when.iter().any(|s| same_as_env(s, raw))
        || (check.safe_when_set && !raw.is_empty()))
    .then_some(Verdict::Protected)
}

/// Compare a present value against the signature's two lists. The evidence
/// is built lazily because a protected check keeps none.
fn classify(check: &Check, value: &Value, evidence: impl Fn(&Value) -> Evidence) -> Verdict {
    let hit = |list: &[Value]| match value {
        Value::String(raw) => list.iter().any(|s| s == value || same_as_env(s, raw)),
        _ => list.iter().any(|s| s == value),
    };
    if hit(&check.uploading_when) {
        Verdict::AtRisk(evidence(value))
    } else if hit(&check.safe_when) || (check.safe_when_set && is_set(value)) {
        Verdict::Protected
    } else {
        Verdict::Unknown {
            title: format!("{} — unrecognized value", check.title),
            evidence: evidence(value),
            skipped: false,
        }
    }
}

/// Presence semantics: set to anything but the empty string / null / false.
fn is_set(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// An environment variable is always a string; `1`, `true` and `"1"` in a
/// signature all mean the same exported value.
fn same_as_env(expected: &Value, raw: &str) -> bool {
    match expected {
        Value::String(s) => s == raw,
        Value::Bool(b) => b.to_string() == raw,
        Value::Number(n) => n.to_string() == raw,
        _ => false,
    }
}

fn finding(
    sig: &AgentSignature,
    check: &Check,
    posture: Posture,
    severity: Severity,
    title: String,
    evidence: Option<Evidence>,
) -> EgressFinding {
    EgressFinding {
        provider: sig.agent.clone(),
        detector: Detector::Config,
        signature_id: check.id.clone(),
        severity,
        posture,
        title,
        evidence,
        remediation: Some(check.veto.clone()),
        session_id: None,
    }
}

/// `~/x` resolves under the scan home and only ever under it: root, `..` and
/// `.` components are dropped, so a path the loader would have refused still
/// cannot escape here. (`home.join("/etc/passwd")` would have returned
/// `/etc/passwd` — a signature pack must not be able to read the machine.)
fn resolve(ctx: &ScanContext, path: &str) -> PathBuf {
    let rel = path.strip_prefix("~/").unwrap_or(path);
    let mut out = ctx.home.clone();
    for component in Path::new(rel).components() {
        if let Component::Normal(part) = component {
            out.push(part);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cannot_leave_the_scan_home() {
        let ctx = ScanContext::new("/scan/home");
        for hostile in [
            "/etc/passwd",
            "~/../../etc/passwd",
            "~//etc/passwd",
            "../x",
            "~/./a/../b",
        ] {
            let resolved = resolve(&ctx, hostile);
            assert!(
                resolved.starts_with("/scan/home"),
                "{hostile} resolved to {}",
                resolved.display()
            );
            assert!(
                !resolved.components().any(|c| c == Component::ParentDir),
                "{hostile} kept a .. component: {}",
                resolved.display()
            );
        }
        assert_eq!(
            resolve(&ctx, "~/.grok/config.toml"),
            PathBuf::from("/scan/home/.grok/config.toml")
        );
    }

    #[test]
    fn env_values_match_the_signature_forms() {
        assert!(same_as_env(&Value::from(1), "1"));
        assert!(same_as_env(&Value::Bool(true), "true"));
        assert!(same_as_env(&Value::String("1".into()), "1"));
        assert!(!same_as_env(&Value::Bool(true), "1"));
    }
}
