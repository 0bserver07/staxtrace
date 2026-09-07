//! The compiled-in `stackunderflow/static/` tree.
//!
//! `build.rs` generates a sorted `&[(&str, &[u8])]` — one `include_bytes!` per
//! file, keyed by the path relative to `stackunderflow/static/` with `/`
//! separators. This module is the read side: a binary search, a directory
//! predicate, and the path-to-key translation the route modules need.
//!
//! # Why the binary carries the bundle at all
//!
//! `docs/specs/decommission-report.md` §4.3 lists the React build as one of the
//! two RUNTIME couplings that kept a Rust-only machine dependent on the Python
//! package directory. Compiling it in is what makes `stax-server` stand with
//! `stackunderflow/` deleted. `STAX_STATIC_DIR` re-points at a directory for
//! frontend development, and that override is the only way to reach the disk.
//!
//! # The key space is the *nominal* path, not a real one
//!
//! Callers already do their path math against `AppState::static_dir()` —
//! `misc.rs`'s `/assets/{path}` containment check is a string-prefix test on a
//! lexically resolved path, ported from `os.path` and load-bearing (a request
//! resolving exactly to the assets directory is `400 Invalid path`, not `404`).
//! None of that math touches the disk, so it works unchanged against a root
//! that does not exist. [`key_for`] is the last step: it turns the resolved
//! absolute path back into a table key. The containment rules stay where they
//! were written, and only the final byte fetch moves.

use std::path::{Component, Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/static_assets.rs"));

/// The bytes at `key`, or `None` when the table has no such file.
#[must_use]
pub fn get(key: &str) -> Option<&'static [u8]> {
    EMBEDDED
        .binary_search_by_key(&key, |(name, _)| *name)
        .ok()
        .map(|index| EMBEDDED[index].1)
}

/// Whether `key` names a directory — i.e. some file's key starts with
/// `key/`.
///
/// There are no directory entries in the table (nothing is stored for them), so
/// "is a directory" is a question about the key space. The sorted table makes
/// it a `partition_point`: the first key `>= "key/"` either starts with it or
/// the directory is empty and therefore, for serving purposes, absent.
#[must_use]
pub fn is_dir(key: &str) -> bool {
    if key.is_empty() {
        return !EMBEDDED.is_empty();
    }
    let prefix = format!("{key}/");
    let at = EMBEDDED.partition_point(|(name, _)| *name < prefix.as_str());
    EMBEDDED
        .get(at)
        .is_some_and(|(name, _)| name.starts_with(&prefix))
}

/// How many files the binary carries.
#[must_use]
pub fn len() -> usize {
    EMBEDDED.len()
}

/// Every key, in sorted order.
pub fn keys() -> impl Iterator<Item = &'static str> {
    EMBEDDED.iter().map(|(name, _)| *name)
}

/// The table key for `path` under the nominal root `root`.
///
/// Both sides are resolved lexically first — `root` is
/// `package_dir.join("static")` and `package_dir` is routinely a relative
/// `…/crates/stax-server/../../../stackunderflow`, while callers hand in a path
/// that has already been through their own `..`-collapsing resolver. Comparing
/// one collapsed path against one uncollapsed root would miss every time.
///
/// `None` when `path` is not under `root`, which is the caller's own
/// containment failure showing up a second time and is answered the same way a
/// missing file is.
#[must_use]
pub fn key_for(root: &Path, path: &Path) -> Option<String> {
    let root = resolve_lexically(root);
    let path = resolve_lexically(path);
    let relative = path.strip_prefix(&root).ok()?;
    let key = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    Some(key)
}

/// Collapse `.` and `..` without touching the filesystem.
///
/// The same nine lines `routes/misc.rs` and `routes/projects.rs` each carry;
/// duplicated here rather than imported because those two are private to route
/// modules and this crate's law is one owner per helper — the owner of *key*
/// resolution is this module.
fn resolve_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const UI_BUNDLE_RECEIPT: &str = "ui-bundle-provenance.json";

    fn is_sha256(value: &str) -> bool {
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    #[test]
    fn the_bundle_is_compiled_in() {
        // If this fails the dashboard has no oracle and `stax-server` cannot
        // stand without the Python tree. It must never be "fixed" by running a
        // frontend build — the bundle is checked in and stays unmodified
        // (`docs/specs/rust-port.md` §2.3).
        assert!(len() >= 60, "only {} files embedded", len());
        let index = get("react/index.html").expect("the SPA entry is embedded");
        assert!(
            String::from_utf8_lossy(index).contains("<div id=\"root\">"),
            "not the SPA entry"
        );
        assert!(get("favicon.ico").is_some(), "favicon.ico is embedded");
    }

    #[test]
    fn the_ui_bundle_receipt_is_compiled_in_and_well_formed() {
        let receipt = get(UI_BUNDLE_RECEIPT).expect("the UI provenance receipt is embedded");
        let receipt: serde_json::Value =
            serde_json::from_slice(receipt).expect("the UI provenance receipt is valid JSON");

        assert_eq!(
            receipt["schema"], "staxtrace.ui-bundle-provenance/1",
            "unexpected UI provenance schema"
        );
        for field in [
            "source_tree_sha256",
            "lockfile_sha256",
            "generated_output_tree_sha256",
        ] {
            let digest = receipt[field]
                .as_str()
                .unwrap_or_else(|| panic!("{field} must be a string"));
            assert!(is_sha256(digest), "{field} must be a SHA-256 digest");
        }
        assert!(
            receipt["toolchain"]["node"].as_str().is_some(),
            "the Node identity is required"
        );
        assert!(
            receipt["toolchain"]["npm"].as_str().is_some(),
            "the npm identity is required"
        );
    }

    #[test]
    fn the_embedded_bytes_are_the_checked_in_bytes() {
        // Not a tautology: it catches a `build.rs` root that silently points at
        // some other tree, which is the whole hazard `include_str!` carries and
        // the reason the capability table was kept on disk for eight waves.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/static");
        for key in keys() {
            let on_disk = std::fs::read(root.join(key.replace('/', std::path::MAIN_SEPARATOR_STR)))
                .unwrap_or_else(|err| panic!("reading {key}: {err}"));
            assert_eq!(
                get(key).expect("keys() yields present keys"),
                on_disk.as_slice(),
                "{key} drifted between the tree and the binary"
            );
        }
    }

    #[test]
    fn directories_are_a_question_about_the_key_space() {
        assert!(is_dir(""), "the root");
        assert!(is_dir("react"));
        assert!(is_dir("react/assets"));
        assert!(!is_dir("react/index.html"), "a file is not a directory");
        assert!(!is_dir("nonesuch"));
        // A prefix that is not a whole segment must not read as a directory.
        assert!(!is_dir("reac"));
    }

    #[test]
    fn keys_are_sorted_so_the_search_is_valid() {
        let mut sorted: Vec<&str> = keys().collect();
        let original = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(original, sorted, "build.rs must emit a sorted table");
    }

    #[test]
    fn a_key_is_relative_to_the_nominal_root_however_it_is_spelled() {
        let root = Path::new("/pkg/stackunderflow/static");
        assert_eq!(
            key_for(
                root,
                Path::new("/pkg/stackunderflow/static/react/index.html")
            )
            .as_deref(),
            Some("react/index.html")
        );
        // The shape every caller actually produces: a relative package dir with
        // `..` in it on one side, a collapsed path on the other.
        assert_eq!(
            key_for(
                Path::new("crates/stax-server/../../../stackunderflow/static"),
                Path::new("stackunderflow/static/favicon.ico")
            )
            .as_deref(),
            Some("favicon.ico")
        );
        assert_eq!(key_for(root, Path::new("/elsewhere/x")), None);
        assert_eq!(key_for(root, root).as_deref(), Some(""));
    }
}
