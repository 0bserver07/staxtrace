//! A5 — `stax doctor --install`: the support tool for launch-day "it doesn't
//! work" reports. Additive flag only: bare `stax doctor` output is byte-parity
//! pinned against the Python CLI (gate 4) and stays untouched.

use std::process::Command;

#[test]
fn doctor_install_reports_the_environment() {
    let home = std::env::temp_dir().join(format!("stax-doctor-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_stax"))
        .args(["doctor", "--install"])
        .env("HOME", &home)
        .env("COLUMNS", "100")
        .output()
        .expect("spawn stax");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "doctor reports, it does not fail; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for needle in [
        "INSTALL CHECK",
        "stax-server",
        "stax-hooks",
        "signatures",
        "data dir",
    ] {
        assert!(stdout.contains(needle), "missing {needle}:\n{stdout}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn doctor_help_offers_the_install_flag() {
    let out = Command::new(env!("CARGO_BIN_EXE_stax"))
        .args(["doctor", "--help"])
        .output()
        .expect("spawn stax");
    assert!(String::from_utf8_lossy(&out.stdout).contains("--install"));
}
