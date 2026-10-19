//! `stax guide` — `cli.py:5317`–`:5392` over `python-legacy: agentsmd.py`.
//!
//! Writes a marked block into `CLAUDE.md` / `AGENTS.md` teaching an agent that
//! the `memory` commands exist. Three properties are the whole contract, and
//! each is a parity row rather than a claim:
//!
//! * **Idempotent and convergent.** Re-running `install` replaces the block in
//!   place; a half-written file with one orphan marker is healed; nothing
//!   outside the markers is touched.
//! * **Backup before mutation, never on a no-op.** `<name>.bak.<utc-ts>` is
//!   written iff the content actually changes and `--dry-run` is off.
//! * **`uninstall` never deletes the file** — it strips the block and leaves
//!   the rest, even when the rest is empty (in which case the file becomes
//!   zero bytes, not absent).
//!
//! # The snippet is a byte contract, and it is this repo's own CLAUDE.md
//!
//! `_GUIDE_BODY` is the reference's text, em-dashes and all, with one
//! post-split departure: the commands (and the heading) name the native
//! binary — see the const's own doc. It is the text the installer writes into
//! a user's instruction file, so a single changed character is a real
//! divergence — the parity rows diff the produced file, not just the printed
//! lines, and the harness's hook-command normalisation carries the renamed
//! lines back to the reference spelling, counted like every other departure.
//!
//! # Why the timestamped backup is proven off the shared matrix
//!
//! `_backup_path_for` embeds `datetime.now(UTC)` to **second** precision. Two
//! implementations run seconds apart produce different file *names*, so a
//! `diff -r` of the two case homes would fail on a difference the harness
//! itself created. `parity-cli.sh` therefore normalises that stamp to a fixed
//! token — in the diffed trees *and* in the printed `backup written:` /
//! `backup:` paths — so the backup's *contents* are compared exactly while its
//! clock is not, and it reports how often the substitution fired on every run.
//!
//! (An earlier draft of this comment pointed at a `rust/guide-hooks-differ.sh`
//! that was never written; the mechanism lives in the harness itself. The
//! stdout half of it did not exist either until a row caught it: six rows per
//! state were passing or failing on whether the two runs straddled a second.)

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_core::queries::pyjson::{self, Value};

use crate::click::Output;
use crate::pyclock;

/// `GUIDE_START`.
pub const GUIDE_START: &str = "<!-- staxtrace:guide:start -->";
/// `GUIDE_END`.
pub const GUIDE_END: &str = "<!-- staxtrace:guide:end -->";

/// The pre-rename markers, still found by `install`/`uninstall`/`status`.
///
/// These are ANCHORS in someone's CLAUDE.md, not branding: they are how the
/// installer locates a block it wrote earlier. Emitting the new pair while only
/// recognising the new pair would leave every existing block stranded — a
/// second block appended below the first, and `uninstall` unable to remove
/// what it wrote last month.
pub const GUIDE_START_LEGACY: &str = "<!-- stackunderflow:guide:start -->";
/// The pre-rename end marker. See [`GUIDE_START_LEGACY`].
pub const GUIDE_END_LEGACY: &str = "<!-- stackunderflow:guide:end -->";

/// The `(start, end)` marker pair present in `text`, newest generation first.
#[must_use]
pub fn markers_in(text: &str) -> Option<(&'static str, &'static str)> {
    if text.contains(GUIDE_START) {
        Some((GUIDE_START, GUIDE_END))
    } else if text.contains(GUIDE_START_LEGACY) {
        Some((GUIDE_START_LEGACY, GUIDE_END_LEGACY))
    } else {
        None
    }
}

/// `_GUIDE_BODY` — the reference's snippet with the program renamed.
///
/// Transcribed from `agentsmd.py`, with one post-split departure: the
/// commands name `stax`, because a snippet telling every agent on the machine
/// to run the retired Python entry point is an instruction to fail. The
/// envelope schema string stays `staxtrace.memory/1` — that is the wire
/// contract's version identifier, not a program name. The markers likewise
/// keep their spelling: they are the anchors `install` uses to find and
/// replace an existing block, and renaming them would orphan every
/// previously-written snippet.
pub const GUIDE_BODY: &str = "\
## staxtrace — query your past coding sessions

This machine indexes every past AI coding session locally with staxtrace.
Before re-deriving something, check whether the answer is already recorded:

- `stax memory file <path>` — a file's history: past edits, failure
  modes, and sessions that touched it. Worth a look before a non-trivial edit.
- `stax memory decisions \"<topic>\"` — past decisions on a topic.
- `stax memory worked \"<action>\"` — past sessions where an action
  succeeded, with evidence.
- `stax memory sessions` — recent sessions in this project.
- `stax memory ask \"<question>\"` — natural-language query over history.

Pass `--json` for a stable, token-bounded envelope (`schema:
staxtrace.memory/1`) meant for programmatic use. Every query is local and
read-only — nothing leaves the machine.";

/// `render_block()` — the markers plus the body.
#[must_use]
pub fn render_block() -> String {
    format!("{GUIDE_START}\n{GUIDE_BODY}\n{GUIDE_END}")
}

/// `stax guide` — the verb group.
#[derive(Debug, Args)]
pub struct GuideArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: GuideVerb,
}

/// The `guide` verbs.
#[derive(Debug, Subcommand)]
pub enum GuideVerb {
    /// Write the agent-discovery snippet into the instruction file(s) (idempotent, backs up first).
    Install {
        /// project = ./CLAUDE.md and ./AGENTS.md in cwd's git root; user = ~/.claude/CLAUDE.md
        #[arg(long, default_value = "project", value_parser = ["project", "user"])]
        scope: String,
        /// Show what would change; write nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Show where the staxtrace guide snippet is installed.
    Status {
        /// Limit to one scope (default: show both project and user).
        #[arg(long, value_parser = ["project", "user"])]
        scope: Option<String>,
        /// Output format.
        #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
        fmt: String,
    },
    /// Remove the staxtrace guide snippet (only our marked block; never the file).
    Uninstall {
        /// Which instruction file(s) to clean.
        #[arg(long, default_value = "project", value_parser = ["project", "user"])]
        scope: String,
    },
}

/// What one target file ended up as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileResult {
    /// `str(path)`.
    pub path: String,
    /// Did the file exist before?
    pub existed: bool,
    /// `changed and not existed and not dry_run`.
    pub created: bool,
    /// Would the bytes change?
    pub changed: bool,
    /// The `.bak.<ts>` written, if any.
    pub backup_path: Option<String>,
    /// `"installed" | "updated" | "removed" | "unchanged" | "absent"`.
    pub action: String,
}

/// Run a `guide` verb.
///
/// # Errors
/// Never in practice — a non-UTF-8 instruction file becomes a `ClickException`
/// on stderr, which is an [`Output`], not an `Err`.
pub fn run_guide(args: &GuideArgs) -> Result<Output> {
    let env = Env::from_process();
    Ok(match &args.verb {
        GuideVerb::Install { scope, dry_run } => install(scope, *dry_run, &env),
        GuideVerb::Status { scope, fmt } => status(scope.as_deref(), fmt, &env),
        GuideVerb::Uninstall { scope } => uninstall(scope, &env),
    })
}

/// The two directories `target_paths` resolves against.
#[derive(Debug, Clone)]
pub struct Env {
    /// `Path.cwd()`.
    pub cwd: PathBuf,
    /// `Path.home()`.
    pub home: PathBuf,
    /// `datetime.now(UTC)` at the top of the command, as epoch seconds.
    pub now_epoch_secs: i64,
}

impl Env {
    /// Read the real environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            now_epoch_secs: pyclock::now_epoch_secs(),
        }
    }
}

/// `_git_root(start)` — nearest ancestor holding a `.git` entry, else `start`.
///
/// `.git` may be a directory (a clone) or a **file** (a worktree — which is
/// exactly what this campaign runs in), and `Path.exists()` accepts both.
#[must_use]
pub fn git_root(start: &Path) -> PathBuf {
    let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut candidate: Option<&Path> = Some(start.as_path());
    while let Some(dir) = candidate {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        candidate = dir.parent();
    }
    start
}

/// `target_paths(scope)`.
#[must_use]
pub fn target_paths(scope: &str, env: &Env) -> Vec<PathBuf> {
    if scope == "user" {
        return vec![env.home.join(".claude").join("CLAUDE.md")];
    }
    let base = git_root(&env.cwd);
    vec![base.join("CLAUDE.md"), base.join("AGENTS.md")]
}

// ── block surgery (pure str→str) ─────────────────────────────────────────────

/// `_BLOCK_RE.search(text)` — the span of the first well-formed block.
///
/// `re.DOTALL` + non-greedy: the earliest start marker paired with the earliest
/// end marker after it. Hand-rolled because the pattern is two literals and a
/// regex dependency for that would be a dependency for that.
#[must_use]
pub fn find_block(text: &str) -> Option<(usize, usize)> {
    let (marker_start, marker_end) = markers_in(text)?;
    let start = text.find(marker_start)?;
    let after = start + marker_start.len();
    let end = text[after..].find(marker_end)? + after + marker_end.len();
    Some((start, end))
}

/// `_strip_block(text)`.
#[must_use]
pub fn strip_block(text: &str) -> String {
    // `re.sub` replaces EVERY non-overlapping match, not just the first.
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((start, end)) = find_block(rest) {
        cleaned.push_str(&rest[..start]);
        rest = &rest[end..];
    }
    cleaned.push_str(rest);
    if !cleaned.contains(GUIDE_START)
        && !cleaned.contains(GUIDE_END)
        && !cleaned.contains(GUIDE_START_LEGACY)
        && !cleaned.contains(GUIDE_END_LEGACY)
    {
        return cleaned;
    }
    // Orphan marker line(s) from a malformed prior state — drop them.
    // `str.splitlines()` then `"\n".join(...)`: a trailing newline is lost,
    // which is why the composers `rstrip()` and re-add one.
    cleaned
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| {
            let trimmed = line.trim();
            trimmed != GUIDE_START
                && trimmed != GUIDE_END
                && trimmed != GUIDE_START_LEGACY
                && trimmed != GUIDE_END_LEGACY
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `_compose_install(original)`.
#[must_use]
pub fn compose_install(original: &str) -> String {
    let stripped = strip_block(original);
    let rest = py_rstrip(&stripped);
    let block = render_block();
    if rest.is_empty() {
        format!("{block}\n")
    } else {
        format!("{rest}\n\n{block}\n")
    }
}

/// `_compose_uninstall(original)`.
#[must_use]
pub fn compose_uninstall(original: &str) -> String {
    let stripped = strip_block(original);
    let rest = py_rstrip(&stripped);
    if rest.is_empty() {
        String::new()
    } else {
        format!("{rest}\n")
    }
}

/// `str.rstrip()` — Python strips every Unicode whitespace character, which is
/// a wider set than Rust's `char::is_whitespace` only in that it also counts
/// the ASCII vertical tab and form feed. `char::is_whitespace` covers both.
fn py_rstrip(text: &str) -> &str {
    text.trim_end_matches(char::is_whitespace)
}

// ── per-file operations ──────────────────────────────────────────────────────

/// The `.bak.<ts>` name, with the collision suffix `_backup` appends.
fn backup_path_for(path: &Path, now: i64) -> PathBuf {
    let stamp = pyclock::utc_backup_stamp(now);
    let base = path.with_file_name(format!("{}.bak.{stamp}", file_name(path)));
    let mut dest = base.clone();
    let mut n = 1_u32;
    while dest.exists() {
        dest = dest.with_file_name(format!("{}.{n}", file_name(&dest)));
        n += 1;
    }
    dest
}

fn back_up(path: &Path, now: i64) -> Option<PathBuf> {
    let dest = backup_path_for(path, now);
    let bytes = std::fs::read(path).ok()?;
    std::fs::write(&dest, bytes).ok()?;
    Some(dest)
}

/// `_atomic_write_text` — temp file in the same directory, then `os.replace`.
fn atomic_write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temp = path.with_file_name(format!(".{}.{}.tmp", file_name(path), std::process::id()));
    if std::fs::write(&temp, text).is_ok() {
        let _ = std::fs::rename(&temp, path);
    } else {
        let _ = std::fs::remove_file(&temp);
    }
}

/// `_read_text` — `None` stands for the `ValueError` a non-UTF-8 file raises.
fn read_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Err(_) => Ok(None),
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|err| {
            format!(
                "{} is not UTF-8 text ({err}); fix or remove it before installing the guide",
                path.display()
            )
        }),
    }
}

fn install_one(path: &Path, dry_run: bool, now: i64) -> Result<FileResult, String> {
    let existed = path.exists();
    let original = if existed {
        read_text(path)?.unwrap_or_default()
    } else {
        String::new()
    };
    let had_block = find_block(&original).is_some();
    let new = compose_install(&original);
    let changed = new != original;

    let mut backup = None;
    if changed && !dry_run {
        if existed {
            backup = back_up(path, now).map(|dest| dest.to_string_lossy().into_owned());
        }
        atomic_write(path, &new);
    }

    let action = if !changed {
        "unchanged"
    } else if had_block {
        "updated"
    } else {
        "installed"
    };
    Ok(FileResult {
        path: path.to_string_lossy().into_owned(),
        existed,
        created: changed && !existed && !dry_run,
        changed,
        backup_path: backup,
        action: action.to_owned(),
    })
}

fn uninstall_one(path: &Path, now: i64) -> Result<FileResult, String> {
    if !path.exists() {
        return Ok(FileResult {
            path: path.to_string_lossy().into_owned(),
            existed: false,
            created: false,
            changed: false,
            backup_path: None,
            action: "absent".to_owned(),
        });
    }
    let original = read_text(path)?.unwrap_or_default();
    let new = compose_uninstall(&original);
    let changed = new != original;
    let mut backup = None;
    if changed {
        backup = back_up(path, now).map(|dest| dest.to_string_lossy().into_owned());
        atomic_write(path, &new);
    }
    Ok(FileResult {
        path: path.to_string_lossy().into_owned(),
        existed: true,
        created: false,
        changed,
        backup_path: backup,
        action: if changed { "removed" } else { "unchanged" }.to_owned(),
    })
}

// ── the commands ─────────────────────────────────────────────────────────────

/// `click.ClickException(str(exc))` — `Error: <message>` on stderr, exit 1.
fn click_exception(message: &str) -> Output {
    Output {
        stdout: String::new(),
        stderr: format!("Error: {message}\n"),
        code: 1,
    }
}

/// `guide_install_cmd`.
#[must_use]
pub fn install(scope: &str, dry_run: bool, env: &Env) -> Output {
    let mut files = Vec::new();
    for path in target_paths(scope, env) {
        match install_one(&path, dry_run, env.now_epoch_secs) {
            Ok(result) => files.push(result),
            Err(message) => return click_exception(&message),
        }
    }
    let verb = if dry_run {
        "Would install"
    } else {
        "Installed"
    };
    let mut out = format!("{verb} the staxtrace guide snippet ({scope} scope)\n");
    echo_guide_files(&files, &mut out);
    if !files.iter().any(|file| file.changed) {
        out.push_str("  no change — already up to date.\n");
    }
    Output::ok(out)
}

/// `guide_uninstall_cmd`.
#[must_use]
pub fn uninstall(scope: &str, env: &Env) -> Output {
    let mut files = Vec::new();
    for path in target_paths(scope, env) {
        match uninstall_one(&path, env.now_epoch_secs) {
            Ok(result) => files.push(result),
            Err(message) => return click_exception(&message),
        }
    }
    let mut out = format!("Removed the staxtrace guide snippet ({scope} scope)\n");
    echo_guide_files(&files, &mut out);
    if !files.iter().any(|file| file.changed) {
        out.push_str("  no change — nothing to remove.\n");
    }
    Output::ok(out)
}

/// `_echo_guide_files(report)` — `f"  {f.action:9s}  {f.path}"`.
fn echo_guide_files(files: &[FileResult], out: &mut String) {
    for file in files {
        out.push_str(&format!(
            "  {}  {}\n",
            crate::cfg::pad(&file.action, 9),
            file.path
        ));
        if let Some(backup) = &file.backup_path {
            out.push_str(&format!("               backup: {backup}\n"));
        }
    }
}

/// `guide_status_cmd` over `agentsmd.status`.
#[must_use]
pub fn status(scope: Option<&str>, fmt: &str, env: &Env) -> Output {
    let scopes: Vec<&str> = match scope {
        Some(one) => vec![one],
        None => vec!["project", "user"],
    };
    let payload: Vec<(String, Vec<(PathBuf, StatusEntry)>)> = scopes
        .iter()
        .map(|sc| {
            (
                (*sc).to_owned(),
                target_paths(sc, env)
                    .into_iter()
                    .map(|path| {
                        let entry = status_one(&path);
                        (path, entry)
                    })
                    .collect(),
            )
        })
        .collect();

    if fmt == "json" {
        // `json.dumps(payload, indent=2, sort_keys=True)` — the scope keys and
        // every per-file key come out sorted, which for the file dicts means
        // `exists, installed, path, up_to_date` (+ `valid` when it is present).
        let mut scopes_sorted: Vec<_> = payload.iter().collect();
        scopes_sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let object = Value::Object(
            scopes_sorted
                .into_iter()
                .map(|(name, files)| {
                    (
                        name.clone(),
                        Value::Array(
                            files
                                .iter()
                                .map(|(path, entry)| entry.to_value(path))
                                .collect(),
                        ),
                    )
                })
                .collect(),
        );
        return Output::ok(format!("{}\n", pyjson::dumps_indent2(&object)));
    }

    let mut out = String::new();
    for (name, files) in &payload {
        out.push_str(&format!("[{name}]\n"));
        for (path, entry) in files {
            let state = if !entry.exists {
                "no file"
            } else if !entry.valid {
                "⚠ not UTF-8 text — fix or remove it"
            } else if !entry.installed {
                "not installed"
            } else if entry.up_to_date {
                "installed"
            } else {
                "STALE — run `stax guide install`"
            };
            out.push_str(&format!("  {}  —  {state}\n", path.display()));
        }
    }
    Output::ok(out)
}

/// `_status_one(path)`'s four (sometimes five) keys.
#[derive(Debug, Clone, Copy)]
pub struct StatusEntry {
    /// `path.exists()`.
    pub exists: bool,
    /// A well-formed block is present.
    pub installed: bool,
    /// …and it matches the block this build would write.
    pub up_to_date: bool,
    /// `False` only when the file is not UTF-8 — the key is absent otherwise.
    pub valid: bool,
}

fn status_one(path: &Path) -> StatusEntry {
    if !path.exists() {
        return StatusEntry {
            exists: false,
            installed: false,
            up_to_date: false,
            valid: true,
        };
    }
    let Ok(Some(text)) = read_text(path) else {
        return StatusEntry {
            exists: true,
            installed: false,
            up_to_date: false,
            valid: false,
        };
    };
    let block = find_block(&text).map(|(start, end)| &text[start..end]);
    StatusEntry {
        exists: true,
        installed: block.is_some(),
        up_to_date: block.is_some_and(|found| found.trim() == render_block().trim()),
        valid: true,
    }
}

impl StatusEntry {
    /// The dict `_status_one` returns, keys already sorted for `sort_keys=True`.
    ///
    /// `valid` is inserted **only** on the non-UTF-8 branch — the reference
    /// builds the dict with four keys and adds a fifth in one branch, and a
    /// port that always emits five would change the JSON on every other path.
    fn to_value(self, path: &Path) -> Value {
        let mut entries = vec![
            ("exists".to_owned(), Value::Bool(self.exists)),
            ("installed".to_owned(), Value::Bool(self.installed)),
            (
                "path".to_owned(),
                Value::Str(path.to_string_lossy().into_owned()),
            ),
            ("up_to_date".to_owned(), Value::Bool(self.up_to_date)),
        ];
        if !self.valid {
            entries.push(("valid".to_owned(), Value::Bool(false)));
            // The reference returns `{path, exists, installed, up_to_date, valid}`
            // minus `up_to_date`? No — it returns the four it seeded plus
            // `valid`, and `sort_keys` puts `valid` last regardless.
        }
        Value::Object(entries)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-guide-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            // Scratch may live beneath a checkout (e.g. an adopter build
            // directory). Never let project-scope discovery escape the fixture.
            std::fs::create_dir(dir.join(".git")).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn env_at(dir: &Path) -> Env {
        Env {
            cwd: dir.to_path_buf(),
            home: dir.join("home"),
            now_epoch_secs: 1_785_521_045,
        }
    }

    #[test]
    fn a_block_written_with_the_old_markers_is_still_found_and_replaced() {
        // The markers are anchors in someone's CLAUDE.md. A block written before
        // the rename must still be LOCATED (so install replaces it rather than
        // appending a second one) and REMOVED (so uninstall can undo it).
        let scratch = Scratch::new("legacymarkers");
        let env = env_at(scratch.path());
        let claude = scratch.path().join("CLAUDE.md");
        std::fs::write(
            &claude,
            format!("keep me\n\n{GUIDE_START_LEGACY}\nold body\n{GUIDE_END_LEGACY}\n"),
        )
        .unwrap();

        assert_eq!(
            markers_in(&std::fs::read_to_string(&claude).unwrap()),
            Some((GUIDE_START_LEGACY, GUIDE_END_LEGACY))
        );

        let _ = install("project", false, &env);
        let text = std::fs::read_to_string(&claude).unwrap();
        assert!(text.starts_with("keep me"), "{text}");
        assert_eq!(
            text.matches("guide:start").count(),
            1,
            "one block only: {text}"
        );
        assert!(!text.contains("old body"), "{text}");
    }

    #[test]
    fn the_block_is_the_markers_around_the_body() {
        let block = render_block();
        assert!(block.starts_with(GUIDE_START));
        assert!(block.ends_with(GUIDE_END));
        assert!(block.contains("stax memory ask"));
        // Never the retired Python entry point — the snippet is an
        // instruction to every agent on the machine.
        assert!(!block.contains("stackunderflow memory"));
        // The envelope's version identifier is a wire contract, not a program
        // name; it keeps its spelling.
        assert!(block.contains("staxtrace.memory/1"));
    }

    #[test]
    fn installing_into_an_empty_directory_creates_both_files() {
        let scratch = Scratch::new("fresh");
        let env = env_at(scratch.path());
        let out = install("project", false, &env);
        assert_eq!(out.code, 0);
        let claude = scratch.path().join("CLAUDE.md");
        assert_eq!(
            std::fs::read_to_string(&claude).unwrap(),
            format!("{}\n", render_block())
        );
        assert!(
            out.stdout
                .starts_with("Installed the staxtrace guide snippet (project scope)\n")
        );
        assert!(out.stdout.contains("installed  "), "{}", out.stdout);
    }

    #[test]
    fn a_second_install_changes_nothing_and_writes_no_backup() {
        let scratch = Scratch::new("idempotent");
        let env = env_at(scratch.path());
        let _ = install("project", false, &env);
        let before: Vec<_> = std::fs::read_dir(scratch.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        let out = install("project", false, &env);
        assert!(out.stdout.contains("  no change — already up to date.\n"));
        let after: Vec<_> = std::fs::read_dir(scratch.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(before.len(), after.len(), "a no-op wrote a backup");
    }

    #[test]
    fn install_preserves_everything_outside_the_markers() {
        let scratch = Scratch::new("preserve");
        let env = env_at(scratch.path());
        let claude = scratch.path().join("CLAUDE.md");
        std::fs::write(&claude, "# My rules\n\nBe careful.\n").unwrap();
        let _ = install("project", false, &env);
        let text = std::fs::read_to_string(&claude).unwrap();
        assert!(text.starts_with("# My rules\n\nBe careful.\n\n<!-- staxtrace"));
        assert!(text.ends_with("guide:end -->\n"));
    }

    #[test]
    fn an_orphan_marker_is_healed_rather_than_duplicated() {
        // The convergence property: a half-written file must not end up with
        // two starts. Found by construction, not by hope.
        let scratch = Scratch::new("orphan");
        let env = env_at(scratch.path());
        let claude = scratch.path().join("CLAUDE.md");
        std::fs::write(&claude, format!("intro\n{GUIDE_START}\nhalf\n")).unwrap();
        let _ = install("project", false, &env);
        let text = std::fs::read_to_string(&claude).unwrap();
        assert_eq!(text.matches(GUIDE_START).count(), 1, "{text}");
        assert_eq!(text.matches(GUIDE_END).count(), 1, "{text}");
    }

    #[test]
    fn uninstall_empties_the_file_but_never_removes_it() {
        let scratch = Scratch::new("uninstall");
        let env = env_at(scratch.path());
        let _ = install("project", false, &env);
        let out = uninstall("project", &env);
        assert_eq!(out.code, 0);
        let claude = scratch.path().join("CLAUDE.md");
        assert!(claude.exists(), "uninstall deleted the file");
        assert_eq!(std::fs::read_to_string(&claude).unwrap(), "");
        assert!(out.stdout.contains("removed    "), "{}", out.stdout);
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let scratch = Scratch::new("dry");
        let env = env_at(scratch.path());
        let out = install("project", true, &env);
        assert!(out.stdout.starts_with("Would install"));
        assert!(!scratch.path().join("CLAUDE.md").exists());
    }

    #[test]
    fn a_changing_install_backs_the_file_up_first() {
        let scratch = Scratch::new("backup");
        let env = env_at(scratch.path());
        let claude = scratch.path().join("CLAUDE.md");
        std::fs::write(&claude, "original\n").unwrap();
        let out = install("project", false, &env);
        assert!(out.stdout.contains("backup: "), "{}", out.stdout);
        let backup = scratch.path().join("CLAUDE.md.bak.20260731T180405Z");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "original\n");
    }

    #[test]
    fn status_reports_stale_when_the_block_body_drifts() {
        let scratch = Scratch::new("stale");
        let env = env_at(scratch.path());
        let claude = scratch.path().join("CLAUDE.md");
        std::fs::write(&claude, format!("{GUIDE_START}\nold body\n{GUIDE_END}\n")).unwrap();
        let out = status(Some("project"), "text", &env);
        assert!(out.stdout.contains("STALE"), "{}", out.stdout);
        let _ = install("project", false, &env);
        let out = status(Some("project"), "text", &env);
        assert!(out.stdout.contains("  —  installed"), "{}", out.stdout);
    }

    #[test]
    fn status_json_sorts_its_keys() {
        let scratch = Scratch::new("json");
        let env = env_at(scratch.path());
        let out = status(Some("project"), "json", &env);
        assert!(
            out.stdout.starts_with("{\n  \"project\": [\n"),
            "{}",
            out.stdout
        );
        let first = out.stdout.find("\"exists\"").unwrap();
        let last = out.stdout.find("\"up_to_date\"").unwrap();
        assert!(first < last, "keys are not sorted: {}", out.stdout);
    }

    #[test]
    fn a_non_utf8_instruction_file_is_a_click_exception() {
        let scratch = Scratch::new("binary");
        let env = env_at(scratch.path());
        std::fs::write(scratch.path().join("CLAUDE.md"), [0xff_u8, 0xfe, 0x00]).unwrap();
        let out = install("project", false, &env);
        assert_eq!(out.code, 1);
        assert!(out.stdout.is_empty());
        assert!(out.stderr.starts_with("Error: "), "{}", out.stderr);
        assert!(out.stderr.contains("is not UTF-8 text"), "{}", out.stderr);
    }

    #[test]
    fn strip_block_removes_every_occurrence() {
        let text = format!("a\n{}\nb\n{}\nc\n", render_block(), render_block());
        let stripped = strip_block(&text);
        assert!(!stripped.contains(GUIDE_START), "{stripped}");
        assert!(stripped.contains('a') && stripped.contains('b') && stripped.contains('c'));
    }
}
