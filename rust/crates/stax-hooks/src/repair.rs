//! `hooks/_repair.py` — canonicalise stale hook commands, change nothing else.
//!
//! A hook entry that references a moved venv (`/old/venv/bin/stackunderflow
//! hooks run …`) or the legacy singular `hook run` spelling still *parses* as
//! ours, so `repair` rewrites exactly that `command` string to the portable
//! form and leaves every byte around it. No hooks added, none removed, every
//! non-StackUnderflow entry untouched, a per-file backup before any mutation,
//! and `--dry-run` reports without writing.
//!
//! # The `$HOME` walk
//!
//! `--scope all` is the only verb in the whole CLI that walks the user's home
//! directory, and it is bounded three ways: never into a symlinked directory,
//! never below eight levels, never into a pruned name. The prune list is data,
//! transcribed exactly — a name missing from it is a slower scan, a name added
//! to it is a settings file the user asked to repair and did not get.
//!
//! It runs only when asked. Nothing here is reachable from an install script or
//! from any other verb.

use std::path::{Path, PathBuf};

use stax_core::queries::pyjson::Value;

use crate::install::{
    Env, atomic_write_json, back_up, count_other_hooks, entry_is_ours, read_settings,
    resolve_settings_path,
};
use crate::templates;

/// `_VALID_REPAIR_SCOPES`.
pub const VALID_REPAIR_SCOPES: [&str; 3] = ["project", "user", "all"];

/// `_PRUNE_DIRS` — transcribed, not summarised.
pub const PRUNE_DIRS: [&str; 30] = [
    "node_modules",
    ".git",
    ".npm",
    ".cache",
    ".nvm",
    ".Trash",
    ".rustup",
    ".cargo",
    ".gradle",
    ".m2",
    ".bun",
    ".deno",
    ".pnpm-store",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    ".tox",
    ".nox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".hatch",
    ".eggs",
    "build",
    "dist",
    "target",
    ".next",
    ".nuxt",
    ".svelte-kit",
];

/// The two names `_PRUNE_DIRS` carries that do not fit the list above.
///
/// `frozenset` is unordered and this port needs a stable one, so the set is
/// split rather than reordered: [`PRUNE_DIRS`] plus these two is the whole set.
/// `.parcel-cache` and `Library` are the remaining members.
pub const PRUNE_DIRS_TAIL: [&str; 2] = [".parcel-cache", "Library"];

/// `_MAX_DEPTH`.
pub const MAX_DEPTH: usize = 8;

/// `RepairReport`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepairReport {
    /// The scope asked for.
    pub scope: String,
    /// Was this a dry run?
    pub dry_run: bool,
    /// `settings.json` files inspected, in walk order.
    pub scanned_files: Vec<String>,
    /// `(file, hook_id, old, new)` for every rewritten command.
    pub repaired: Vec<RepairedCommand>,
    /// `.bak.<ts>` files written.
    pub backups: Vec<String>,
    /// Directories skipped during the walk (informational).
    pub pruned_dirs: usize,
}

impl RepairReport {
    /// `files_changed` — `len({entry["file"] for entry in repaired})`.
    #[must_use]
    pub fn files_changed(&self) -> usize {
        let mut files: Vec<&str> = self
            .repaired
            .iter()
            .map(|entry| entry.file.as_str())
            .collect();
        files.sort_unstable();
        files.dedup();
        files.len()
    }
}

/// One rewritten `command` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairedCommand {
    /// The settings file it lives in.
    pub file: String,
    /// The hook id the command names.
    pub hook_id: String,
    /// What it said.
    pub old: String,
    /// What it says now.
    pub new: String,
}

fn is_pruned(name: &str) -> bool {
    PRUNE_DIRS.contains(&name) || PRUNE_DIRS_TAIL.contains(&name)
}

/// `_scan_settings_files(root)` — `([…/.claude/settings.json], pruned_count)`.
///
/// `os.walk(topdown=True)` yields directories in `os.scandir` order and prunes
/// by mutating `dirnames` in place; this reproduces the *set* of files and the
/// pruned *count*, and sorts the result so the report is deterministic across
/// filesystems. The reference's own order is `scandir`'s, which is already
/// arbitrary — DIV-258 records the sort as the deliberate difference.
#[must_use]
pub fn scan_settings_files(root: &Path, max_depth: usize) -> (Vec<PathBuf>, usize) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut found: Vec<PathBuf> = Vec::new();
    let mut pruned = 0_usize;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root, 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut has_settings = false;
        for entry in reader.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                subdirs.push(path);
            } else if meta.is_symlink() && path.is_dir() {
                // Counted as a directory by `os.walk`, then stripped by the
                // `os.path.islink` test below.
                subdirs.push(path);
            } else if path.file_name().is_some_and(|name| name == "settings.json") {
                has_settings = true;
            }
        }
        if depth >= max_depth {
            pruned += subdirs.len();
        } else {
            for path in subdirs {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let is_link = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_symlink());
                if is_pruned(&name) || is_link {
                    pruned += 1;
                    continue;
                }
                stack.push((path, depth + 1));
            }
        }
        if has_settings && dir.file_name().is_some_and(|name| name == ".claude") {
            found.push(dir.join("settings.json"));
        }
    }
    found.sort();
    (found, pruned)
}

/// `_repaired_command(command)` — `(hook_id, canonical)` when it is stale.
#[must_use]
pub fn repaired_command(command: &str) -> Option<(String, String)> {
    let (hook_id, capture_content) = templates::parse_hook_command(command)?;
    let canonical = templates::canonical_command(&hook_id, capture_content);
    // `command.strip() == canon` — Python strips before comparing, so a command
    // that is canonical but for surrounding whitespace is NOT rewritten.
    if command.trim() == canonical {
        return None;
    }
    Some((hook_id, canonical))
}

/// `_repair_settings_obj(settings)` — `(new_settings, changes)`.
#[must_use]
pub fn repair_settings_obj(settings: &Value) -> (Value, Vec<(String, String, String)>) {
    let mut new = settings.clone();
    let mut changes: Vec<(String, String, String)> = Vec::new();
    let Value::Object(root) = &mut new else {
        return (new, changes);
    };
    let Some((_, Value::Object(events))) = root.iter_mut().find(|(name, _)| name == "hooks") else {
        return (new, changes);
    };
    for (_event, groups) in events.iter_mut() {
        let Value::Array(groups) = groups else {
            continue;
        };
        for group in groups.iter_mut() {
            let Value::Object(group_entries) = group else {
                continue;
            };
            let Some((_, Value::Array(entries))) =
                group_entries.iter_mut().find(|(name, _)| name == "hooks")
            else {
                continue;
            };
            for entry in entries.iter_mut() {
                if !matches!(entry, Value::Object(_)) {
                    continue;
                }
                if entry_is_ours(entry).is_none() {
                    continue;
                }
                let Value::Object(fields) = entry else {
                    continue;
                };
                let Some((_, Value::Str(command))) =
                    fields.iter_mut().find(|(name, _)| name == "command")
                else {
                    continue;
                };
                let Some((hook_id, fixed)) = repaired_command(command) else {
                    continue;
                };
                changes.push((hook_id, command.clone(), fixed.clone()));
                *command = fixed;
            }
        }
    }
    (new, changes)
}

/// `_repair_one_file(path, dry_run=…)`.
fn repair_one_file(
    path: &Path,
    dry_run: bool,
    now_epoch_secs: i64,
) -> (Vec<(String, String, String)>, Option<String>) {
    if !path.exists() {
        return (Vec::new(), None);
    }
    let Ok(settings) = read_settings(path) else {
        // A broken settings file is the user's to fix; we will not touch it.
        return (Vec::new(), None);
    };
    let (new_settings, changes) = repair_settings_obj(&settings);
    if changes.is_empty() {
        return (Vec::new(), None);
    }
    // The invariant: a repair must never change the count of non-ours hooks.
    if count_other_hooks(&new_settings) != count_other_hooks(&settings) {
        return (Vec::new(), None);
    }
    let mut backup_path = None;
    if !dry_run {
        backup_path = back_up(path, now_epoch_secs)
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        atomic_write_json(path, &new_settings);
    }
    (changes, backup_path)
}

/// `repair(scope, dry_run=…)`.
///
/// # Errors
/// The `ValueError` an unknown scope raises.
pub fn repair(scope: &str, dry_run: bool, env: &Env) -> Result<RepairReport, String> {
    if !VALID_REPAIR_SCOPES.contains(&scope) {
        return Err(format!(
            "scope must be one of ('project', 'user', 'all'), got '{scope}'"
        ));
    }
    let mut report = RepairReport {
        scope: scope.to_owned(),
        dry_run,
        ..RepairReport::default()
    };

    let targets: Vec<PathBuf> = if scope == "all" {
        let (files, pruned) = scan_settings_files(&env.home, MAX_DEPTH);
        report.pruned_dirs = pruned;
        files
    } else {
        vec![resolve_settings_path(scope, env)?]
    };

    for path in targets {
        report
            .scanned_files
            .push(path.to_string_lossy().into_owned());
        let (changes, backup_path) = repair_one_file(&path, dry_run, env.now_epoch_secs);
        for (hook_id, old, new) in changes {
            report.repaired.push(RepairedCommand {
                file: path.to_string_lossy().into_owned(),
                hook_id,
                old,
                new,
            });
        }
        if let Some(backup) = backup_path {
            report.backups.push(backup);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-hooks-repair-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            // Project discovery must not walk into a checkout enclosing TMPDIR.
            std::fs::write(dir.join(".git"), "test fixture boundary\n").unwrap();
            Self(dir)
        }
        fn env(&self) -> Env {
            Env {
                cwd: self.0.clone(),
                home: self.0.clone(),
                store_path: self.0.join("store.db"),
                now_epoch_secs: 1_785_521_045,
            }
        }
        fn write(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    const STALE: &str = r#"{"hooks": {"Stop": [{"hooks": [
        {"type": "command", "command": "/old/venv/bin/stax hooks run staxtrace-stop"},
        {"type": "command", "command": "someone-elses-tool"}
    ]}]}}"#;

    #[test]
    fn a_stale_absolute_path_becomes_the_portable_form() {
        let scratch = Scratch::new("basic");
        scratch.write(".claude/settings.json", STALE);
        let report = repair("project", false, &scratch.env()).unwrap();
        assert_eq!(report.repaired.len(), 1);
        assert_eq!(report.files_changed(), 1);
        assert_eq!(report.repaired[0].new, "stax-hooks run staxtrace-stop");
        assert_eq!(report.backups.len(), 1);
        let text = std::fs::read_to_string(scratch.0.join(".claude/settings.json")).unwrap();
        assert!(text.contains("someone-elses-tool"), "{text}");
        assert!(!text.contains("/old/venv"), "{text}");
    }

    #[test]
    fn a_dry_run_reports_and_writes_nothing() {
        let scratch = Scratch::new("dry");
        let path = scratch.write(".claude/settings.json", STALE);
        let before = std::fs::read_to_string(&path).unwrap();
        let report = repair("project", true, &scratch.env()).unwrap();
        assert_eq!(report.repaired.len(), 1);
        assert!(report.backups.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_canonical_file_is_left_alone() {
        let scratch = Scratch::new("clean");
        scratch.write(
            ".claude/settings.json",
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "stax-hooks run staxtrace-stop"}]}]}}"#,
        );
        let report = repair("project", false, &scratch.env()).unwrap();
        assert!(report.repaired.is_empty());
        assert!(report.backups.is_empty());
    }

    #[test]
    fn the_pre_cutover_python_form_is_stale_and_repaired() {
        // Bare-name `stackunderflow hooks run …` was the reference's canonical;
        // after the split it names a program a Rust-only install does not have,
        // so `repair` rewrites it like any other stale spelling.
        let scratch = Scratch::new("python-form");
        scratch.write(
            ".claude/settings.json",
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "stax hooks run staxtrace-stop"}]}]}}"#,
        );
        let report = repair("project", false, &scratch.env()).unwrap();
        assert_eq!(report.repaired.len(), 1);
        assert_eq!(report.repaired[0].new, "stax-hooks run staxtrace-stop");
    }

    #[test]
    fn the_capture_content_choice_survives_the_rewrite() {
        let scratch = Scratch::new("capture");
        scratch.write(
            ".claude/settings.json",
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/x/stackunderflow hook run staxtrace-stop --capture-content"}]}]}}"#,
        );
        let report = repair("project", false, &scratch.env()).unwrap();
        assert_eq!(
            report.repaired[0].new,
            "stax-hooks run staxtrace-stop --capture-content"
        );
    }

    #[test]
    fn an_unparseable_settings_file_is_skipped_not_rewritten() {
        let scratch = Scratch::new("broken");
        let path = scratch.write(".claude/settings.json", "{not json");
        let report = repair("project", false, &scratch.env()).unwrap();
        assert!(report.repaired.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }

    #[test]
    fn the_home_walk_finds_nested_settings_and_prunes_by_name() {
        let scratch = Scratch::new("walk");
        scratch.write("a/.claude/settings.json", STALE);
        scratch.write("node_modules/pkg/.claude/settings.json", STALE);
        scratch.write("b/c/d/.claude/settings.json", STALE);
        let (found, pruned) = scan_settings_files(&scratch.0, MAX_DEPTH);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(pruned >= 1, "node_modules was not pruned");
        assert!(
            found
                .iter()
                .all(|path| !path.to_string_lossy().contains("node_modules"))
        );
    }

    #[test]
    fn the_home_walk_stops_at_the_depth_budget() {
        let scratch = Scratch::new("depth");
        scratch.write("1/2/3/4/5/6/7/8/9/.claude/settings.json", STALE);
        let (found, pruned) = scan_settings_files(&scratch.0, MAX_DEPTH);
        assert!(found.is_empty(), "{found:?}");
        assert!(pruned > 0);
    }

    #[test]
    fn scope_all_repairs_every_file_it_finds() {
        let scratch = Scratch::new("all");
        scratch.write("a/.claude/settings.json", STALE);
        scratch.write("b/.claude/settings.json", STALE);
        let report = repair("all", false, &scratch.env()).unwrap();
        assert_eq!(report.scanned_files.len(), 2);
        assert_eq!(report.repaired.len(), 2);
        assert_eq!(report.files_changed(), 2);
    }

    #[test]
    fn an_unknown_scope_is_the_reference_message() {
        let scratch = Scratch::new("scope");
        let err = repair("everywhere", false, &scratch.env()).unwrap_err();
        assert_eq!(
            err,
            "scope must be one of ('project', 'user', 'all'), got 'everywhere'"
        );
    }

    #[test]
    fn the_prune_list_is_the_reference_set() {
        // 32 names, and `.claude` is deliberately NOT one of them — pruning it
        // would make `--scope all` find nothing at all.
        assert_eq!(PRUNE_DIRS.len() + PRUNE_DIRS_TAIL.len(), 32);
        assert!(!is_pruned(".claude"));
        assert!(is_pruned("Library") && is_pruned(".parcel-cache"));
    }
}
