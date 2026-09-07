//! `hooks/inject.py` — the three context-injection hooks.
//!
//! Where the capture handlers WRITE `captured_events`, these READ the store and
//! hand Claude Code a small block of text to splice into the live session:
//!
//! * `SessionStart` → the recent recorded sessions in this repo,
//! * `UserPromptSubmit` → past decisions that lexically overlap the prompt,
//! * `PreToolUse` (Edit/Write/MultiEdit) → how editing this file went wrong before.
//!
//! The output is Claude Code's context-injection envelope, rendered with
//! `json.dumps` and **every default** — `", "` / `": "` separators and
//! `ensure_ascii=True`:
//!
//! ```text
//! {"hookSpecificOutput": {"hookEventName": "<Event>", "additionalContext": "<text>"}}
//! ```
//!
//! That last flag is not cosmetic here. The truncator inserts `…` on its own,
//! so the *common* case for a clipped digest is a non-ASCII character in the
//! payload, and `stax_core::queries::pyjson::dumps_default` is the writer that
//! escapes it the way CPython does (`…`). Using the HTTP writer, or
//! serde_json's, would diverge on the first clipped line.
//!
//! ## Divergences from the reference, both filed
//!
//! * **DIV-200 — the reference opens the store READ-WRITE from a hook.**
//!   `inject._connect` calls `store.db.connect`, which is
//!   `sqlite3.connect(...)` followed by `PRAGMA journal_mode = WAL` and
//!   `PRAGMA synchronous = NORMAL`. On a store that is not already in WAL that
//!   is a write to the maintainer's live database, performed from inside an
//!   agent's hook budget, by a code path whose own docstring says "Fast +
//!   read-only … no writes of our own". This port opens `SQLITE_OPEN_READ_ONLY`.
//!   No stdout effect; recorded, not hidden.
//! * **DIV-026 (inherited)** — `discovery`'s `find_*` functions bump
//!   `discovery_telemetry` for every surfaced session, so the reference writes a
//!   second time per injection fire. A read-only handle cannot, which is the
//!   ruling already on the books for the CLI verbs.

use stax_core::queries::pyjson::Value;
use stax_core::queries::{self, pyjson};
use stax_core::store::Store;

use crate::env::{HookEnv, abspath};
use crate::pystr;
use crate::templates;

// ── budgets ─────────────────────────────────────────────────────────────────

/// Look up a canonical injection id without re-running the external legacy-id
/// normalizer. The event/id table is the installed-hook authority.
fn injection_event(hook_id: &str) -> Option<&'static str> {
    templates::INJECT_EVENT_HOOK_IDS
        .iter()
        .find_map(|&(event, id)| (id == hook_id).then_some(event))
}

/// `inject._TOKEN_BUDGET` — SessionStart fires once and can afford a fuller
/// digest; the per-prompt / per-edit hooks stay lean.
#[must_use]
pub fn token_budget(hook_id: &str) -> i64 {
    match injection_event(hook_id) {
        Some("SessionStart") => 400,
        // `_clip`'s `.get(hook_id, 200)` — the default for an id with no entry.
        _ => 200,
    }
}

/// `inject._CHARS_PER_TOKEN` — the chars/4 estimate the discovery packer uses.
const CHARS_PER_TOKEN: i64 = 4;

const SESSION_START_LIMIT: i64 = 6;
const USER_PROMPT_LIMIT: i64 = 3;
const PRE_TOOL_USE_LIMIT: i64 = 3;

const SNIPPET_CHARS: usize = 140;
const EVIDENCE_CHARS: usize = 140;

/// `discovery.DEFAULT_MIN_OUTCOME_CONFIDENCE`, which
/// `find_failure_modes_for_file` defaults to and `inject` never overrides.
const DEFAULT_MIN_OUTCOME_CONFIDENCE: f64 = 0.5;

// ── public entry point ──────────────────────────────────────────────────────

/// `inject.build_injection` — the JSON envelope for *hook_id*, or `""`.
///
/// Never fails. Any failure — unknown id, bad payload, no store, query error —
/// returns `""` so the caller emits nothing and exits 0. An empty return is also
/// the normal "nothing useful to say" outcome, not just the error path; the
/// reference's blanket `except Exception` is reproduced as a `Result` that every
/// internal step folds into `""`.
#[must_use]
pub fn build_injection(hook_id: &str, payload: &Value, env: &HookEnv) -> String {
    let Some(event) = injection_event(hook_id) else {
        return String::new();
    };

    let budget = token_budget(hook_id);
    let text = match event {
        "SessionStart" => session_start_context(payload, env, budget),
        "UserPromptSubmit" => user_prompt_context(payload, env, budget),
        "PreToolUse" => pre_tool_use_context(payload, env),
        _ => Ok(String::new()),
    }
    .unwrap_or_default();

    // The agent inbox rides the same two mid-session events (agent-remotes
    // Phase 3): unseen cross-machine messages surface ahead of the memory
    // block, once each. Works even with no store — the inbox is files.
    // PreToolUse is what makes this a real interject: a message lands in a
    // *running* turn at the next tool call, not just at the next prompt.
    //
    // Placed BEFORE `clip`, as the reference places it: the inbox block is
    // inside the hook's token budget, not on top of it, so a chatty peer costs
    // the memory block its tail rather than blowing the budget. And
    // `render_for_injection` is called unconditionally for these two ids —
    // its mark-seen side effect fires even when the text it produced is
    // dropped by the emptiness check below, which is the reference's
    // behaviour and the reason a message is never announced twice.
    let text = if matches!(event, "UserPromptSubmit" | "PreToolUse") {
        let inbox = stax_core::agent_inbox::render_for_injection(Some(&env.app_dir));
        if inbox.is_empty() {
            text
        } else if text.trim().is_empty() {
            inbox
        } else {
            format!("{inbox}\n\n{text}").trim().to_string()
        }
    } else {
        text
    };

    let text = clip(&text, hook_id);
    if text.trim().is_empty() {
        return String::new();
    }
    pyjson::dumps_default(&Value::Object(vec![(
        "hookSpecificOutput".into(),
        Value::Object(vec![
            ("hookEventName".into(), Value::Str(event.to_string())),
            ("additionalContext".into(), Value::Str(text)),
        ]),
    )]))
}

// ── store access ────────────────────────────────────────────────────────────

/// `inject._connect` — the store for reading, or `None` if it isn't there yet.
///
/// No schema apply: injection is a reader. The short `busy_timeout` is the
/// reference's, and its reasoning is the reference's too — injected context is
/// nice-to-have, so a fire under writer contention skips rather than stalls the
/// agent. See DIV-200 in the module docs for the one thing that differs.
fn connect(env: &HookEnv) -> Option<Store> {
    if !env.store_path.exists() {
        return None;
    }
    let store = Store::open_read_only(&env.store_path).ok()?;
    store
        .conn()
        .busy_timeout(std::time::Duration::from_millis(250))
        .ok()?;
    Some(store)
}

// ── SessionStart: project digest ────────────────────────────────────────────

fn session_start_context(
    payload: &Value,
    env: &HookEnv,
    token_budget: i64,
) -> anyhow::Result<String> {
    let Some(cwd) = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(String::new());
    };
    let Some(store) = connect(env) else {
        return Ok(String::new());
    };
    let result = queries::find_sessions_in_path(
        store.conn(),
        cwd,
        None,
        SESSION_START_LIMIT,
        None,
        &env.budget(token_budget),
    )?;

    if result.sessions.is_empty() {
        return Ok(String::new());
    }
    let mut lines =
        vec!["[staxtrace memory] This project has prior recorded coding sessions:".to_string()];
    lines.extend(result.sessions.iter().map(session_line));
    lines.push(
        "Query this history with `stax memory sessions --json`, or \
         `memory file <path> --json` / `memory decisions \"<topic>\" --json`."
            .to_string(),
    );
    Ok(lines.join("\n"))
}

/// `inject._session_line`.
fn session_line(m: &queries::SessionMatch) -> String {
    let ts = date_or_undated(&m.last_ts);
    let provider = if m.provider.is_empty() {
        "?"
    } else {
        m.provider.as_str()
    };
    format!(
        "  • {ts}  {} msgs  ${}  [{provider}]",
        m.message_count,
        pystr::format_2f(m.cost_usd)
    )
}

/// `(getattr(m, "last_ts", "") or "")[:10] or "(undated)"`.
fn date_or_undated(last_ts: &str) -> String {
    let head = pystr::head(last_ts, 10);
    if head.is_empty() {
        "(undated)".to_string()
    } else {
        head
    }
}

// ── UserPromptSubmit: matching past decision ────────────────────────────────

/// `inject._PROMPT_STOPWORDS` — tokens too generic to be a useful substring
/// query against past message text.
const PROMPT_STOPWORDS: [&str; 42] = [
    "about",
    "after",
    "again",
    "build",
    "could",
    "current",
    "every",
    "first",
    "function",
    "instead",
    "other",
    "please",
    "really",
    "right",
    "should",
    "still",
    "stuff",
    "tests",
    "their",
    "there",
    "thing",
    "these",
    "those",
    "using",
    "where",
    "which",
    "while",
    "would",
    "write",
    "files",
    "change",
    "create",
    "delete",
    "remove",
    "update",
    "because",
    "before",
    "between",
    "implement",
    "something",
    "anything",
    "everything",
];

const MIN_TOKEN_LEN: usize = 5;
/// `inject._IDENTIFIER_CHARS` — a token carrying one of these is identifier /
/// path / dotted-name shaped. `"::"` is a two-character member, and Python's
/// `any(c in tok for c in _IDENTIFIER_CHARS)` tests it as a *substring*.
const IDENTIFIER_CHARS: [&str; 4] = ["_", ".", "/", "::"];

/// `inject._TOKEN_RE` = `[A-Za-z0-9_./:-]+`, scanned rather than compiled — the
/// class is a fixed ASCII set, so a byte scan is exactly the same automaton and
/// costs nothing at spawn.
fn tokens(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (index, byte) in bytes.iter().enumerate() {
        let member =
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'/' | b':' | b'-');
        match (member, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                out.push(&text[begin..index]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        out.push(&text[begin..]);
    }
    out
}

/// `inject._prompt_to_query` — the single most search-worthy token, or `None`.
///
/// `search_past_decisions` does a substring `LIKE` over past message text, so
/// the query has to be something that plausibly *recurs verbatim*. A prompt with
/// nothing distinctive yields `None`: inject nothing rather than match
/// everything.
#[must_use]
pub fn prompt_to_query(prompt: &str) -> Option<String> {
    if prompt.trim().is_empty() {
        return None;
    }
    let window = pystr::head(prompt.trim(), 400);
    let mut best: Option<&str> = None;
    let mut best_score = 0.0_f64;
    for raw in tokens(&window) {
        let tok = raw.trim_matches(|c| matches!(c, '.' | '/' | ':' | '-'));
        let lowered = tok.to_lowercase();
        if pystr::len_chars(tok) < MIN_TOKEN_LEN || PROMPT_STOPWORDS.contains(&lowered.as_str()) {
            continue;
        }
        let mut score = pystr::len_chars(tok) as f64;
        if IDENTIFIER_CHARS.iter().any(|needle| tok.contains(needle)) {
            score += 20.0; // file / identifier / dotted-name shape
        }
        // `any(c.isupper() for c in tok[1:])` — the camelCase / PascalCase hump,
        // skipping the first character.
        if tok.chars().skip(1).any(char::is_uppercase) {
            score += 6.0;
        }
        if score > best_score {
            best_score = score;
            best = Some(tok);
        }
    }
    best.map(str::to_string)
}

fn user_prompt_context(
    payload: &Value,
    env: &HookEnv,
    token_budget: i64,
) -> anyhow::Result<String> {
    let Some(prompt) = payload.get("prompt").and_then(Value::as_str) else {
        return Ok(String::new());
    };
    let Some(query) = prompt_to_query(prompt) else {
        return Ok(String::new());
    };
    let Some(store) = connect(env) else {
        return Ok(String::new());
    };
    let slug = slug_from_cwd(payload.get("cwd"), env);
    let result = queries::search_past_decisions(
        store.conn(),
        &query,
        slug.as_deref(),
        None,
        USER_PROMPT_LIMIT,
        &env.budget(token_budget),
    )?;

    if result.sessions.is_empty() {
        return Ok(String::new());
    }
    let mut lines = vec![format!(
        "[staxtrace memory] Past decisions here mention \"{query}\":"
    )];
    lines.extend(result.sessions.iter().map(decision_line));
    lines.push(format!(
        "Full context: `stax memory decisions \"{query}\" --json`."
    ));
    Ok(lines.join("\n"))
}

/// `inject._decision_line`.
fn decision_line(m: &queries::SessionMatch) -> String {
    let ts = date_or_undated(&m.last_ts);
    let snippet = pystr::trim(m.snippet.as_deref().unwrap_or(""), SNIPPET_CHARS);
    if snippet.is_empty() {
        format!("  • {ts}  (session {})", pystr::head(&m.session_id, 12))
    } else {
        format!("  • {ts}  {snippet}")
    }
}

// ── PreToolUse: failure modes for the file about to be edited ────────────────

fn pre_tool_use_context(payload: &Value, env: &HookEnv) -> anyhow::Result<String> {
    let Some(file_path) = edited_file_path(payload) else {
        return Ok(String::new());
    };
    let Some(store) = connect(env) else {
        return Ok(String::new());
    };
    let matches = queries::find_failure_modes_for_file(
        store.conn(),
        &file_path,
        None,
        PRE_TOOL_USE_LIMIT,
        DEFAULT_MIN_OUTCOME_CONFIDENCE,
    )?;

    if matches.is_empty() {
        return Ok(String::new());
    }
    let mut lines = vec![format!(
        "[staxtrace memory] Editing {} has gone wrong before:",
        pystr::basename(&file_path)
    )];
    lines.extend(matches.iter().map(failure_line));
    lines.push(format!(
        "Review the full history: `stax memory file {file_path} --json`."
    ));
    Ok(lines.join("\n"))
}

/// `inject._edited_file_path` — the target of an Edit/Write/MultiEdit call.
#[must_use]
pub fn edited_file_path(payload: &Value) -> Option<String> {
    let tool_input = payload.get("tool_input")?;
    if !matches!(tool_input, Value::Object(_)) {
        return None;
    }
    for key in ["file_path", "path", "notebook_path", "filename"] {
        if let Some(Value::Str(value)) = tool_input.get(key)
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// `inject._failure_line`.
fn failure_line(m: &queries::SessionMatch) -> String {
    let ts = date_or_undated(&m.last_ts);
    let (outcome, evidence) = match &m.outcome {
        Some(fields) => (
            if fields.outcome.is_empty() {
                "?".to_string()
            } else {
                fields.outcome.clone()
            },
            pystr::trim(&fields.outcome_evidence, EVIDENCE_CHARS),
        ),
        // `getattr(m, "outcome", "?") or "?"` on a row with no outcome at all.
        None => ("?".to_string(), String::new()),
    };
    if evidence.is_empty() {
        format!("  • {ts}  {outcome}")
    } else {
        format!("  • {ts}  {outcome}: {evidence}")
    }
}

// ── small utils ─────────────────────────────────────────────────────────────

/// `inject._slug_from_cwd` — the Claude-style project slug for a `cwd`.
///
/// `/Users/a/b` → `-Users-a-b`. Best-effort by design: a `cwd` that is a
/// *subdirectory* of the project root encodes to a slug that will not match, in
/// which case the project scope simply yields no rows. Mirrors the encoding
/// `handlers::resolve_project_id` uses.
#[must_use]
pub fn slug_from_cwd(cwd: Option<&Value>, env: &HookEnv) -> Option<String> {
    let cwd = cwd?.as_str()?;
    if cwd.is_empty() {
        return None;
    }
    Some(slugify(&abspath(cwd, &env.cwd)))
}

/// `abspath.rstrip(os.sep).replace(os.sep, "-").replace("_", "-")`.
#[must_use]
pub fn slugify(absolute: &str) -> String {
    // `.replace(os.sep, "-").replace("_", "-")` — two passes in the reference,
    // one here because both source characters map to the same target, so the
    // second pass can never see output of the first.
    absolute
        .trim_end_matches('/')
        .chars()
        .map(|ch| if ch == '/' || ch == '_' { '-' } else { ch })
        .collect()
}

/// `inject._clip` — hard-clip to the hook's token budget (the chars/4 estimate).
///
/// The real, unconditional bound: `context_budget` caps row *count*, but the
/// rendered text is what lands in the context window, so it gets the final say.
#[must_use]
pub fn clip(text: &str, hook_id: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let max_chars = token_budget(hook_id) * CHARS_PER_TOKEN;
    pystr::clip(text, usize::try_from(max_chars).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before the epoch")
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "stax-inject-dispatch-{tag}-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create injection scratch directory");
            Self { path }
        }

        fn store(&self) -> PathBuf {
            self.path.join("store.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn env() -> HookEnv {
        HookEnv {
            store_path: PathBuf::from("/nonexistent/store.db"),
            app_dir: PathBuf::from("/nonexistent"),
            weights: (0.5, 0.2, 0.3),
            now_micros: 1_785_456_000_000_000,
            cwd: PathBuf::from("/home/u/proj"),
            config: None,
            proactive_disabled: None,
            recall_timeout: None,
            memory_bin: "stackunderflow".into(),
            proactive: crate::env::ProactiveSettings::default(),
        }
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        )
    }

    fn seeded_env(tag: &str) -> (Scratch, HookEnv) {
        let scratch = Scratch::new(tag);
        let project_dir = scratch.path.join("project");
        fs::create_dir_all(project_dir.join("src")).expect("create seeded project tree");
        let project_dir = fs::canonicalize(project_dir).expect("resolve seeded project tree");
        let project_path = project_dir.to_string_lossy().into_owned();
        let file_path = project_dir
            .join("src/lib.rs")
            .to_string_lossy()
            .into_owned();
        let env = HookEnv {
            store_path: scratch.store(),
            app_dir: scratch.path.clone(),
            cwd: project_dir,
            ..env()
        };
        let conn = rusqlite::Connection::open(&env.store_path).expect("create injection store");
        stax_core::schema::apply(&conn).expect("apply production schema");
        conn.execute(
            "INSERT INTO projects \
             (id, provider, slug, path, display_name, first_seen, last_modified) \
             VALUES (1, 'local', ?, ?, 'project', 0, 0)",
            rusqlite::params![slugify(&project_path), project_path],
        )
        .expect("seed project");
        conn.execute_batch(
            r#"
            INSERT INTO sessions
              (id, project_id, session_id, first_ts, last_ts, message_count)
            VALUES
              (1, 1, 'seeded-session',
               '2026-07-30T09:00:00+00:00', '2026-07-30T10:00:00+00:00', 3);

            INSERT INTO session_mart
              (session_id, project_id, provider, first_ts, last_ts, message_count, cost_usd)
            VALUES
              ('seeded-session', 1, 'local',
               '2026-07-30T09:00:00+00:00', '2026-07-30T10:00:00+00:00', 3, 0.25);
            "#,
        )
        .expect("seed session evidence");

        let partition: String = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND (name GLOB 'messages_[0-9][0-9][0-9][0-9][0-9][0-9]' \
                      OR name = 'messages_unknown') \
                 ORDER BY name LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("production schema must expose a message partition");
        let suffix = partition
            .strip_prefix("messages_")
            .expect("trusted partition prefix");
        assert!(
            suffix == "unknown"
                || (suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_digit())),
            "reject an untrusted partition identifier"
        );
        let insert = format!(
            "INSERT INTO \"{partition}\" \
             (id, session_fk, seq, timestamp, role, content_text, tools_json, raw_json) \
             VALUES (?, 1, ?, ?, ?, ?, ?, '{{}}')"
        );
        conn.execute(
            &insert,
            rusqlite::params![
                1_i64,
                1_i64,
                "2026-07-30T09:10:00+00:00",
                "user",
                "keep canonicalDispatchToken in the selected path",
                "[]"
            ],
        )
        .expect("seed decision message");
        let tools_json = pyjson::dumps_default(&Value::Array(vec![obj(&[
            ("name", Value::Str("Edit".into())),
            ("input", obj(&[("file_path", Value::Str(file_path))])),
        ])]));
        conn.execute(
            &insert,
            rusqlite::params![
                2_i64,
                2_i64,
                "2026-07-30T09:20:00+00:00",
                "assistant",
                "",
                tools_json
            ],
        )
        .expect("seed file edit message");
        conn.execute(
            &insert,
            rusqlite::params![
                3_i64,
                3_i64,
                "2026-07-30T09:30:00+00:00",
                "user",
                "that broke the build",
                "[]"
            ],
        )
        .expect("seed failure response");
        let visible: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .expect("read production messages view");
        assert_eq!(visible, 3, "the production view must expose seeded rows");
        drop(conn);
        (scratch, env)
    }

    #[test]
    fn an_unknown_id_injects_nothing() {
        assert_eq!(build_injection("nope", &Value::Object(vec![]), &env()), "");
        // A capture id is not an injection id, even though it HAS an event.
        assert_eq!(
            build_injection("staxtrace-stop", &Value::Object(vec![]), &env()),
            ""
        );
    }

    #[test]
    fn a_missing_store_injects_nothing_rather_than_failing() {
        let payload = Value::Object(vec![("cwd".into(), Value::Str("/home/u/proj".into()))]);
        assert_eq!(
            build_injection("staxtrace-inject-session-start", &payload, &env()),
            ""
        );
    }

    #[test]
    fn canonical_query_errors_and_malformed_payloads_are_silent() {
        let scratch = Scratch::new("query-failure");
        let broken = HookEnv {
            store_path: scratch.store(),
            app_dir: scratch.path.clone(),
            ..env()
        };
        drop(
            rusqlite::Connection::open(&broken.store_path)
                .expect("create a store without the production schema"),
        );
        let payload = obj(&[("cwd", Value::Str("/project".into()))]);
        assert_eq!(
            crate::handlers::run("staxtrace-inject-session-start", &payload, false, &broken,)
                .stdout,
            ""
        );
        assert_eq!(
            crate::handlers::run(
                "staxtrace-inject-user-prompt",
                &Value::Str("malformed".into()),
                false,
                &broken,
            )
            .stdout,
            ""
        );
    }

    #[test]
    fn every_canonical_injection_and_legacy_command_reaches_the_same_envelope() {
        let (_scratch, env) = seeded_env("matrix");
        let project_path = env.cwd.to_string_lossy().into_owned();
        let source_dir = env.cwd.join("src").to_string_lossy().into_owned();
        let file_path = env.cwd.join("src/lib.rs").to_string_lossy().into_owned();
        let cases = [
            (
                "staxtrace-inject-session-start",
                "stackunderflow-inject-session-start",
                obj(&[("cwd", Value::Str(source_dir))]),
                "SessionStart",
                "prior recorded coding sessions",
            ),
            (
                "staxtrace-inject-user-prompt",
                "stackunderflow-inject-user-prompt",
                obj(&[
                    (
                        "prompt",
                        Value::Str("what about canonicalDispatchToken?".into()),
                    ),
                    ("cwd", Value::Str(project_path)),
                ]),
                "UserPromptSubmit",
                "canonicalDispatchToken",
            ),
            (
                "staxtrace-inject-pre-tool-use",
                "stackunderflow-inject-pre-tool-use",
                obj(&[("tool_input", obj(&[("file_path", Value::Str(file_path))]))]),
                "PreToolUse",
                "has gone wrong before",
            ),
        ];

        for (canonical_id, legacy_id, payload, event, evidence) in cases {
            if event == "SessionStart" {
                let direct = session_start_context(&payload, &env, 400)
                    .expect("seeded SessionStart query must succeed");
                assert!(
                    !direct.is_empty(),
                    "the seeded SessionStart store must warrant output"
                );
            }
            let canonical = crate::handlers::run(canonical_id, &payload, false, &env).stdout;
            let legacy = crate::handlers::run(legacy_id, &payload, false, &env).stdout;
            assert!(!canonical.is_empty(), "{canonical_id} must inject evidence");
            assert_eq!(legacy, canonical, "legacy input must normalize once");
            assert!(
                canonical.contains(&format!(r#""hookEventName": "{event}""#)),
                "{canonical_id} selected the wrong event envelope: {canonical}"
            );
            assert!(
                canonical.contains(evidence),
                "{canonical_id} omitted its seeded evidence: {canonical}"
            );
        }
    }

    #[test]
    fn only_the_two_canonical_mid_session_hooks_consume_inbox_messages() {
        let (_scratch, env) = seeded_env("inbox");
        stax_core::agent_inbox::deliver_local(
            "fresh coordination note",
            "peer",
            "message-1",
            "2026-08-27T12:00:00+00:00",
            Some(&env.app_dir),
        )
        .expect("seed inbox message");
        let session_start = crate::handlers::run(
            "staxtrace-inject-session-start",
            &obj(&[(
                "cwd",
                Value::Str(env.cwd.join("src").to_string_lossy().into_owned()),
            )]),
            false,
            &env,
        )
        .stdout;
        assert!(!session_start.contains("[staxtrace inbox]"));
        assert_eq!(
            stax_core::agent_inbox::list_messages(false, Some(&env.app_dir)).len(),
            1,
            "SessionStart must leave the inbox message unseen"
        );

        let prompt_payload = obj(&[
            (
                "prompt",
                Value::Str("what about canonicalDispatchToken?".into()),
            ),
            ("cwd", Value::Str(env.cwd.to_string_lossy().into_owned())),
        ]);
        let prompt_output =
            crate::handlers::run("staxtrace-inject-user-prompt", &prompt_payload, false, &env)
                .stdout;
        let inbox = prompt_output
            .find("[staxtrace inbox]")
            .expect("prompt inbox block");
        let memory = prompt_output
            .find("[staxtrace memory]")
            .expect("prompt memory block");
        assert!(inbox < memory, "the inbox must remain ahead of memory");
        assert!(prompt_output.contains("fresh coordination note"));
        assert!(stax_core::agent_inbox::list_messages(false, Some(&env.app_dir)).is_empty());

        stax_core::agent_inbox::deliver_local(
            "second coordination note",
            "peer",
            "message-2",
            "2026-08-27T12:01:00+00:00",
            Some(&env.app_dir),
        )
        .expect("seed second inbox message");
        let pretool_output = crate::handlers::run(
            "staxtrace-inject-pre-tool-use",
            &obj(&[(
                "tool_input",
                obj(&[(
                    "file_path",
                    Value::Str(env.cwd.join("src/lib.rs").to_string_lossy().into_owned()),
                )]),
            )]),
            false,
            &env,
        )
        .stdout;
        assert!(pretool_output.contains("second coordination note"));
        let inbox = pretool_output
            .find("[staxtrace inbox]")
            .expect("pre-tool inbox block");
        let memory = pretool_output
            .find("[staxtrace memory]")
            .expect("pre-tool memory block");
        assert!(inbox < memory, "the inbox must remain ahead of memory");
        assert!(stax_core::agent_inbox::list_messages(false, Some(&env.app_dir)).is_empty());
    }

    #[test]
    fn canonical_registry_drives_the_exact_injection_budgets() {
        assert_eq!(token_budget("staxtrace-inject-session-start"), 400);
        assert_eq!(token_budget("staxtrace-inject-user-prompt"), 200);
        assert_eq!(token_budget("staxtrace-inject-pre-tool-use"), 200);
        assert_eq!(token_budget("staxtrace-stop"), 200);
        assert_eq!(token_budget("unknown"), 200);
    }

    #[test]
    fn the_query_picker_favours_identifier_shapes() {
        // `services/discovery.py` carries a `/` and a `.` → +20, and is long.
        assert_eq!(
            prompt_to_query("please update services/discovery.py now").as_deref(),
            Some("services/discovery.py")
        );
        // All-stopwords / too-short → nothing to say.
        assert_eq!(prompt_to_query("please update the tests"), None);
        assert_eq!(prompt_to_query("   "), None);
        assert_eq!(prompt_to_query(""), None);
        // The camelCase hump (+6) beats the longer plain word.
        assert_eq!(
            prompt_to_query("rename theThing to otherwise").as_deref(),
            Some("theThing")
        );
    }

    #[test]
    fn the_token_scanner_matches_the_character_class() {
        assert_eq!(tokens("a-b_c.d/e:f g!h"), vec!["a-b_c.d/e:f", "g", "h"]);
        assert_eq!(tokens(""), Vec::<&str>::new());
        assert_eq!(tokens("!!!"), Vec::<&str>::new());
    }

    #[test]
    fn strip_of_the_token_edges_matches_python() {
        // `raw.strip("./:-")` — both ends, any of the four characters.
        assert_eq!(
            prompt_to_query("...configuration...").as_deref(),
            Some("configuration")
        );
    }

    #[test]
    fn the_clip_is_the_real_bound() {
        let long = "x".repeat(2_000);
        let clipped = clip(&long, "staxtrace-inject-session-start");
        assert_eq!(pystr::len_chars(&clipped), 1_600);
        assert!(clipped.ends_with('…'));
        // The per-prompt budget is a quarter of that.
        let clipped = clip(&long, "staxtrace-inject-user-prompt");
        assert_eq!(pystr::len_chars(&clipped), 800);
        let clipped = clip(&long, "staxtrace-inject-pre-tool-use");
        assert_eq!(pystr::len_chars(&clipped), 800);
        // An unknown id takes `.get(hook_id, 200)`.
        assert_eq!(pystr::len_chars(&clip(&long, "who?")), 800);
    }

    #[test]
    fn the_envelope_escapes_non_ascii_like_cpython() {
        // The ellipsis the truncator itself inserts is the common case.
        let rendered = pyjson::dumps_default(&Value::Object(vec![(
            "hookSpecificOutput".into(),
            Value::Object(vec![
                ("hookEventName".into(), Value::Str("SessionStart".into())),
                ("additionalContext".into(), Value::Str("a…b".into())),
            ]),
        )]));
        assert_eq!(
            rendered,
            r#"{"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "a\u2026b"}}"#
        );
    }

    #[test]
    fn the_slug_encoding_matches_the_adapters() {
        assert_eq!(slugify("/Users/a/b"), "-Users-a-b");
        assert_eq!(slugify("/Users/a/my_proj/"), "-Users-a-my-proj");
        let payload = Value::Object(vec![("cwd".into(), Value::Str("src".into()))]);
        assert_eq!(
            slug_from_cwd(payload.get("cwd"), &env()).as_deref(),
            Some("-home-u-proj-src")
        );
        assert_eq!(slug_from_cwd(None, &env()), None);
    }

    #[test]
    fn the_edited_path_probe_order_is_the_references() {
        let payload = Value::Object(vec![(
            "tool_input".into(),
            Value::Object(vec![
                ("path".into(), Value::Str("/second".into())),
                ("file_path".into(), Value::Str("  /first  ".into())),
            ]),
        )]);
        assert_eq!(edited_file_path(&payload).as_deref(), Some("/first"));
        // Whitespace-only is not a path.
        let payload = Value::Object(vec![(
            "tool_input".into(),
            Value::Object(vec![("file_path".into(), Value::Str("   ".into()))]),
        )]);
        assert_eq!(edited_file_path(&payload), None);
        // A non-dict tool_input is not probed at all.
        let payload = Value::Object(vec![("tool_input".into(), Value::Str("x".into()))]);
        assert_eq!(edited_file_path(&payload), None);
    }

    #[test]
    fn undated_rows_say_so() {
        assert_eq!(date_or_undated(""), "(undated)");
        assert_eq!(date_or_undated("2026-07-31T12:00:00"), "2026-07-31");
    }
}
