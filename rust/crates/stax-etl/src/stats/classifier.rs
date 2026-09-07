//! Port of `python-legacy: stats/classifier.py` — the mart-path subset.
//!
//! `project_mart`'s second pass runs the real classifier, so this is a
//! transcription rather than a re-derivation. Ported: [`RawEntry`],
//! [`TaggedEntry`], [`tag`], [`determine_kind`], `_detect_error`,
//! `_categorise` (the two-tier taxonomy), `_surface_text`, and the two
//! interruption markers. Not ported (nothing in the mart path reads them):
//! the `_Tier` enum, which is dead weight in Python too — it labels the
//! taxonomy rows and is never consulted.
//!
//! # DIV-002 lives here, and it is load-bearing
//!
//! [`determine_kind`] falls through to `"assistant"` for any entry it cannot
//! place (`classifier.py:174`). On the maintainer's store that misfiles **5,656
//! legacy-history user turns as assistant messages** in `project_mart`, and 57
//! of 243 events-backed rows carry `total_commands = 0` from the same path
//! (`docs/specs/rust-port.md` §6b divergence 2). Cent-exact mart parity means
//! *reproducing* those numbers, so the fall-through is ported verbatim and
//! pinned by a test named for it. Fixing it is a maintainer decision recorded
//! against DIV-002, not something a port does quietly.
//!
//! # Regexes without a regex engine
//!
//! `_TAXONOMY` is 23 rows of `(label, tier, keyword, re.Pattern)` and every
//! pattern is ASCII with `re.I`. Rather than add a regex crate to a shared
//! workspace lock for 23 fixed patterns, each confirming pattern is a named
//! matcher below with the Python source quoted above it. The two-tier structure
//! is preserved exactly — cheap keyword screen on the lowercased text, then the
//! confirming match on the original — because the screen is observable: a
//! pattern whose keyword misses is never confirmed, even when it would match.

use serde_json::Value;

use super::pytext::{contains_ci, find_ci, is_py_space, py_str, starts_with_ci};

/// `_CANCEL_MARKER` / `INTERRUPT_PREFIX` — exported by Python for the aggregator.
pub const INTERRUPT_PREFIX: &str = "[Request interrupted by user for tool use]";

/// `_ABORT_SIGNAL` / `INTERRUPT_API`.
pub const INTERRUPT_API: &str = "API Error: Request was aborted.";

/// One line from a JSONL file, lightly annotated (`classifier.RawEntry`).
///
/// `origin` is carried by the Python `NamedTuple` and copied through to
/// `TaggedEntry`; nothing in the mart path reads it, so it is not stored.
#[derive(Debug, Clone)]
pub struct RawEntry {
    /// The decoded log line.
    pub payload: Value,
    /// The session this entry belongs to.
    pub session_id: String,
    /// The provider that produced it (`"anthropic"` by default in Python).
    pub provider: String,
}

/// A raw entry annotated with classification metadata (`classifier.TaggedEntry`).
#[derive(Debug, Clone)]
pub struct TaggedEntry {
    /// The decoded log line, unchanged.
    pub payload: Value,
    /// The session this entry belongs to.
    pub session_id: String,
    /// `"user"` | `"assistant"` | `"summary"` | `"compact_summary"` | `"task"`.
    pub kind: String,
    /// Whether a `tool_result` block reported an error (or the turn is an abort).
    pub is_error: bool,
    /// The taxonomy label when `is_error`, else `None`.
    pub error_category: Option<String>,
    /// Whether the surface text opens with an interruption marker.
    pub is_interruption: bool,
    /// The provider, carried through.
    pub provider: String,
}

/// `classifier.tag` — classify a batch of raw entries.
#[must_use]
pub fn tag(entries: Vec<RawEntry>) -> Vec<TaggedEntry> {
    entries.into_iter().map(classify).collect()
}

/// One entry of [`tag`], for callers that stream rows rather than batch them
/// — the mart path, which must not hold a project's payloads all at once.
#[must_use]
pub fn tag_one(entry: RawEntry) -> TaggedEntry {
    classify(entry)
}

fn classify(entry: RawEntry) -> TaggedEntry {
    let kind = determine_kind(&entry.payload);
    let text = surface_text(&entry.payload);

    let is_interruption = text.starts_with(INTERRUPT_PREFIX) || text.starts_with(INTERRUPT_API);
    let (is_error, error_category) = detect_error(&entry.payload, &kind, &text);

    TaggedEntry {
        payload: entry.payload,
        session_id: entry.session_id,
        kind,
        is_error,
        error_category,
        is_interruption,
        provider: entry.provider,
    }
}

/// `classifier._determine_kind` — **DIV-002**: unmatched entries fall through
/// to `"assistant"`.
///
/// Ported line for line, including the final `return "assistant"` that makes a
/// legacy-history user turn (no `type`, no `message.role`) count as an
/// assistant message in `project_mart`.
#[must_use]
pub fn determine_kind(data: &Value) -> String {
    let raw = data.get("type").and_then(Value::as_str).unwrap_or("");
    match raw {
        "human" => return "user".to_string(),
        "assistant" => return "assistant".to_string(),
        "summary" | "compact_summary" => return raw.to_string(),
        "task_start" | "task" => return "task".to_string(),
        _ => {}
    }
    if let Some(msg) = data.get("message").and_then(Value::as_object) {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "user" || role == "assistant" {
            return role.to_string();
        }
    }
    // DIV-002. Do not "fix" this without moving the ledger row first.
    "assistant".to_string()
}

/// `classifier._surface_text` — a quick text representation for the
/// interruption / error checks.
///
/// Deliberately *not* [`super::enricher::text_from`]: this one ignores
/// `tool_use` and `tool_result` blocks and joins with `\n`, where the
/// enricher's flattens them. Both exist in Python and they disagree; the
/// classifier's is what decides `is_interruption`.
#[must_use]
pub fn surface_text(data: &Value) -> String {
    if let Some(s) = data.get("summary").and_then(Value::as_str) {
        return s.to_string();
    }
    let Some(msg) = data.get("message").and_then(Value::as_object) else {
        return String::new();
    };
    let body = msg.get("content");
    match body {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut pieces: Vec<&str> = Vec::new();
            for blk in items {
                match blk {
                    Value::String(s) => pieces.push(s),
                    Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("text") => {
                        pieces.push(o.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                    _ => {}
                }
            }
            pieces.join("\n")
        }
        // `msg.get("content", "")` defaults to `""`, and a non-str/non-list
        // value falls through both branches to the trailing `return ""`.
        _ => String::new(),
    }
}

/// `classifier._detect_error` — `(is_error, category)` from `tool_result`
/// blocks and the abort signal.
///
/// One recorded deviation: Python builds the list-shaped error body with
/// `" ".join(b.get("text", "") for b in err_body if isinstance(b, dict))`,
/// which raises `TypeError` if a block's `text` is a non-string. This port
/// coerces a non-string `text` to `""` instead of raising. Python's behavior
/// there is to abort the entire mart refresh, so the divergence is only
/// reachable on a store where the Python rebuild cannot complete at all.
#[must_use]
pub fn detect_error(data: &Value, kind: &str, text: &str) -> (bool, Option<String>) {
    if let Some(msg) = data.get("message").and_then(Value::as_object)
        && let Some(content) = msg.get("content").and_then(Value::as_array)
    {
        for block in content {
            let Some(b) = block.as_object() else { continue };
            if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            if !b.get("is_error").is_some_and(super::pytext::py_truthy) {
                continue;
            }
            // `block.get("content", "")`
            let err_body = b
                .get("content")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let rendered = match &err_body {
                Value::Array(items) => items
                    .iter()
                    .filter_map(|x| x.as_object())
                    .map(|o| o.get("text").and_then(Value::as_str).unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(" "),
                other => py_str(other),
            };
            return (true, Some(categorise(&rendered)));
        }
    }

    if kind == "assistant" && super::pytext::py_strip(text) == INTERRUPT_API {
        return (true, Some("User Interruption".to_string()));
    }

    (false, None)
}

/// One `_TAXONOMY` row: the label, the fast keyword screen, and the confirming
/// matcher that stands in for the compiled regex.
struct Rule {
    label: &'static str,
    keyword: &'static str,
    confirm: fn(&str) -> bool,
}

/// `classifier._TAXONOMY`, in order. Order is observable: `_categorise` returns
/// the first row whose keyword screens *and* whose pattern confirms.
const TAXONOMY: &[Rule] = &[
    // ── user-initiated halts ─────────────────────────────────────────
    Rule {
        label: "User Interruption",
        keyword: "want",
        confirm: m_user_doesnt_want,
    },
    Rule {
        label: "User Interruption",
        keyword: "interrupted",
        confirm: m_request_interrupted,
    },
    Rule {
        label: "Command Timeout",
        keyword: "timed out",
        confirm: m_command_timed_out,
    },
    // ── filesystem state ─────────────────────────────────────────────
    Rule {
        label: "File Not Read",
        keyword: "not been read",
        confirm: m_not_read_yet,
    },
    Rule {
        label: "File Modified",
        keyword: "modified since",
        confirm: m_modified_since,
    },
    Rule {
        label: "File Too Large",
        keyword: "maximum allowed",
        confirm: m_exceeds_max,
    },
    // ── resource lookup failures ─────────────────────────────────────
    Rule {
        label: "Content Not Found",
        keyword: "not found",
        confirm: m_string_not_found,
    },
    Rule {
        label: "Content Not Found",
        keyword: "does not exist",
        confirm: m_does_not_exist,
    },
    Rule {
        label: "Content Not Found",
        keyword: "enoent",
        confirm: m_npm_enoent,
    },
    Rule {
        label: "No Changes",
        keyword: "no changes",
        confirm: m_no_changes,
    },
    // ── permissions & access ─────────────────────────────────────────
    Rule {
        label: "Permission Error",
        keyword: "permission denied",
        confirm: m_permission_denied,
    },
    Rule {
        label: "Permission Error",
        keyword: "was blocked",
        confirm: m_cd_blocked,
    },
    // ── tooling problems ─────────────────────────────────────────────
    Rule {
        label: "Tool Not Found",
        keyword: "command not found",
        confirm: m_command_not_found,
    },
    Rule {
        label: "Wrong Tool",
        keyword: "notebookread",
        confirm: m_jupyter_notebookread,
    },
    // ── code execution ───────────────────────────────────────────────
    Rule {
        label: "Code Runtime Error",
        keyword: "cannot find module",
        confirm: m_cannot_find_module,
    },
    Rule {
        label: "Code Runtime Error",
        keyword: "traceback",
        confirm: m_traceback,
    },
    Rule {
        label: "Port Binding Error",
        keyword: "bind on address",
        confirm: m_bind_on_address,
    },
    // ── syntax / validation ──────────────────────────────────────────
    Rule {
        label: "Syntax Error",
        keyword: "syntaxerror",
        confirm: m_syntax_error,
    },
    Rule {
        label: "Syntax Error",
        keyword: "replace_all is false",
        confirm: m_replace_all_false,
    },
    Rule {
        label: "Syntax Error",
        keyword: "null (null)",
        confirm: m_null_no_keys,
    },
    Rule {
        label: "Syntax Error",
        keyword: "jq: error",
        confirm: m_jq_or_ive,
    },
    // ── notebook ─────────────────────────────────────────────────────
    Rule {
        label: "Notebook Cell Not Found",
        keyword: "not found in notebook",
        confirm: m_cell_not_found,
    },
    // ── catch-all ────────────────────────────────────────────────────
    Rule {
        label: "Other Tool Errors",
        keyword: "[details] error",
        confirm: m_details_error,
    },
];

/// `classifier._categorise` — two-tier match: keyword screen, then confirm.
///
/// The screen runs against `text.lower()` in Python. Every keyword is ASCII, so
/// the ASCII-folded `contains_ci` is equivalent except where a non-ASCII code
/// point *lowercases onto* ASCII (`U+212A KELVIN SIGN` → `k`); Python's `re.I`
/// folds the same way on the confirming pattern, so both tiers move together
/// and the net divergence needs such a character inside a tool-error body.
#[must_use]
pub fn categorise(text: &str) -> String {
    for rule in TAXONOMY {
        if contains_ci(text, rule.keyword) && (rule.confirm)(text) {
            return rule.label.to_string();
        }
    }
    "Other".to_string()
}

// ── the confirming matchers ─────────────────────────────────────────────────
//
// Each stands in for one compiled `re.Pattern` with `re.I`. `.` is
// "any character except newline" (no `re.S` anywhere in the taxonomy).

/// `r"user doesn.t want to (?:proceed|take this action)"`
fn m_user_doesnt_want(text: &str) -> bool {
    let mut from = 0;
    while let Some(i) = find_ci(text, "user doesn", from) {
        from = i + 1;
        let rest = &text[i + "user doesn".len()..];
        // `.` — exactly one character, and not a newline.
        let Some(c) = rest.chars().next() else {
            continue;
        };
        if c == '\n' {
            continue;
        }
        let after = &rest[c.len_utf8()..];
        if !starts_with_ci(after, "t want to ") {
            continue;
        }
        let tail = &after["t want to ".len()..];
        if starts_with_ci(tail, "proceed") || starts_with_ci(tail, "take this action") {
            return true;
        }
    }
    false
}

/// `r"\[Request interrupted"`
fn m_request_interrupted(text: &str) -> bool {
    contains_ci(text, "[Request interrupted")
}

/// `r"command timed out"`
fn m_command_timed_out(text: &str) -> bool {
    contains_ci(text, "command timed out")
}

/// `r"file has not been read yet"`
fn m_not_read_yet(text: &str) -> bool {
    contains_ci(text, "file has not been read yet")
}

/// `r"file has been modified since read"`
fn m_modified_since(text: &str) -> bool {
    contains_ci(text, "file has been modified since read")
}

/// `r"exceeds maximum allowed"`
fn m_exceeds_max(text: &str) -> bool {
    contains_ci(text, "exceeds maximum allowed")
}

/// `r"string (?:to replace )?not found|no module named|no such file"`
fn m_string_not_found(text: &str) -> bool {
    if contains_ci(text, "no module named") || contains_ci(text, "no such file") {
        return true;
    }
    let mut from = 0;
    while let Some(i) = find_ci(text, "string ", from) {
        from = i + 1;
        let rest = &text[i + "string ".len()..];
        if starts_with_ci(rest, "not found") {
            return true;
        }
        if starts_with_ci(rest, "to replace ")
            && starts_with_ci(&rest["to replace ".len()..], "not found")
        {
            return true;
        }
    }
    false
}

/// `r"file does not exist"`
fn m_does_not_exist(text: &str) -> bool {
    contains_ci(text, "file does not exist")
}

/// `r"npm error enoent"`
fn m_npm_enoent(text: &str) -> bool {
    contains_ci(text, "npm error enoent")
}

/// `r"no changes to make"`
fn m_no_changes(text: &str) -> bool {
    contains_ci(text, "no changes to make")
}

/// `r"permission denied"`
fn m_permission_denied(text: &str) -> bool {
    contains_ci(text, "permission denied")
}

/// `r"cd to.*was blocked|was blocked.*cd to"`
fn m_cd_blocked(text: &str) -> bool {
    dot_star_between(text, "cd to", "was blocked") || dot_star_between(text, "was blocked", "cd to")
}

/// `r"command not found"`
fn m_command_not_found(text: &str) -> bool {
    contains_ci(text, "command not found")
}

/// `r"jupyter notebook.*notebookread"`
fn m_jupyter_notebookread(text: &str) -> bool {
    dot_star_between(text, "jupyter notebook", "notebookread")
}

/// `r"cannot find module"`
fn m_cannot_find_module(text: &str) -> bool {
    contains_ci(text, "cannot find module")
}

/// `r"traceback"`
fn m_traceback(text: &str) -> bool {
    contains_ci(text, "traceback")
}

/// `r"attempting to bind on address"`
fn m_bind_on_address(text: &str) -> bool {
    contains_ci(text, "attempting to bind on address")
}

/// `r"syntax\s*error"` — `\s` is CPython's whitespace set, not Unicode's.
///
/// `\s*` is greedy with backtracking, but `error` cannot begin with a
/// whitespace character, so "consume the whole run, then match" is equivalent.
fn m_syntax_error(text: &str) -> bool {
    let mut from = 0;
    while let Some(i) = find_ci(text, "syntax", from) {
        from = i + 1;
        let mut rest = &text[i + "syntax".len()..];
        rest = rest.trim_start_matches(is_py_space);
        if starts_with_ci(rest, "error") {
            return true;
        }
    }
    false
}

/// `r"replace_all is false"`
fn m_replace_all_false(text: &str) -> bool {
    contains_ci(text, "replace_all is false")
}

/// `r"null \(null\) has no keys"`
fn m_null_no_keys(text: &str) -> bool {
    contains_ci(text, "null (null) has no keys")
}

/// `r"jq: error|inputvalidationerror"`
fn m_jq_or_ive(text: &str) -> bool {
    contains_ci(text, "jq: error") || contains_ci(text, "inputvalidationerror")
}

/// `r'cell with id "[0-9a-f]+" not found in notebook'`
///
/// `re.I` makes the class match `A-F` too. `+` is greedy and no backtrack can
/// help: every character it would give back is a hex digit, never the `"` the
/// pattern needs next.
fn m_cell_not_found(text: &str) -> bool {
    const HEAD: &str = "cell with id \"";
    const TAIL: &str = "\" not found in notebook";
    let mut from = 0;
    while let Some(i) = find_ci(text, HEAD, from) {
        from = i + 1;
        let rest = &text[i + HEAD.len()..];
        let hex_len = rest.bytes().take_while(u8::is_ascii_hexdigit).count();
        if hex_len == 0 {
            continue;
        }
        if starts_with_ci(&rest[hex_len..], TAIL) {
            return true;
        }
    }
    false
}

/// `r"\[details\] error: error"`
fn m_details_error(text: &str) -> bool {
    contains_ci(text, "[details] error: error")
}

/// `A.*B` with `re.I` and no `re.S`: `B` must start after `A` ends with no
/// newline in between.
fn dot_star_between(text: &str, a: &str, b: &str) -> bool {
    let mut from = 0;
    while let Some(i) = find_ci(text, a, from) {
        from = i + 1;
        let start = i + a.len();
        // `.*` cannot cross a newline, so `B` must begin before the next one.
        let limit = text[start..]
            .find('\n')
            .map_or(text.len(), |off| start + off);
        if let Some(j) = find_ci(text, b, start)
            && j <= limit
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn div_002_unmatched_entries_fall_through_to_assistant() {
        // The bug the wave-3 gate must reproduce: a legacy-history user turn
        // with no `type` and no `message.role` is counted as an assistant
        // message. 5,656 of these on the maintainer's store (§6b divergence 2).
        assert_eq!(
            determine_kind(&json!({"message": {"content": "hi"}})),
            "assistant"
        );
        assert_eq!(determine_kind(&json!({})), "assistant");
        assert_eq!(determine_kind(&json!({"type": "nonsense"})), "assistant");
        // …while the paths that DO match are unaffected.
        assert_eq!(determine_kind(&json!({"type": "human"})), "user");
        assert_eq!(determine_kind(&json!({"type": "summary"})), "summary");
        assert_eq!(
            determine_kind(&json!({"type": "compact_summary"})),
            "compact_summary"
        );
        assert_eq!(determine_kind(&json!({"type": "task_start"})), "task");
        assert_eq!(determine_kind(&json!({"type": "task"})), "task");
        assert_eq!(
            determine_kind(&json!({"message": {"role": "user"}})),
            "user"
        );
        assert_eq!(
            determine_kind(&json!({"message": {"role": "system"}})),
            "assistant"
        );
    }

    #[test]
    fn surface_text_ignores_tool_blocks_where_the_enricher_flattens_them() {
        let payload = json!({"message": {"content": [
            {"type": "text", "text": "a"},
            {"type": "tool_use", "name": "Read"},
            {"type": "text", "text": "b"},
        ]}});
        assert_eq!(surface_text(&payload), "a\nb");
        assert_eq!(
            surface_text(&json!({"summary": "s", "message": {"content": "x"}})),
            "s"
        );
        assert_eq!(surface_text(&json!({"message": {"content": {"k": 1}}})), "");
    }

    /// Every case here was run through CPython's `classifier._categorise` and
    /// the right-hand side is *its* answer, not a guess about what the pattern
    /// ought to do. Four of them are cases where the port's first draft was
    /// wrong in the direction of being too generous — see the two tests below
    /// that call the surprises out by name.
    const PYTHON_ORACLE: &[(&str, &str)] = &[
        ("the user doesn't want to proceed", "User Interruption"),
        (
            "the user doesn't want to take this action",
            "User Interruption",
        ),
        ("user doesnXt want to proceed", "User Interruption"),
        ("user doesn\nt want to proceed", "Other"),
        ("[Request interrupted by user]", "User Interruption"),
        ("Command timed out after 2m", "Command Timeout"),
        ("File has not been read yet", "File Not Read"),
        ("File has been modified since read", "File Modified"),
        ("exceeds maximum allowed size", "File Too Large"),
        ("String to replace not found in file", "Content Not Found"),
        ("String not found", "Content Not Found"),
        ("ModuleNotFoundError: No module named 'x'", "Other"),
        ("no module named foo, not found", "Content Not Found"),
        ("no such file or directory, not found", "Content Not Found"),
        ("File does not exist.", "Content Not Found"),
        ("npm error enoent", "Content Not Found"),
        ("No changes to make", "No Changes"),
        ("bash: permission denied", "Permission Error"),
        ("cd to /tmp was blocked", "Permission Error"),
        ("was blocked when trying to cd to /tmp", "Permission Error"),
        ("cd to /tmp\nwas blocked", "Other"),
        ("foo: command not found", "Tool Not Found"),
        ("this is a jupyter notebook, use NotebookRead", "Wrong Tool"),
        ("Error: Cannot find module 'x'", "Code Runtime Error"),
        ("Traceback (most recent call last)", "Code Runtime Error"),
        ("attempting to bind on address", "Port Binding Error"),
        ("SyntaxError: invalid syntax error", "Syntax Error"),
        ("a syntax  error here", "Other"),
        ("SyntaxError: a syntax  error here", "Syntax Error"),
        ("syntax\u{1c}error and syntaxerror", "Syntax Error"),
        ("replace_all is false", "Syntax Error"),
        ("null (null) has no keys", "Syntax Error"),
        ("jq: error at line 1", "Syntax Error"),
        ("InputValidationError happened, jq: error", "Syntax Error"),
        (
            "cell with id \"a1b2c3\" not found in notebook",
            "Notebook Cell Not Found",
        ),
        (
            "cell with id \"A1B2C3\" not found in notebook",
            "Notebook Cell Not Found",
        ),
        ("[details] error: error occurred", "Other Tool Errors"),
        ("something else entirely", "Other"),
    ];

    #[test]
    fn taxonomy_rows_agree_with_cpython_case_for_case() {
        for (text, expected) in PYTHON_ORACLE {
            assert_eq!(&categorise(text), expected, "input {text:?}");
        }
    }

    #[test]
    fn the_keyword_screen_is_observable_not_decoration() {
        // The screen is not a fast path with the same answer — it *changes* the
        // answer, twice in the oracle above:
        //
        // * `ModuleNotFoundError: No module named 'x'` matches the pattern's
        //   `no module named` branch, but the row's keyword is the two-word
        //   `not found` and this text only has the run-together `notfound`. The
        //   row never gets to confirm, and the text falls through to "Other".
        //   Add a literal "not found" and the same row fires.
        assert!(!contains_ci(
            "ModuleNotFoundError: No module named 'x'",
            "not found"
        ));
        assert_eq!(
            categorise("ModuleNotFoundError: No module named 'x'"),
            "Other"
        );
        assert_eq!(
            categorise("no module named foo, not found"),
            "Content Not Found"
        );
        //
        // * `syntax\s*error` confirms across a gap, but the row's keyword is the
        //   run-together `syntaxerror`.
        assert!(!contains_ci("a syntax  error here", "syntaxerror"));
        assert_eq!(categorise("a syntax  error here"), "Other");
        assert_eq!(
            categorise("SyntaxError: a syntax  error here"),
            "Syntax Error"
        );
    }

    #[test]
    fn the_dot_in_user_doesn_t_is_any_character_except_newline() {
        // `user doesn.t want to proceed` — the apostrophe is written as `.`,
        // which happily matches an `X`, and stops at a newline.
        assert_eq!(
            categorise("user doesnXt want to proceed"),
            "User Interruption"
        );
        assert_eq!(categorise("user doesn\nt want to proceed"), "Other");
    }

    #[test]
    fn dot_star_does_not_cross_a_newline() {
        assert!(dot_star_between(
            "cd to /tmp was blocked",
            "cd to",
            "was blocked"
        ));
        assert!(!dot_star_between(
            "cd to /tmp\nwas blocked",
            "cd to",
            "was blocked"
        ));
    }

    #[test]
    fn syntax_error_uses_cpython_whitespace() {
        // U+001C is `\s` to CPython and not whitespace to Rust.
        assert!(m_syntax_error("syntax\u{1c}error"));
        assert!(m_syntax_error("syntaxerror"));
        assert!(!m_syntax_error("syntax_error"));
    }

    #[test]
    fn detect_error_reads_tool_result_blocks_then_the_abort_signal() {
        let payload = json!({"message": {"content": [
            {"type": "tool_result", "is_error": true, "content": "Traceback (most recent)"}
        ]}});
        assert_eq!(
            detect_error(&payload, "user", ""),
            (true, Some("Code Runtime Error".to_string()))
        );
        // list-shaped body joins with a single space
        let payload = json!({"message": {"content": [
            {"type": "tool_result", "is_error": true,
             "content": [{"text": "permission"}, {"text": "denied"}]}
        ]}});
        assert_eq!(
            detect_error(&payload, "user", ""),
            (true, Some("Permission Error".to_string()))
        );
        // is_error falsy → not an error
        let payload = json!({"message": {"content": [
            {"type": "tool_result", "is_error": false, "content": "Traceback"}
        ]}});
        assert_eq!(detect_error(&payload, "user", ""), (false, None));
        // abort signal, assistant only, after .strip()
        assert_eq!(
            detect_error(
                &json!({}),
                "assistant",
                "  API Error: Request was aborted.  "
            ),
            (true, Some("User Interruption".to_string()))
        );
        assert_eq!(
            detect_error(&json!({}), "user", INTERRUPT_API),
            (false, None)
        );
    }

    #[test]
    fn tagging_sets_the_interruption_flag_from_surface_text() {
        let tagged = tag(vec![RawEntry {
            payload: json!({"type": "human", "message": {"content": INTERRUPT_PREFIX}}),
            session_id: "s".into(),
            provider: "anthropic".into(),
        }]);
        assert_eq!(tagged[0].kind, "user");
        assert!(tagged[0].is_interruption);
    }
}
