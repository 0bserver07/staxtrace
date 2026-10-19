//! `stax hooks` — `cli.py:5133`–`:5306`, the five human-facing hook verbs.
//!
//! Thin wrappers, exactly as in Python: the surgery lives in
//! [`stax_hooks::install`] / [`stax_hooks::repair`] and the handlers in
//! [`stax_hooks::run`]; what is here is the printing, and the printing is the
//! byte contract — the two-space indents, the column alignment of
//! `settings file:` against `hooks active:`. One deliberate departure: the
//! stale hint reads `run \`stax hooks repair\``. The reference spelled the
//! program `stackunderflow` (a string in cli.py, not `sys.argv[0]`; DIV-237
//! kept it), but since the split that is advice to run a program a Rust-only
//! install does not have — the hint follows the program, and the harness's
//! hook-command normalisation covers the divergence.
//!
//! # `hooks run` exists here and is not the fast path
//!
//! `stax hooks run <id>` is a parity surface: a script or a settings file that
//! says `stax hooks run …` has to work. But the hook Claude Code spawns on the
//! agent's critical path is the standalone **`stax-hooks` binary** — this crate
//! links the adapters, the report layer and clap, and a hook pays for what it
//! links at every spawn (wave 6, PERF.md). Both entry points call the same
//! [`stax_hooks::run`], so they cannot drift; only the startup cost differs.
//!
//! # `click.secho(..., fg="yellow")` prints no colour here
//!
//! Click strips styling when the stream is not a TTY, and every surface that
//! diffs these bytes captures them to a file. The three yellow lines are
//! therefore plain text, which is also what the parity harness compares.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_core::queries::pyjson::{self, Value};
use stax_hooks::install as installer;
use stax_hooks::repair as repairer;
use stax_hooks::templates;

use crate::click::Output;
use crate::pyclock;

/// `stax hooks` — the verb group.
#[derive(Debug, Args)]
pub struct HooksArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: HooksVerb,
}

/// The `hooks` verbs.
#[derive(Debug, Subcommand)]
pub enum HooksVerb {
    /// Register the staxtrace hooks in a settings.json (idempotent, backs up first).
    Install {
        /// project = .claude/settings.json in cwd's git root; user = ~/.claude/settings.json
        #[arg(long, default_value = "project", value_parser = ["project", "user"])]
        scope: String,
        /// Show what would change; write nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Store full hook payloads (prompt text, tool output) instead of sanitised
        /// metadata. Off by default — the conservative choice.
        #[arg(long = "capture-content")]
        capture_content: bool,
        /// Also install the context-injection hooks (SessionStart / UserPromptSubmit /
        /// PreToolUse) that feed StackUnderflow's memory back into the live agent.
        /// Opt-in separately from capture; off by default.
        #[arg(long)]
        inject: bool,
    },
    /// Rewrite stale StackUnderflow hook commands to the portable form (changes nothing else).
    Repair {
        /// project = cwd's git root; user = ~/.claude; all = walk $HOME for every .claude/settings.json
        #[arg(long, default_value = "project", value_parser = ["project", "user", "all"])]
        scope: String,
        /// Report stale entries; rewrite nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Internal — invoked by Claude Code. Reads the hook payload as JSON on stdin.
    Run {
        /// The hook id Claude Code is firing.
        hook_id: String,
        /// Store the full payload (set by `hooks install --capture-content`).
        #[arg(long = "capture-content")]
        capture_content: bool,
    },
    /// Show which staxtrace hooks are installed, where, and whether any are stale.
    Status {
        /// Limit to one scope (default: show both project and user).
        #[arg(long, value_parser = ["project", "user"])]
        scope: Option<String>,
        /// Output format.
        #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
        fmt: String,
    },
    /// Remove the staxtrace hooks (only ours; never the file or other tools' hooks).
    Uninstall {
        /// Which settings.json to clean.
        #[arg(long, default_value = "project", value_parser = ["project", "user"])]
        scope: String,
    },
}

/// Run a `hooks` verb.
///
/// # Errors
/// Never: a `ValueError` from the installer becomes a `ClickException`, which is
/// an [`Output`] with exit code 1.
pub fn run_hooks(args: &HooksArgs) -> Result<Output> {
    let env = env_from_process();
    Ok(match &args.verb {
        HooksVerb::Install {
            scope,
            dry_run,
            capture_content,
            inject,
        } => install(scope, *dry_run, *capture_content, *inject, &env),
        HooksVerb::Repair { scope, dry_run } => repair(scope, *dry_run, &env),
        HooksVerb::Run {
            hook_id,
            capture_content,
        } => return run_hook(hook_id, *capture_content),
        HooksVerb::Status { scope, fmt } => status(scope.as_deref(), fmt, &env),
        HooksVerb::Uninstall { scope } => uninstall(scope, &env),
    })
}

fn env_from_process() -> installer::Env {
    installer::Env {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
        store_path: stax_core::settings::store_path(),
        now_epoch_secs: pyclock::now_epoch_secs(),
    }
}

/// `click.ClickException(str(exc))`.
fn click_exception(message: &str) -> Output {
    Output {
        stdout: String::new(),
        stderr: format!("Error: {message}\n"),
        code: 1,
    }
}

// ── hooks install ────────────────────────────────────────────────────────────

/// `hooks_install_cmd`.
#[must_use]
pub fn install(
    scope: &str,
    dry_run: bool,
    capture_content: bool,
    inject: bool,
    env: &installer::Env,
) -> Output {
    let report = match installer::install(scope, dry_run, capture_content, inject, env) {
        Ok(report) => report,
        Err(message) => return click_exception(&message),
    };
    let verb = if dry_run {
        "Would install"
    } else if report.changed {
        "Installed"
    } else {
        "Already installed"
    };
    let mut out = format!("{verb} staxtrace hooks ({scope} scope)\n");
    out.push_str(&format!("  settings file:   {}\n", report.settings_path));

    if dry_run {
        if report.changed {
            out.push_str("  would write the 'hooks' block:\n");
            let block = templates::canonical_hooks_block(capture_content, inject);
            let wrapped = Value::Object(vec![("hooks".to_owned(), block)]);
            for line in pyjson::dumps_indent2(&wrapped).split('\n') {
                out.push_str(&format!("    {line}\n"));
            }
            if !report.stale_entries_replaced.is_empty() {
                out.push_str(&format!(
                    "  would replace stale entries: {}\n",
                    sorted_unique(&report.stale_entries_replaced).join(", ")
                ));
            }
            out.push_str(&format!(
                "  would preserve {} non-staxtrace hook entry(ies)\n",
                report.other_hooks_preserved
            ));
        } else {
            out.push_str("  no change — already up to date.\n");
        }
        return Output::ok(out);
    }

    if let Some(backup) = &report.backup_path {
        out.push_str(&format!("  backup written:  {backup}\n"));
    }
    if report.created_file {
        out.push_str("  (created a new settings.json)\n");
    }
    out.push_str(&format!(
        "  hooks active:    {}\n",
        report.hooks_installed.join(", ")
    ));
    if report.inject {
        out.push_str(
            "  injection:       on — memory is fed back to the agent on SessionStart / UserPromptSubmit / PreToolUse\n",
        );
    }
    if !report.stale_entries_replaced.is_empty() {
        out.push_str(&format!(
            "  replaced stale:  {}\n",
            sorted_unique(&report.stale_entries_replaced).join(", ")
        ));
    }
    out.push_str(&format!(
        "  preserved:       {} non-staxtrace hook entry(ies)\n",
        report.other_hooks_preserved
    ));
    if report.capture_content {
        out.push_str(
            "  ⚠  --capture-content: full payloads (incl. prompt text & tool output) will be stored.\n",
        );
    }
    if !report.captured_events_table_ready {
        out.push_str(
            "  note: couldn't pre-create the captured_events table; it'll be created on first hook fire.\n",
        );
    }
    if crate::backup::which("stax-hooks").is_none() {
        out.push_str(
            "  note: 'stax-hooks' isn't on your PATH — Claude Code may not be able to run the hook command. Install all three binaries (stax, stax-server, stax-hooks) into one PATH directory.\n",
        );
    }
    Output::ok(out)
}

/// `', '.join(sorted(set(xs)))`.
fn sorted_unique(items: &[String]) -> Vec<String> {
    let mut out = items.to_vec();
    out.sort();
    out.dedup();
    out
}

// ── hooks uninstall ──────────────────────────────────────────────────────────

/// `hooks_uninstall_cmd`.
#[must_use]
pub fn uninstall(scope: &str, env: &installer::Env) -> Output {
    let report = match installer::uninstall(scope, env) {
        Ok(report) => report,
        Err(message) => return click_exception(&message),
    };
    if !report.file_existed {
        return Output::ok(format!(
            "No settings.json at {} — nothing to uninstall.\n",
            report.settings_path
        ));
    }
    if !report.changed {
        return Output::ok(format!(
            "No staxtrace hooks in {} — nothing to remove.\n",
            report.settings_path
        ));
    }
    let mut out = format!("Removed staxtrace hooks ({scope} scope)\n");
    out.push_str(&format!("  settings file:  {}\n", report.settings_path));
    out.push_str(&format!(
        "  backup written: {}\n",
        report.backup_path.clone().unwrap_or_default()
    ));
    out.push_str(&format!(
        "  removed:        {}\n",
        sorted_unique(&report.hooks_removed).join(", ")
    ));
    out.push_str(&format!(
        "  preserved:      {} non-staxtrace hook entry(ies)\n",
        report.other_hooks_preserved
    ));
    Output::ok(out)
}

// ── hooks status ─────────────────────────────────────────────────────────────

/// `hooks_status_cmd`.
#[must_use]
pub fn status(scope: Option<&str>, fmt: &str, env: &installer::Env) -> Output {
    let entries = match installer::status(scope, env) {
        Ok(entries) => entries,
        Err(message) => return click_exception(&message),
    };

    if fmt == "json" {
        // `json.dumps(payload, indent=2, sort_keys=True)` — sorted at every
        // level, so the per-scope keys come out
        // `exists, hooks, other_hook_count, settings_path, stale, valid_json`.
        let mut scopes: Vec<_> = entries.iter().collect();
        scopes.sort_by(|a, b| a.0.cmp(&b.0));
        let payload = Value::Object(
            scopes
                .into_iter()
                .map(|(name, entry)| {
                    let mut hooks = entry.hooks.clone();
                    hooks.sort_by(|a, b| a.0.cmp(&b.0));
                    (
                        name.clone(),
                        Value::Object(vec![
                            ("exists".to_owned(), Value::Bool(entry.exists)),
                            (
                                "hooks".to_owned(),
                                Value::Object(
                                    hooks
                                        .into_iter()
                                        .map(|(id, capture)| (id, Value::Bool(capture)))
                                        .collect(),
                                ),
                            ),
                            (
                                "other_hook_count".to_owned(),
                                Value::Int(
                                    i64::try_from(entry.other_hook_count).unwrap_or(i64::MAX),
                                ),
                            ),
                            (
                                "settings_path".to_owned(),
                                Value::Str(entry.settings_path.clone()),
                            ),
                            (
                                "stale".to_owned(),
                                Value::Array(
                                    entry
                                        .stale
                                        .iter()
                                        .map(|id| Value::Str(id.clone()))
                                        .collect(),
                                ),
                            ),
                            ("valid_json".to_owned(), Value::Bool(entry.valid_json)),
                        ]),
                    )
                })
                .collect(),
        );
        return Output::ok(format!("{}\n", pyjson::dumps_indent2(&payload)));
    }

    let mut out = String::new();
    for (name, entry) in &entries {
        out.push_str(&format!("[{name}]  {}\n", entry.settings_path));
        if !entry.exists {
            out.push_str("  (no settings.json)\n");
            continue;
        }
        if !entry.valid_json {
            out.push_str("  ⚠  not valid JSON — fix or remove it before installing.\n");
            continue;
        }
        if entry.hooks.is_empty() {
            out.push_str("  no staxtrace hooks installed.\n");
        } else {
            let mut hooks = entry.hooks.clone();
            hooks.sort_by(|a, b| a.0.cmp(&b.0));
            for (hook_id, capture_content) in hooks {
                let mut tags: Vec<&str> = Vec::new();
                if capture_content {
                    tags.push("capture-content");
                }
                if entry.stale.contains(&hook_id) {
                    tags.push("STALE — run `stax hooks repair`");
                }
                let suffix = if tags.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", tags.join(", "))
                };
                out.push_str(&format!("  ✓ {hook_id}{suffix}\n"));
            }
        }
        out.push_str(&format!(
            "  ({} non-staxtrace hook entry(ies) in this file)\n",
            entry.other_hook_count
        ));
    }
    Output::ok(out)
}

// ── hooks repair ─────────────────────────────────────────────────────────────

/// `hooks_repair_cmd`.
#[must_use]
pub fn repair(scope: &str, dry_run: bool, env: &installer::Env) -> Output {
    let report = match repairer::repair(scope, dry_run, env) {
        Ok(report) => report,
        Err(message) => return click_exception(&message),
    };
    let n = report.repaired.len();
    let mut out = String::new();
    if scope == "all" {
        out.push_str(&format!(
            "Scanned {} settings.json file(s) under $HOME ({} dir(s) pruned).\n",
            report.scanned_files.len(),
            report.pruned_dirs
        ));
    } else {
        out.push_str(&format!(
            "Scanned: {}\n",
            report
                .scanned_files
                .first()
                .map_or("(none)", String::as_str)
        ));
    }
    if n == 0 {
        out.push_str("No stale staxtrace hook commands found.\n");
        return Output::ok(out);
    }
    let verb = if dry_run { "Would rewrite" } else { "Rewrote" };
    out.push_str(&format!(
        "{verb} {n} stale command(s) across {} file(s):\n",
        report.files_changed()
    ));
    for entry in &report.repaired {
        out.push_str(&format!("  {}\n", entry.file));
        out.push_str(&format!("    {}: {}\n", entry.hook_id, entry.old));
        out.push_str(&format!("      → {}\n", entry.new));
    }
    if !dry_run && !report.backups.is_empty() {
        out.push_str(&format!("  backups written: {}\n", report.backups.len()));
    }
    Output::ok(out)
}

// ── hooks run ────────────────────────────────────────────────────────────────

/// `hooks_run_cmd` — the same [`stax_hooks::run`] the sidecar binary calls.
///
/// # Errors
/// Never; the exit code is `sys.exit(_run(...))`, which is always 0.
pub fn run_hook(hook_id: &str, capture_content: bool) -> Result<Output> {
    use std::io::{IsTerminal as _, Read as _};

    let mut raw = String::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut bytes = Vec::new();
        if stdin.lock().read_to_end(&mut bytes).is_ok() {
            raw = String::from_utf8(bytes).unwrap_or_default();
        }
    }
    let payload = if raw.trim().is_empty() {
        Value::Object(Vec::new())
    } else {
        match pyjson::loads(&raw) {
            Some(value @ Value::Object(_)) => value,
            _ => Value::Object(Vec::new()),
        }
    };
    let fired = stax_hooks::run(
        hook_id,
        &payload,
        capture_content,
        &stax_hooks::HookEnv::from_process(),
    );
    Ok(Output {
        stdout: fired.stdout,
        stderr: String::new(),
        code: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-cli-hooks-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            // Keep project-scope discovery inside this disposable fixture even
            // when TMPDIR itself is beneath another repository.
            std::fs::create_dir(dir.join(".git")).unwrap();
            Self(dir)
        }
        fn env(&self) -> installer::Env {
            installer::Env {
                cwd: self.0.clone(),
                home: self.0.join("home"),
                store_path: self.0.join("store.db"),
                now_epoch_secs: 1_785_521_045,
            }
        }
        fn write_settings(&self, text: &str) {
            let path = self.0.join(".claude").join("settings.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn a_dry_run_prints_the_block_it_would_write() {
        let scratch = Scratch::new("dry");
        let out = install("project", true, false, false, &scratch.env());
        assert_eq!(out.code, 0);
        assert!(
            out.stdout
                .starts_with("Would install staxtrace hooks (project scope)\n")
        );
        assert!(out.stdout.contains("  would write the 'hooks' block:\n"));
        assert!(out.stdout.contains("    {\n"), "{}", out.stdout);
        assert!(
            out.stdout.contains("      \"PostToolUse\": ["),
            "{}",
            out.stdout
        );
        assert!(
            out.stdout
                .ends_with("  would preserve 0 non-staxtrace hook entry(ies)\n"),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn a_second_install_says_already_installed() {
        let scratch = Scratch::new("again");
        let _ = install("project", false, false, false, &scratch.env());
        let out = install("project", false, false, false, &scratch.env());
        assert!(out.stdout.starts_with("Already installed staxtrace hooks"));
        assert!(!out.stdout.contains("backup written"));
    }

    #[test]
    fn the_capture_content_warning_is_plain_text() {
        let scratch = Scratch::new("capture");
        let out = install("project", false, true, false, &scratch.env());
        assert!(
            out.stdout.contains("  ⚠  --capture-content:"),
            "{}",
            out.stdout
        );
        assert!(
            !out.stdout.contains('\u{1b}'),
            "styling leaked into the bytes"
        );
    }

    #[test]
    fn uninstall_of_an_absent_file_names_the_path() {
        let scratch = Scratch::new("absent");
        let out = uninstall("project", &scratch.env());
        assert!(out.stdout.starts_with("No settings.json at "));
        assert!(out.stdout.ends_with(" — nothing to uninstall.\n"));
    }

    #[test]
    fn uninstall_of_a_clean_file_says_nothing_to_remove() {
        let scratch = Scratch::new("clean");
        scratch.write_settings("{}");
        let out = uninstall("project", &scratch.env());
        assert!(
            out.stdout.contains(" — nothing to remove.\n"),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn status_text_lists_installed_hooks_with_a_tick() {
        let scratch = Scratch::new("status");
        let _ = install("project", false, false, false, &scratch.env());
        let out = status(Some("project"), "text", &scratch.env());
        assert!(out.stdout.starts_with("[project]  "));
        assert_eq!(out.stdout.matches("  ✓ ").count(), 4, "{}", out.stdout);
        assert!(
            out.stdout
                .ends_with("  (0 non-staxtrace hook entry(ies) in this file)\n")
        );
    }

    #[test]
    fn status_json_is_sorted_at_every_level() {
        let scratch = Scratch::new("statusjson");
        let out = status(None, "json", &scratch.env());
        let exists = out.stdout.find("\"exists\"").unwrap();
        let hooks = out.stdout.find("\"hooks\"").unwrap();
        let valid = out.stdout.find("\"valid_json\"").unwrap();
        assert!(exists < hooks && hooks < valid, "{}", out.stdout);
        assert!(
            out.stdout.starts_with("{\n  \"project\": {\n"),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn status_flags_a_file_that_is_not_json() {
        let scratch = Scratch::new("badjson");
        scratch.write_settings("{oops");
        let out = status(Some("project"), "text", &scratch.env());
        assert!(
            out.stdout
                .contains("  ⚠  not valid JSON — fix or remove it before installing.\n"),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn repair_reports_the_scanned_file_even_when_clean() {
        let scratch = Scratch::new("repairclean");
        let out = repair("project", false, &scratch.env());
        assert!(out.stdout.starts_with("Scanned: "));
        assert!(
            out.stdout
                .ends_with("No stale staxtrace hook commands found.\n")
        );
    }

    #[test]
    fn repair_prints_the_arrow_block_for_each_rewrite() {
        let scratch = Scratch::new("repairstale");
        scratch.write_settings(
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/old/bin/stackunderflow hook run staxtrace-stop"}]}]}}"#,
        );
        let out = repair("project", true, &scratch.env());
        assert!(
            out.stdout
                .contains("Would rewrite 1 stale command(s) across 1 file(s):\n")
        );
        assert!(
            out.stdout
                .contains("      → stax-hooks run staxtrace-stop\n")
        );
        assert!(!out.stdout.contains("backups written"));
    }

    #[test]
    fn an_unknown_scope_is_a_click_exception_on_stderr() {
        let scratch = Scratch::new("scope");
        let out = install("global", false, false, false, &scratch.env());
        assert_eq!(out.code, 1);
        assert!(out.stdout.is_empty());
        assert_eq!(
            out.stderr,
            "Error: scope must be one of ('project', 'user'), got 'global'\n"
        );
    }
}
