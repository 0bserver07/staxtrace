//! B4 — `stax audit` end to end: the real binary, a synthetic `$HOME`,
//! nothing read from the machine it runs on.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "stax-audit-e2e-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn write(&self, rel: &str, contents: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stax(home: &TempHome, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_stax"))
        .args(args)
        .env("HOME", &home.0)
        .env("COLUMNS", "100")
        .output()
        .expect("spawn stax")
}

#[test]
fn audit_reports_the_grok_shape_end_to_end() {
    let home = TempHome::new();
    // Both vetoes absent — the incident posture (Spec 28 §0.3).
    home.write(".grok/config.toml", "[features]\ntelemetry = true\n");
    let out = stax(&home, &["audit"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("EGRESS AUDIT"), "got:\n{stdout}");
    assert!(stdout.contains("grok"), "got:\n{stdout}");
    assert!(
        stdout.contains("trace_upload"),
        "the veto names the fix:\n{stdout}"
    );
}

#[test]
fn audit_json_parses_and_strict_exits_2_when_at_risk() {
    let home = TempHome::new();
    home.write(".grok/config.toml", "[features]\ntelemetry = true\n");
    let out = stax(&home, &["audit", "--json", "--strict"]);
    assert_eq!(out.status.code(), Some(2), "strict + at_risk must exit 2");
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("audit --json emits valid JSON");
    let findings = value["findings"].as_array().expect("findings array");
    assert!(!findings.is_empty());
    assert_eq!(findings[0]["detector"], "config");
}

#[test]
fn audit_is_quiet_and_exits_0_on_a_vetoed_machine() {
    let home = TempHome::new();
    home.write(
        ".grok/config.toml",
        "[features]\ntelemetry = false\n[telemetry]\ntrace_upload = false\ndisable_codebase_upload = true\n",
    );
    let out = stax(&home, &["audit", "--strict"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "vetoed machine is not at risk:\n{stdout}"
    );
    assert!(
        stdout.contains("0 of your 1 coding agents"),
        "got:\n{stdout}"
    );
}
