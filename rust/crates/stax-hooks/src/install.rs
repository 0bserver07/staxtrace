//! `hooks/_install.py` — install / uninstall / status for the hook block.
//!
//! Wave 6 landed the nine hook *handlers* and deliberately left this module and
//! [`crate::repair`] open with a recorded reason ("install-time, not
//! hook-budget"). Wave 8 tranche 2 closes them, because they are what
//! `stax hooks install|uninstall|status` are.
//!
//! Every mutation obeys the same four constraints the reference's docstring
//! states, and each one is a test or a parity row here:
//!
//! * **Opt-in only.** Nothing in this crate calls [`install`] except the CLI.
//! * **Backup before mutation** — `settings.json.bak.<utc-ts>`, written iff the
//!   content actually changes, never on a no-op re-install, never under
//!   `dry_run`.
//! * **Never delete another tool's hook.** [`strip_our_hooks`] removes only
//!   entries [`crate::templates::parse_hook_command`] positively claims, and
//!   [`count_other_hooks`] is the invariant `uninstall` reports.
//! * **Scope is explicit** — `project` (`<git-root>/.claude/settings.json`) or
//!   `user` (`~/.claude/settings.json`). `all` exists only on `repair`.
//!
//! `install` is idempotent *and* convergent: it strips every pre-existing
//! StackUnderflow entry (a stale absolute path, an older `--capture-content`
//! choice, the legacy `hook run` spelling) and writes the canonical block fresh.
//!
//! # The one thing that is not pure
//!
//! `_ensure_captured_events_table_quiet` opens the real store and runs a
//! `CREATE TABLE IF NOT EXISTS`. Its result is a *printed line*, so it cannot be
//! skipped — and it is the reason the non-dry-run `hooks install` rows run
//! against a home **seeded with a `store.db`**: when the file has to be created
//! from nothing, the two implementations write different bytes at offsets
//! 96–99, which is SQLite's `SQLITE_VERSION_NUMBER` header field — 3053001 for
//! the CPython build, 3053002 for rusqlite's bundled one. Measured, one byte,
//! and the schema is identical (4 objects, equal `sqlite_master`). DIV-257.
//!
//! With an existing store the whole case-home tree is byte-identical, so the
//! rows are ordinary three-axis rows. The first attempt was NOT: it diverged at
//! byte 19 because this port opened the store without `store/db.py::connect`'s
//! `PRAGMA journal_mode = WAL`, which writes header bytes 18 and 19. The
//! `diff -r` is what caught it — an stdout-only comparison would have passed.

use std::path::{Path, PathBuf};

use stax_core::queries::pyjson::{self, Value};

use crate::templates;

/// `_VALID_SCOPES`.
pub const VALID_SCOPES: [&str; 2] = ["project", "user"];

/// `InstallReport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// The scope asked for.
    pub scope: String,
    /// `str(path)` of the settings file.
    pub settings_path: String,
    /// Was this a dry run?
    pub dry_run: bool,
    /// Did the caller ask for full payload capture?
    pub capture_content: bool,
    /// Were the injection + recall + nudge hooks installed too?
    pub inject: bool,
    /// Did (or would) the file content change?
    pub changed: bool,
    /// Was `settings.json` absent before?
    pub created_file: bool,
    /// The `.bak.<ts>` written.
    pub backup_path: Option<String>,
    /// Hook ids in the resulting config.
    pub hooks_installed: Vec<String>,
    /// Hook ids whose stale entry was rewritten.
    pub stale_entries_replaced: Vec<String>,
    /// Non-StackUnderflow hook entries left untouched.
    pub other_hooks_preserved: usize,
    /// Did the `captured_events` bootstrap succeed?
    pub captured_events_table_ready: bool,
}

/// `UninstallReport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallReport {
    /// The scope asked for.
    pub scope: String,
    /// `str(path)` of the settings file.
    pub settings_path: String,
    /// Did the file exist?
    pub file_existed: bool,
    /// Did the content change?
    pub changed: bool,
    /// The `.bak.<ts>` written.
    pub backup_path: Option<String>,
    /// Hook ids removed, in the order they were found.
    pub hooks_removed: Vec<String>,
    /// Non-StackUnderflow hook entries left untouched.
    pub other_hooks_preserved: usize,
}

/// One scope's row in `status()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// `str(path)`.
    pub settings_path: String,
    /// `path.exists()`.
    pub exists: bool,
    /// `False` when the file is not a JSON object.
    pub valid_json: bool,
    /// hook id → its `--capture-content` choice, sorted by id.
    pub hooks: Vec<(String, bool)>,
    /// Hook ids whose entry is not byte-canonical, `sorted(set(...))`.
    pub stale: Vec<String>,
    /// Non-StackUnderflow hook entries in this file.
    pub other_hook_count: usize,
}

/// Where the installer looks, injected so a test never moves the process env.
#[derive(Debug, Clone)]
pub struct Env {
    /// `Path.cwd()` — `project` scope walks up from here for `.git`.
    pub cwd: PathBuf,
    /// `Path.home()` — `user` scope is `~/.claude/settings.json`.
    pub home: PathBuf,
    /// `deps.store_path` — the `captured_events` bootstrap target.
    pub store_path: PathBuf,
    /// `datetime.now(UTC)` as epoch seconds, for the `.bak.<ts>` name.
    pub now_epoch_secs: i64,
}

/// `_git_root(start)`.
///
/// `.git` may be a directory (a clone) or a file (a worktree or submodule) —
/// `Path.exists()` accepts both, and this campaign runs inside a worktree, so
/// the file case is the one that is actually exercised.
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

/// `resolve_settings_path(scope)`.
///
/// # Errors
/// The `ValueError` an unknown scope raises, which the CLI turns into a
/// `ClickException`.
pub fn resolve_settings_path(scope: &str, env: &Env) -> Result<PathBuf, String> {
    if !VALID_SCOPES.contains(&scope) {
        return Err(format!(
            "scope must be one of ('project', 'user'), got {}",
            py_repr(scope)
        ));
    }
    if scope == "user" {
        return Ok(env.home.join(".claude").join("settings.json"));
    }
    Ok(git_root(&env.cwd).join(".claude").join("settings.json"))
}

/// `repr(s)` for the scope strings the error messages interpolate.
fn py_repr(text: &str) -> String {
    if text.contains('\'') && !text.contains('"') {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

// ── settings.json IO ─────────────────────────────────────────────────────────

/// `_read_settings(path)` — `{}` when absent.
///
/// # Errors
/// The two `ValueError`s: not valid JSON, or valid JSON that is not an object.
pub fn read_settings(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        return Ok(Value::Object(Vec::new()));
    }
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let Some(value) = pyjson::loads(&raw) else {
        // `str(json.JSONDecodeError)`, reproduced — see `crate::jsonerr`. The
        // parity row is what forced it: the generic message this used to emit
        // differed from CPython's on the very first malformed file tried.
        let detail = crate::jsonerr::decode_error(&raw)
            .unwrap_or_else(|| "Expecting value: line 1 column 1 (char 0)".to_owned());
        return Err(format!(
            "{} is not valid JSON ({detail}); fix or remove it before installing hooks",
            path.display()
        ));
    };
    match value {
        Value::Object(_) => Ok(value),
        other => Err(format!(
            "{} must contain a JSON object, found {}",
            path.display(),
            py_type_name(&other)
        )),
    }
}

fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `_backup(path)` — `<path>.bak.<utc-ts>`, with a numeric suffix on collision.
///
/// # Errors
/// Any IO failure; the reference would raise here too.
pub fn back_up(path: &Path, now_epoch_secs: i64) -> Result<PathBuf, String> {
    let stamp = utc_stamp(now_epoch_secs);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut dest = path.with_file_name(format!("{name}.bak.{stamp}"));
    let mut n = 1_u32;
    while dest.exists() {
        let current = dest
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        dest = dest.with_file_name(format!("{current}.{n}"));
        n += 1;
    }
    let bytes = std::fs::read(path).map_err(|err| err.to_string())?;
    std::fs::write(&dest, bytes).map_err(|err| err.to_string())?;
    Ok(dest)
}

/// `datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")`.
#[must_use]
pub fn utc_stamp(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// `_atomic_write_json(path, data)` — `json.dumps(data, indent=2) + "\n"`.
pub fn atomic_write_json(path: &Path, data: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = format!("{}\n", pyjson::dumps_indent2(data));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    if std::fs::write(&temp, text).is_ok() {
        let _ = std::fs::rename(&temp, path);
    } else {
        let _ = std::fs::remove_file(&temp);
    }
}

// ── hook-block surgery (pure Value→Value) ────────────────────────────────────

fn object_entries(value: &Value) -> Option<&Vec<(String, Value)>> {
    match value {
        Value::Object(entries) => Some(entries),
        _ => None,
    }
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    object_entries(value)?
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// `_entry_is_ours(entry)` — `(hook_id, capture_content)` when we recognise it.
#[must_use]
pub fn entry_is_ours(entry: &Value) -> Option<(String, bool)> {
    let Some(Value::Str(command)) = get(entry, "command") else {
        return None;
    };
    match get(entry, "type") {
        None => {}
        Some(Value::Str(kind)) if kind == "command" => {}
        Some(_) => return None,
    }
    templates::parse_hook_command(command)
}

/// `_iter_hook_entries(settings)` — every well-shaped entry, tolerantly.
fn iter_hook_entries(settings: &Value) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    let Some(Value::Object(events)) = get(settings, "hooks") else {
        return out;
    };
    for (event, groups) in events {
        let Value::Array(groups) = groups else {
            continue;
        };
        for group in groups {
            let Some(Value::Array(entries)) = get(group, "hooks") else {
                continue;
            };
            for entry in entries {
                if matches!(entry, Value::Object(_)) {
                    out.push((event.clone(), entry));
                }
            }
        }
    }
    out
}

/// `count_other_hooks(settings)`.
#[must_use]
pub fn count_other_hooks(settings: &Value) -> usize {
    iter_hook_entries(settings)
        .into_iter()
        .filter(|(_, entry)| entry_is_ours(entry).is_none())
        .count()
}

/// `detect_our_hooks(settings)` — `(hook_id, capture_content, is_canonical)`.
#[must_use]
pub fn detect_our_hooks(settings: &Value) -> Vec<(String, bool, bool)> {
    iter_hook_entries(settings)
        .into_iter()
        .filter_map(|(_, entry)| {
            let (hook_id, capture_content) = entry_is_ours(entry)?;
            let Some(Value::Str(command)) = get(entry, "command") else {
                return None;
            };
            let canonical = templates::is_canonical(command, capture_content);
            Some((hook_id, capture_content, canonical))
        })
        .collect()
}

/// `_strip_our_hooks(settings)` — `(new_settings, removed_hook_ids)`.
///
/// Empties cascade exactly as the reference makes them: a group whose `hooks`
/// list goes empty is dropped, an event whose group list goes empty is dropped,
/// and an empty `hooks` mapping is removed from the file entirely. Everything
/// else is preserved verbatim — including a malformed group, which is kept as
/// found rather than "fixed".
#[must_use]
pub fn strip_our_hooks(settings: &Value) -> (Value, Vec<String>) {
    let mut new = settings.clone();
    let mut removed: Vec<String> = Vec::new();
    let Value::Object(root) = &mut new else {
        return (new, removed);
    };
    let Some((_, hooks_value)) = root.iter_mut().find(|(name, _)| name == "hooks") else {
        return (new, removed);
    };
    let Value::Object(events) = hooks_value else {
        return (new, removed);
    };
    let mut kept_events: Vec<(String, Value)> = Vec::new();
    for (event, groups) in events.iter() {
        let Value::Array(groups) = groups else {
            // Not a list: `continue` leaves the key untouched.
            kept_events.push((event.clone(), groups.clone()));
            continue;
        };
        let mut kept_groups: Vec<Value> = Vec::new();
        for group in groups {
            let Some(Value::Array(entries)) = get(group, "hooks") else {
                kept_groups.push(group.clone());
                continue;
            };
            let mut kept_entries: Vec<Value> = Vec::new();
            for entry in entries {
                match entry {
                    Value::Object(_) => match entry_is_ours(entry) {
                        Some((hook_id, _)) => removed.push(hook_id),
                        None => kept_entries.push(entry.clone()),
                    },
                    other => kept_entries.push(other.clone()),
                }
            }
            if !kept_entries.is_empty() {
                // `{**group, "hooks": kept_entries}` — the other keys keep
                // their order and `hooks` keeps its position.
                let rebuilt: Vec<(String, Value)> = object_entries(group)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(key, value)| {
                        if key == "hooks" {
                            (key, Value::Array(kept_entries.clone()))
                        } else {
                            (key, value)
                        }
                    })
                    .collect();
                kept_groups.push(Value::Object(rebuilt));
            }
        }
        if !kept_groups.is_empty() {
            kept_events.push((event.clone(), Value::Array(kept_groups)));
        }
    }
    let empty = kept_events.is_empty();
    *hooks_value = Value::Object(kept_events);
    if empty {
        root.retain(|(name, _)| name != "hooks");
    }
    (new, removed)
}

/// `_add_our_hooks(settings, capture_content=…, inject=…)`.
///
/// # Errors
/// The two defensive `ValueError`s the reference raises when `hooks` or
/// `hooks[event]` is present but of the wrong JSON type. They reach the user as
/// a `ClickException`, so they are messages, not panics.
pub fn add_our_hooks(
    settings: &Value,
    capture_content: bool,
    inject: bool,
) -> Result<Value, String> {
    let mut new = settings.clone();
    let Value::Object(root) = &mut new else {
        return Err("settings['hooks'] must be a JSON object".to_owned());
    };
    if !root.iter().any(|(name, _)| name == "hooks") {
        root.push(("hooks".to_owned(), Value::Object(Vec::new())));
    }
    let (_, hooks_value) = root
        .iter_mut()
        .find(|(name, _)| name == "hooks")
        .ok_or_else(|| "settings['hooks'] must be a JSON object".to_owned())?;
    let Value::Object(events) = hooks_value else {
        return Err("settings['hooks'] must be a JSON object".to_owned());
    };

    let mut append = |event: &str, group: Value| -> Result<(), String> {
        if !events.iter().any(|(name, _)| name == event) {
            events.push((event.to_owned(), Value::Array(Vec::new())));
        }
        let Some((_, slot)) = events.iter_mut().find(|(name, _)| name == event) else {
            return Ok(());
        };
        match slot {
            Value::Array(items) => {
                items.push(group);
                Ok(())
            }
            _ => Err(format!(
                "settings['hooks'][{}] must be a JSON array",
                py_repr(event)
            )),
        }
    };

    for (event, _) in templates::EVENT_HOOK_IDS {
        append(event, templates::matcher_group(event, capture_content))?;
    }
    if inject {
        for (event, _) in templates::INJECT_EVENT_HOOK_IDS {
            append(event, templates::inject_matcher_group(event))?;
        }
        for (event, _) in templates::RECALL_EVENT_HOOK_IDS {
            append(event, templates::recall_matcher_group(event))?;
        }
        for (event, _) in templates::NUDGE_EVENT_HOOK_IDS {
            append(event, templates::nudge_matcher_group(event))?;
        }
    }
    Ok(new)
}

/// `json.dumps(value, sort_keys=True)` — used only for the equality test the
/// reference performs, so a canonical form is enough.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    pyjson::dumps_compact(&sorted(value))
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(entries) => {
            let mut sorted_entries: Vec<(String, Value)> = entries
                .iter()
                .map(|(key, value)| (key.clone(), sorted(value)))
                .collect();
            sorted_entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(sorted_entries)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

// ── public API ───────────────────────────────────────────────────────────────

/// `install(scope, dry_run=…, capture_content=…, inject=…)`.
///
/// # Errors
/// The reference's `ValueError`s: an unknown scope, an unparseable
/// `settings.json`, or a `hooks` key of the wrong JSON type.
pub fn install(
    scope: &str,
    dry_run: bool,
    capture_content: bool,
    inject: bool,
    env: &Env,
) -> Result<InstallReport, String> {
    if !VALID_SCOPES.contains(&scope) {
        return Err(format!(
            "scope must be one of ('project', 'user'), got {}",
            py_repr(scope)
        ));
    }
    let path = resolve_settings_path(scope, env)?;
    let existed = path.exists();
    let original = read_settings(&path)?;

    let (stripped, replaced) = strip_our_hooks(&original);
    let desired = add_our_hooks(&stripped, capture_content, inject)?;

    let changed = canonical_json(&desired) != canonical_json(&original);
    let other_count = count_other_hooks(&original);

    let mut backup_path = None;
    if changed && !dry_run {
        if existed {
            backup_path = back_up(&path, env.now_epoch_secs).ok();
        }
        atomic_write_json(&path, &desired);
    }

    let table_ready = if dry_run {
        false
    } else {
        ensure_captured_events_table_quiet(&env.store_path)
    };

    let hooks_installed: Vec<String> = if inject {
        templates::ALL_HOOK_IDS
            .iter()
            .map(|id| (*id).to_owned())
            .collect()
    } else {
        templates::HOOK_IDS
            .iter()
            .map(|id| (*id).to_owned())
            .collect()
    };

    Ok(InstallReport {
        scope: scope.to_owned(),
        settings_path: path.to_string_lossy().into_owned(),
        dry_run,
        capture_content,
        inject,
        changed,
        created_file: changed && !existed && !dry_run,
        backup_path: backup_path.map(|path| path.to_string_lossy().into_owned()),
        hooks_installed,
        stale_entries_replaced: replaced,
        other_hooks_preserved: other_count,
        captured_events_table_ready: table_ready,
    })
}

/// `uninstall(scope)`.
///
/// # Errors
/// An unknown scope, or a `settings.json` that is not a JSON object.
pub fn uninstall(scope: &str, env: &Env) -> Result<UninstallReport, String> {
    if !VALID_SCOPES.contains(&scope) {
        return Err(format!(
            "scope must be one of ('project', 'user'), got {}",
            py_repr(scope)
        ));
    }
    let path = resolve_settings_path(scope, env)?;
    if !path.exists() {
        return Ok(UninstallReport {
            scope: scope.to_owned(),
            settings_path: path.to_string_lossy().into_owned(),
            file_existed: false,
            changed: false,
            backup_path: None,
            hooks_removed: Vec::new(),
            other_hooks_preserved: 0,
        });
    }
    let original = read_settings(&path)?;
    let (stripped, removed) = strip_our_hooks(&original);
    let changed = canonical_json(&stripped) != canonical_json(&original);

    let mut backup_path = None;
    if changed {
        backup_path = back_up(&path, env.now_epoch_secs).ok();
        atomic_write_json(&path, &stripped);
    }

    Ok(UninstallReport {
        scope: scope.to_owned(),
        settings_path: path.to_string_lossy().into_owned(),
        file_existed: true,
        changed,
        backup_path: backup_path.map(|path| path.to_string_lossy().into_owned()),
        hooks_removed: removed,
        other_hooks_preserved: count_other_hooks(&stripped),
    })
}

/// `status(scope)` — one entry per scope, in `_VALID_SCOPES` order.
///
/// # Errors
/// An unknown scope.
pub fn status(scope: Option<&str>, env: &Env) -> Result<Vec<(String, StatusEntry)>, String> {
    if let Some(scope) = scope
        && !VALID_SCOPES.contains(&scope)
    {
        return Err(format!(
            "scope must be one of ('project', 'user') or None, got {}",
            py_repr(scope)
        ));
    }
    let scopes: Vec<&str> = match scope {
        Some(one) => vec![one],
        None => VALID_SCOPES.to_vec(),
    };
    let mut out = Vec::new();
    for sc in scopes {
        let path = resolve_settings_path(sc, env)?;
        let display = path.to_string_lossy().into_owned();
        if !path.exists() {
            out.push((
                sc.to_owned(),
                StatusEntry {
                    settings_path: display,
                    exists: false,
                    valid_json: true,
                    hooks: Vec::new(),
                    stale: Vec::new(),
                    other_hook_count: 0,
                },
            ));
            continue;
        }
        let Ok(settings) = read_settings(&path) else {
            out.push((
                sc.to_owned(),
                StatusEntry {
                    settings_path: display,
                    exists: true,
                    valid_json: false,
                    hooks: Vec::new(),
                    stale: Vec::new(),
                    other_hook_count: 0,
                },
            ));
            continue;
        };
        // `hooks_map[hook_id] = capture_content` — a later duplicate wins, and
        // the dict is rendered in *insertion* order by `json.dumps`, which for
        // this map is first-seen order.
        let mut hooks: Vec<(String, bool)> = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        for (hook_id, capture_content, canonical) in detect_our_hooks(&settings) {
            match hooks.iter_mut().find(|(id, _)| *id == hook_id) {
                Some(slot) => slot.1 = capture_content,
                None => hooks.push((hook_id.clone(), capture_content)),
            }
            if !canonical {
                stale.push(hook_id);
            }
        }
        stale.sort();
        stale.dedup();
        out.push((
            sc.to_owned(),
            StatusEntry {
                settings_path: display,
                exists: true,
                valid_json: true,
                hooks,
                stale,
                other_hook_count: count_other_hooks(&settings),
            },
        ));
    }
    Ok(out)
}

/// `_ensure_captured_events_table_quiet()` — best-effort, never fatal.
///
/// It goes through `store/db.py::connect`'s three PRAGMAs, not a bare
/// `Connection::open`. That is not housekeeping: `journal_mode = WAL` writes
/// bytes **18 and 19** of the SQLite header, so a port that skipped it produced
/// a `store.db` that differed from the reference's at byte 19 on a fresh
/// install — measured, then fixed. The `diff -r` of the two case homes is what
/// found it.
#[must_use]
pub fn ensure_captured_events_table_quiet(store_path: &Path) -> bool {
    if let Some(parent) = store_path.parent()
        && !parent.as_os_str().is_empty()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    let Ok(conn) = rusqlite::Connection::open(store_path) else {
        return false;
    };
    if conn.pragma_update(None, "journal_mode", "WAL").is_err()
        || conn.pragma_update(None, "synchronous", "NORMAL").is_err()
        || conn.pragma_update(None, "foreign_keys", "ON").is_err()
    {
        return false;
    }
    crate::handlers::ensure_captured_events_table(&conn).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "stax-hooks-install-{tag}-{}-{:?}",
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
                home: self.0.join("home"),
                store_path: self.0.join("store.db"),
                now_epoch_secs: 1_785_521_045,
            }
        }
        fn settings(&self) -> PathBuf {
            self.0.join(".claude").join("settings.json")
        }
        fn write_settings(&self, text: &str) {
            let path = self.settings();
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
    fn the_utc_stamp_matches_the_reference_format() {
        assert_eq!(utc_stamp(1_785_521_045), "20260731T180405Z");
        assert_eq!(utc_stamp(0), "19700101T000000Z");
    }

    #[test]
    fn a_fresh_install_writes_four_capture_hooks() {
        let scratch = Scratch::new("fresh");
        let report = install("project", false, false, false, &scratch.env()).unwrap();
        assert!(report.changed);
        assert!(report.created_file);
        assert_eq!(report.backup_path, None);
        assert_eq!(report.hooks_installed.len(), 4);
        let text = std::fs::read_to_string(scratch.settings()).unwrap();
        assert!(text.ends_with("}\n"), "{text}");
        assert!(text.contains("stax-hooks run staxtrace-stop"));
        // Never the Python entry point — the program a post-split install
        // does not have.
        assert!(!text.contains("stax hooks run"), "{text}");
    }

    #[test]
    fn install_is_idempotent_and_writes_no_second_backup() {
        let scratch = Scratch::new("idempotent");
        install("project", false, false, false, &scratch.env()).unwrap();
        let first = std::fs::read_to_string(scratch.settings()).unwrap();
        let report = install("project", false, false, false, &scratch.env()).unwrap();
        assert!(!report.changed, "a re-install reported a change");
        assert_eq!(report.backup_path, None);
        assert_eq!(std::fs::read_to_string(scratch.settings()).unwrap(), first);
    }

    #[test]
    fn install_is_convergent_over_a_stale_absolute_path() {
        let scratch = Scratch::new("stale");
        scratch.write_settings(
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/old/venv/bin/stax hooks run staxtrace-stop"}]}]}}"#,
        );
        let report = install("project", false, false, false, &scratch.env()).unwrap();
        assert!(report.changed);
        assert_eq!(report.stale_entries_replaced, vec!["staxtrace-stop"]);
        let text = std::fs::read_to_string(scratch.settings()).unwrap();
        assert!(!text.contains("/old/venv"), "{text}");
        assert_eq!(text.matches("staxtrace-stop").count(), 1, "{text}");
    }

    #[test]
    fn another_tools_hook_survives_install_and_uninstall() {
        let scratch = Scratch::new("other");
        scratch.write_settings(
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "some-other-tool --go"}]}]}}"#,
        );
        let report = install("project", false, false, false, &scratch.env()).unwrap();
        assert_eq!(report.other_hooks_preserved, 1);
        let report = uninstall("project", &scratch.env()).unwrap();
        assert_eq!(report.other_hooks_preserved, 1);
        let text = std::fs::read_to_string(scratch.settings()).unwrap();
        assert!(text.contains("some-other-tool --go"), "{text}");
        assert!(!text.contains("stax-hooks run"), "{text}");
    }

    #[test]
    fn uninstall_from_a_file_that_was_only_ours_drops_the_hooks_key() {
        let scratch = Scratch::new("cascade");
        install("project", false, false, false, &scratch.env()).unwrap();
        uninstall("project", &scratch.env()).unwrap();
        let text = std::fs::read_to_string(scratch.settings()).unwrap();
        assert_eq!(text, "{}\n", "the empties must cascade: {text}");
    }

    #[test]
    fn a_dry_run_writes_nothing_at_all() {
        let scratch = Scratch::new("dry");
        let report = install("project", true, false, false, &scratch.env()).unwrap();
        assert!(report.changed);
        assert!(!report.created_file);
        assert!(!report.captured_events_table_ready);
        assert!(!scratch.settings().exists());
    }

    #[test]
    fn inject_installs_all_nine() {
        let scratch = Scratch::new("inject");
        let report = install("project", false, false, true, &scratch.env()).unwrap();
        assert_eq!(report.hooks_installed.len(), 9);
        let settings = read_settings(&scratch.settings()).unwrap();
        assert_eq!(detect_our_hooks(&settings).len(), 9);
    }

    #[test]
    fn a_reinstall_without_inject_drops_the_injection_hooks_again() {
        // Convergence in the direction that matters: turning a feature OFF has
        // to remove its entries, not leave them orphaned.
        let scratch = Scratch::new("converge");
        install("project", false, false, true, &scratch.env()).unwrap();
        install("project", false, false, false, &scratch.env()).unwrap();
        let settings = read_settings(&scratch.settings()).unwrap();
        assert_eq!(detect_our_hooks(&settings).len(), 4);
    }

    #[test]
    fn a_settings_file_that_is_not_json_is_a_value_error() {
        let scratch = Scratch::new("badjson");
        scratch.write_settings("{not json");
        let err = install("project", false, false, false, &scratch.env()).unwrap_err();
        assert!(err.contains("is not valid JSON"), "{err}");
    }

    #[test]
    fn a_settings_file_holding_a_list_names_its_type() {
        let scratch = Scratch::new("list");
        scratch.write_settings("[1, 2]");
        let err = install("project", false, false, false, &scratch.env()).unwrap_err();
        assert!(
            err.ends_with("must contain a JSON object, found list"),
            "{err}"
        );
    }

    #[test]
    fn a_hooks_key_of_the_wrong_type_is_the_defensive_value_error() {
        let scratch = Scratch::new("hookstype");
        scratch.write_settings(r#"{"hooks": "nope"}"#);
        let err = install("project", false, false, false, &scratch.env()).unwrap_err();
        assert_eq!(err, "settings['hooks'] must be a JSON object");
    }

    #[test]
    fn an_event_that_is_not_an_array_is_named_in_the_error() {
        let scratch = Scratch::new("eventtype");
        scratch.write_settings(r#"{"hooks": {"Stop": "nope"}}"#);
        let err = install("project", false, false, false, &scratch.env()).unwrap_err();
        assert_eq!(err, "settings['hooks']['Stop'] must be a JSON array");
    }

    #[test]
    fn uninstall_on_a_missing_file_reports_absence_without_creating_it() {
        let scratch = Scratch::new("missing");
        let report = uninstall("project", &scratch.env()).unwrap();
        assert!(!report.file_existed);
        assert!(!report.changed);
        assert!(!scratch.settings().exists());
    }

    #[test]
    fn status_marks_a_stale_entry_and_a_capture_choice() {
        let scratch = Scratch::new("status");
        scratch.write_settings(
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/old/bin/stax hooks run staxtrace-stop --capture-content"}]}]}}"#,
        );
        let entries = status(Some("project"), &scratch.env()).unwrap();
        let (_, entry) = &entries[0];
        assert!(entry.exists && entry.valid_json);
        assert_eq!(entry.hooks, vec![("staxtrace-stop".to_owned(), true)]);
        assert_eq!(entry.stale, vec!["staxtrace-stop"]);
    }

    #[test]
    fn status_over_both_scopes_keeps_the_reference_order() {
        let scratch = Scratch::new("bothscopes");
        let entries = status(None, &scratch.env()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "project");
        assert_eq!(entries[1].0, "user");
    }

    #[test]
    fn an_unknown_scope_is_the_reference_message() {
        let scratch = Scratch::new("scope");
        let err = install("global", false, false, false, &scratch.env()).unwrap_err();
        assert_eq!(
            err,
            "scope must be one of ('project', 'user'), got 'global'"
        );
    }

    #[test]
    fn a_changing_install_backs_up_first() {
        let scratch = Scratch::new("bak");
        scratch.write_settings("{}");
        let report = install("project", false, false, false, &scratch.env()).unwrap();
        let backup = report.backup_path.expect("a backup");
        assert!(
            backup.ends_with("settings.json.bak.20260731T180405Z"),
            "{backup}"
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "{}");
    }

    #[test]
    fn a_backup_name_collision_gets_a_numeric_suffix() {
        let scratch = Scratch::new("collide");
        scratch.write_settings("{}");
        install("project", false, false, false, &scratch.env()).unwrap();
        // Force a second change at the same second.
        scratch.write_settings("{\"other\": 1}");
        let report = install("project", false, false, false, &scratch.env()).unwrap();
        let backup = report.backup_path.expect("a second backup");
        assert!(backup.ends_with(".1"), "{backup}");
    }

    #[test]
    fn a_malformed_group_is_preserved_rather_than_repaired() {
        let scratch = Scratch::new("malformed");
        scratch.write_settings(r#"{"hooks": {"Stop": [{"matcher": "x"}]}}"#);
        install("project", false, false, false, &scratch.env()).unwrap();
        let text = std::fs::read_to_string(scratch.settings()).unwrap();
        assert!(text.contains("\"matcher\": \"x\""), "{text}");
    }
}
