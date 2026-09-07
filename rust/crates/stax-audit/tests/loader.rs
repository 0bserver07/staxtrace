//! B1 — the signature catalog loads, validates, and fails closed.
//!
//! Spec 28 §6: signatures are declarative data following the
//! `adapters/capabilities.json` pattern. A catalog that does not validate is a
//! load error, never a silently-skipped check (§4.4 fail-closed).

use stax_audit::{Severity, catalog_from_str};

const TESTAGENT: &str = r#"{
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
      "title": "testagent uploads telemetry",
      "veto": "set telemetry.enabled=false in ~/.testagent/settings.json",
      "severity": "high"
    }
  ]
}"#;

#[test]
fn loads_a_signature_and_its_check() {
    let sig = catalog_from_str(TESTAGENT).expect("valid signature must load");
    assert_eq!(sig.agent, "testagent");
    assert_eq!(sig.checks.len(), 1);
    let check = &sig.checks[0];
    assert_eq!(check.id, "testagent.telemetry");
    assert_eq!(check.severity, Severity::High);
    assert!(check.veto.contains("telemetry.enabled=false"));
}

#[test]
fn duplicate_check_ids_are_a_load_error() {
    let two = r#"{
  "agent": "testagent",
  "version": 1,
  "detect_dirs": ["~/.testagent"],
  "checks": [
    { "id": "testagent.telemetry", "file": "~/.t/a.json", "format": "json",
      "key": "a", "uploading_when": [true], "title": "a", "veto": "va", "severity": "high" },
    { "id": "testagent.telemetry", "file": "~/.t/b.json", "format": "json",
      "key": "b", "uploading_when": [true], "title": "b", "veto": "vb", "severity": "low" }
  ]
}"#;
    let err = catalog_from_str(two).unwrap_err().to_string();
    assert!(err.contains("duplicate"), "got: {err}");
    assert!(err.contains("testagent.telemetry"), "got: {err}");
}

#[test]
fn missing_veto_is_a_load_error() {
    let noveto = TESTAGENT.replace(
        r#""veto": "set telemetry.enabled=false in ~/.testagent/settings.json","#,
        r#""veto": "","#,
    );
    let err = catalog_from_str(&noveto).unwrap_err().to_string();
    assert!(err.contains("veto"), "got: {err}");
}

#[test]
fn unknown_severity_is_a_load_error() {
    let bad = TESTAGENT.replace(r#""severity": "high""#, r#""severity": "shrug""#);
    assert!(catalog_from_str(&bad).is_err());
}

#[test]
fn a_check_needs_file_or_env_and_a_key_for_files() {
    let neither = TESTAGENT.replace(r#""file": "~/.testagent/settings.json","#, "");
    let err = catalog_from_str(&neither).unwrap_err().to_string();
    assert!(err.contains("file") || err.contains("env"), "got: {err}");
}

#[test]
fn embedded_catalog_loads_and_every_agent_validates() {
    let catalog = stax_audit::embedded_catalog().expect("shipped signatures must validate");
    assert!(
        catalog.iter().any(|s| s.agent == "grok"),
        "the incident-verified grok signature must ship"
    );
    for sig in &catalog {
        assert!(
            !sig.detect_dirs.is_empty(),
            "{}: needs detect_dirs",
            sig.agent
        );
    }
}

#[test]
fn a_signature_may_only_read_under_the_scan_home() {
    // `resolve()` once did `home.join(file)`, and `home.join("/etc/passwd")`
    // is `/etc/passwd`: any third-party signature pack could make
    // `stax audit --json` read and echo arbitrary files. The loader refuses
    // the shape outright now, and `resolve` drops the components anyway.
    for hostile in [
        "/etc/passwd",
        "~/../../etc/passwd",
        "~//etc/passwd",
        "etc/passwd",
        "~/",
        "~",
    ] {
        let bad = TESTAGENT.replace(
            r#""file": "~/.testagent/settings.json","#,
            &format!(r#""file": "{hostile}","#),
        );
        let err = catalog_from_str(&bad).unwrap_err().to_string();
        assert!(err.contains("~/"), "{hostile}: {err}");
    }
    let bad_dir = TESTAGENT.replace(
        r#""detect_dirs": ["~/.testagent"],"#,
        r#""detect_dirs": ["/"],"#,
    );
    let err = catalog_from_str(&bad_dir).unwrap_err().to_string();
    assert!(err.contains("detect_dirs"), "{err}");
}

#[test]
fn the_legacy_format_spelling_still_loads() {
    let legacy = TESTAGENT.replace(r#""format": "json""#, r#""format": "toml-lite""#);
    let sig = catalog_from_str(&legacy).expect("toml-lite is an alias for toml");
    assert_eq!(sig.checks[0].format, stax_audit::Format::Toml);
}

#[test]
fn an_alternate_veto_needs_a_way_to_be_satisfied() {
    let bad = TESTAGENT.replace(
        r#""uploading_when": [true],"#,
        r#""uploading_when": [true], "alt_vetoes": [{"key": "env.X"}],"#,
    );
    let err = catalog_from_str(&bad).unwrap_err().to_string();
    assert!(err.contains("alternate veto"), "{err}");
}
