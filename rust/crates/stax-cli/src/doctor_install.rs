//! `stax doctor --install` — verifies the installation itself: the binary,
//! its two siblings, the embedded signature catalog, and the data dir. The
//! launch-day support tool: "it doesn't work" starts here.
//!
//! Additive on purpose: bare `stax doctor` is byte-parity pinned (gate 4);
//! this runs only behind the flag and prints its own report.

use crate::click::Output;
use anyhow::Result;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub fn run_install_checks() -> Result<Output> {
    let mut out = String::new();
    writeln!(out, "INSTALL CHECK — stax {}", env!("CARGO_PKG_VERSION"))?;

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("stax"));
    writeln!(out, "\n  binary       {}", exe.display())?;

    for name in ["stax-server", "stax-hooks"] {
        match sibling(&exe, name) {
            Some(path) => writeln!(out, "  {name:<12} {}", path.display())?,
            None => writeln!(
                out,
                "  {name:<12} MISSING — the tap and install.sh ship all three binaries \
                 side by side; `stax start` and hooks need it"
            )?,
        }
    }

    match stax_audit::embedded_catalog() {
        Ok(catalog) => {
            let pending = catalog.iter().filter(|s| s.pending.is_some()).count();
            writeln!(
                out,
                "  signatures   {} agents embedded ({} live, {pending} pending)",
                catalog.len(),
                catalog.len() - pending
            )?;
        }
        Err(err) => writeln!(out, "  signatures   LOAD ERROR: {err}")?,
    }

    let data_dir = stax_core::settings::app_dir();
    let state = if probe_writable(&data_dir) {
        "writable"
    } else {
        "NOT WRITABLE"
    };
    writeln!(out, "  data dir     {} ({state})", data_dir.display())?;

    writeln!(
        out,
        "\n  next: `stax audit` · `stax start` · `stax hooks status`"
    )?;
    Ok(Output::ok(out))
}

fn sibling(exe: &Path, name: &str) -> Option<PathBuf> {
    exe.parent()
        .map(|dir| dir.join(name))
        .filter(|path| path.is_file())
}

/// Create-and-remove probe; a doctor that only stats can miss a read-only
/// mount, and one that leaves droppings is worse than none.
fn probe_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".doctor-probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}
