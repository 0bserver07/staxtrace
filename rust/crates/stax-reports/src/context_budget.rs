//! `services/context_budget.py` — the per-session "context tax" estimator.
//!
//! | Item | Python | Notes |
//! |---|---|---|
//! | [`DEFAULT_SYSTEM_PROMPT_TOKENS`] | `DEFAULT_SYSTEM_PROMPT_TOKENS` | fixed 3000 |
//! | [`MCP_BASE_TOKENS`] / [`MCP_PER_TOOL_TOKENS`] / [`MCP_UNKNOWN_TOOLS_FALLBACK`] | same | 200 / 50 / 200 |
//! | [`CHARS_PER_TOKEN`] | `CHARS_PER_TOKEN` | the `// 4` divisor |
//! | [`DEFAULT_INPUT_USD_PER_MILLION`] | same | `$3.00` |
//! | [`DEFAULT_SESSIONS_PER_MONTH`] | same | also imported by `routes/optimize.py` |
//! | [`ContextSlice`] | `@dataclass ContextSlice` | `name` / `tokens` / `source_path` |
//! | [`ContextBudget`] + [`ContextBudget::to_dict`] | `@dataclass ContextBudget` | the payload contract |
//! | [`estimate_tokens`] | `estimate_tokens` | `len(text) // 4` |
//! | [`estimate_context_budget`] | `estimate_context_budget` | project + global sources |
//! | [`estimate_global_budget`] | `estimate_global_budget` | `~/.claude` only |
//!
//! # What this actually does
//!
//! Every coding session pays a fixed "tax" before the user types: the system
//! prompt, the memory files, one block per registered MCP server, one per
//! installed skill, one per subagent definition. This walks the *files on disk*
//! that hold those and turns each into a token estimate. It is not a tokenizer
//! — it is `len(text) // 4`, and the number is for spotting bloat, not billing.
//!
//! So, unusually for this crate, the input is the **filesystem**, not the
//! store. The paths read are, in order:
//!
//! * `<project>/CLAUDE.md` and `<home>/.claude/CLAUDE.md`
//! * `<home>/.claude.json` and `<project>/.claude/settings.json` (the
//!   `mcpServers` map out of each)
//! * `<home>/.claude/skills/*/SKILL.md`
//! * `<project>/.claude/agents/*.md` and `<home>/.claude/agents/*.md`
//!
//! # The injected home, and why it is `$HOME` and not `$STACKUNDERFLOW_HOME`
//!
//! Python's signature is `estimate_context_budget(project_dir, *, home_dir=None)`
//! with `home = home_dir or Path.home()`, and the *production* caller
//! (`routes/context_budget.py`) always passes `None`. `Path.home()` is
//! `os.path.expanduser("~")`, i.e. `$HOME` on POSIX — it is **not** the
//! `$STACKUNDERFLOW_HOME` the server was started with, and the difference
//! matters: the harness points `STACKUNDERFLOW_HOME` at
//! `rust/.parity-state/fresh`, which has no `.claude` in it at all, so binding
//! to it would make this endpoint answer `3000` tokens forever while the
//! reference reads the maintainer's real config.
//!
//! Per finding 5 of `rust/ARCHITECT-STATE.md` nothing here reads the
//! environment: `home` is a parameter, and `routes/context_budget.rs` resolves
//! it once from `std::env::home_dir()` — the same helper `routes/projects.rs`
//! already uses for `~/.claude/projects`. Tests inject a temp dir, which is
//! exactly what `tests/python-legacy: services/test_context_budget.py` does
//! with `home_dir=`.
//!
//! # Three things a careless port gets wrong, all in `len(text)`
//!
//! 1. **`Path.read_text()` opens in TEXT mode**, so `newline=None` universal
//!    newlines apply: `\r\n` and a bare `\r` both become a single `\n` *before*
//!    `len()` sees them. A CRLF `CLAUDE.md` is therefore cheaper than its byte
//!    count. `std::fs::read_to_string` does no such thing, so
//!    [`universal_newlines`] does it here.
//! 2. **`len()` counts code points, not bytes.** A CJK or emoji-heavy
//!    `SKILL.md` costs a quarter of what a byte count would say.
//! 3. **`errors="replace"`** turns each invalid UTF-8 *maximal subpart* into
//!    one `U+FFFD`, which then costs a character. `String::from_utf8_lossy`
//!    follows the same maximal-subpart rule, verified byte for byte against
//!    CPython on `\xff`, `\xc3`, `\xe2\x82`, `\xf0\x9f\x92`, `\x80\x80` and the
//!    surrogate form `\xed\xa0\x80` (1, 1, 1, 1, 2 and 3 replacements).
//!
//! # `source_path` is `str(Path(...))`, which normalises
//!
//! Every slice carries the path it was costed from, and Python renders it with
//! `str()` on a `PurePosixPath`. That collapses `//` runs, drops `.`
//! components and strips a trailing separator — `Path("/a//b/./c/")` prints
//! `/a/b/c`. Store rows can and do hold un-normalised paths, so
//! [`py_path_str`] reproduces `posixpath.splitroot` + the `_parse_path` filter
//! rather than concatenating strings.
//!
//! # Defensive by design, and that is load-bearing
//!
//! Every read is wrapped: a missing file, a malformed JSON config, an
//! unreadable directory all contribute *zero tokens*, never an error. The
//! endpoint must answer on a machine that has never configured MCP or skills.
//! The one hole in that shield is recorded as **DIV-100**.
//!
//! # Measured, not assumed
//!
//! Both entry points were diffed against the reference over a purpose-built
//! fixture tree — a home path containing a space, an un-normalised project path
//! (`proj//ect/.`), a CRLF + BOM + invalid-UTF-8 + emoji `CLAUDE.md`, MCP names
//! differing only in case plus one non-ASCII (`ünïcode`) plus a string stub and
//! an object-valued `tools`, a shadowed project server, skills with and without
//! a `SKILL.md`, and agent files covering `SHOUTY.MD` / `.md` / `a.b.md` /
//! `no-suffix` / a *directory* called `dir.md`. All three payloads (global,
//! project, and the real `$HOME`) came out **byte-identical** through
//! `stax_memory::pyjson::dumps_http` against
//! `json.dumps(..., ensure_ascii=False, separators=(",", ":"))` — 2 884, 3 932
//! and 332 bytes. The project payload's `estimated_monthly_cost_usd` is
//! `1.8176999999999999`, which is the digit any reordering of the two
//! multiplications loses.

use std::path::Path;

use serde_json::{Map, Value};

// ── tunables (`# ── tunables ──` in the reference, same order) ───────────────

/// `DEFAULT_SYSTEM_PROMPT_TOKENS` — the fixed estimate for Claude Code's
/// built-in system prompt.
pub const DEFAULT_SYSTEM_PROMPT_TOKENS: i64 = 3000;

/// `MCP_BASE_TOKENS` — charged once per registered server, for its description.
pub const MCP_BASE_TOKENS: i64 = 200;

/// `MCP_PER_TOOL_TOKENS` — charged per tool when the definition enumerates them.
pub const MCP_PER_TOOL_TOKENS: i64 = 50;

/// `MCP_UNKNOWN_TOOLS_FALLBACK` — the flat fee when it does not (the common
/// case: tools are discovered at runtime, so nothing is in the config file).
pub const MCP_UNKNOWN_TOOLS_FALLBACK: i64 = 200;

/// `CHARS_PER_TOKEN` — "1 token ≈ 4 characters of English prose".
pub const CHARS_PER_TOKEN: usize = 4;

/// `DEFAULT_INPUT_USD_PER_MILLION` — $3/M input tokens, pinned in the source.
pub const DEFAULT_INPUT_USD_PER_MILLION: f64 = 3.0;

/// `DEFAULT_SESSIONS_PER_MONTH` — the order-of-magnitude monthly session count.
///
/// `pub` because `routes/optimize.py` imports it too, as the default of its
/// `sessions_per_month` query parameter (`ge=1, le=100_000`).
pub const DEFAULT_SESSIONS_PER_MONTH: i64 = 100;

/// The `heuristic` string every payload carries.
///
/// Python builds it as an f-string over the four constants at class-definition
/// time; it is a fixed value in every response, so it is a constant here and
/// `the_heuristic_string_is_the_f_string_over_the_constants` pins the two
/// together.
pub const HEURISTIC: &str = "len(text) // 4; per-MCP-server 200 + 50/tool";

// ── dataclasses ──────────────────────────────────────────────────────────────

/// `@dataclass class ContextSlice` — one contributor to the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSlice {
    /// `name` — `system_prompt`, `memory:…`, `mcp:…`, `skill:…`, `agent:…`.
    pub name: String,
    /// `tokens` — always an int, so it renders without a `.0`.
    pub tokens: i64,
    /// `source_path` — `str(Path)`, or `None` for the system prompt.
    pub source_path: Option<String>,
}

impl ContextSlice {
    /// `ContextSlice(name=…, tokens=…, source_path=…)`.
    #[must_use]
    pub fn new(name: impl Into<String>, tokens: i64, source_path: Option<String>) -> Self {
        Self {
            name: name.into(),
            tokens,
            source_path,
        }
    }

    /// `asdict(slice)` — field order is the declaration order.
    #[must_use]
    fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("name".to_owned(), Value::String(self.name.clone()));
        out.insert("tokens".to_owned(), Value::from(self.tokens));
        out.insert(
            "source_path".to_owned(),
            self.source_path
                .as_ref()
                .map_or(Value::Null, |path| Value::String(path.clone())),
        );
        Value::Object(out)
    }
}

/// `@dataclass class ContextBudget` — the whole per-session budget.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextBudget {
    /// `total_tokens` — `sum(s.tokens for s in slices)`, an **int**.
    pub total_tokens: i64,
    /// `slices` — in build order, which is the response order.
    pub slices: Vec<ContextSlice>,
    /// `cost_per_session_usd` — `(total / 1e6) * 3.0`, a float.
    pub cost_per_session_usd: f64,
    /// `estimated_monthly_cost_usd` — `cost_per_session_usd * 100`.
    pub estimated_monthly_cost_usd: f64,
    /// `heuristic` — see [`HEURISTIC`].
    pub heuristic: String,
}

impl ContextBudget {
    /// `ContextBudget(total_tokens=…, slices=…, cost_per_session_usd=…,
    /// estimated_monthly_cost_usd=…)` — `heuristic` takes its field default.
    #[must_use]
    pub fn new(
        total_tokens: i64,
        slices: Vec<ContextSlice>,
        cost_per_session_usd: f64,
        estimated_monthly_cost_usd: f64,
    ) -> Self {
        Self {
            total_tokens,
            slices,
            cost_per_session_usd,
            estimated_monthly_cost_usd,
            heuristic: HEURISTIC.to_owned(),
        }
    }

    /// `ContextBudget.to_dict()`.
    ///
    /// The key ORDER is the payload contract — `preserve_order` is on for the
    /// whole workspace and `stax_memory::pyjson::dumps_http` writes the map in
    /// insertion order, so `total_tokens` first and `heuristic` last is not
    /// cosmetic.
    #[must_use]
    pub fn to_dict(&self) -> Value {
        let mut out = Map::new();
        out.insert("total_tokens".to_owned(), Value::from(self.total_tokens));
        out.insert(
            "slices".to_owned(),
            Value::Array(self.slices.iter().map(ContextSlice::to_value).collect()),
        );
        out.insert(
            "cost_per_session_usd".to_owned(),
            Value::from(self.cost_per_session_usd),
        );
        out.insert(
            "estimated_monthly_cost_usd".to_owned(),
            Value::from(self.estimated_monthly_cost_usd),
        );
        out.insert(
            "heuristic".to_owned(),
            Value::String(self.heuristic.clone()),
        );
        Value::Object(out)
    }
}

// ── token counting ───────────────────────────────────────────────────────────

/// `estimate_tokens(text)` — `len(text) // CHARS_PER_TOKEN`, floored.
///
/// `if not text: return 0` catches Python's `None` as well as `""`; `&str` has
/// no third state, and an empty string floors to 0 through the same arithmetic
/// anyway. `len()` is **code points**, hence `chars().count()` and not `len()`.
#[must_use]
pub fn estimate_tokens(text: &str) -> i64 {
    i64::try_from(text.chars().count() / CHARS_PER_TOKEN).unwrap_or(i64::MAX)
}

/// `_read_text(path)` — the contents, or `""` on any read error.
///
/// Three behaviours in one line of Python (`path.read_text(encoding="utf-8",
/// errors="replace")`), all of them observable in the token count: text mode's
/// universal newlines, `errors="replace"`, and the `except OSError` that turns
/// a missing or unreadable file into a zero-token slice.
fn read_text(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => universal_newlines(&String::from_utf8_lossy(&bytes)),
        // `except (OSError, UnicodeError)` — the debug log has no observable
        // effect, so it is not reproduced.
        Err(_) => String::new(),
    }
}

/// `open(..., newline=None)`'s translation: `\r\n` → `\n`, then bare `\r` → `\n`.
///
/// Done in one pass, which is what CPython's incremental decoder does; a
/// two-pass `replace("\r\n","\n").replace("\r","\n")` would agree here but is
/// not the same operation on a `\r\r\n` run, so the pass is written out.
fn universal_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

// ── `str(PurePosixPath(...))` ────────────────────────────────────────────────

/// `str(pathlib.PurePosixPath(raw))` — what `source_path` actually carries.
///
/// `posixpath.splitroot` then `_parse_path`'s filter: a leading `//` (exactly
/// two, never three or more) is a root of its own, empty and `.` components are
/// dropped, `..` is **kept** (pathlib does no resolution), and an empty result
/// renders as `.`.
#[must_use]
pub fn py_path_str(raw: &str) -> String {
    let bytes = raw.as_bytes();
    // `posixpath.splitroot`: '' | '/' | '//'.
    let root = if bytes.first() != Some(&b'/') {
        ""
    } else if bytes.get(1) != Some(&b'/') || bytes.get(2) == Some(&b'/') {
        "/"
    } else {
        "//"
    };
    let tail: Vec<&str> = raw[root.len()..]
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if tail.is_empty() {
        // `_format_parsed_parts(...) or '.'`.
        if root.is_empty() {
            ".".to_owned()
        } else {
            root.to_owned()
        }
    } else {
        format!("{root}{}", tail.join("/"))
    }
}

/// A path already in `str(Path(...))` form, plus the joins the estimator does.
///
/// Held as a `String` rather than a `PathBuf` because the *string* is half the
/// contract: it goes out in `source_path`, and Python uses the very same object
/// for the filesystem call and the rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PyPath {
    text: String,
}

impl PyPath {
    /// `Path(raw)`.
    fn new(raw: &str) -> Self {
        Self {
            text: py_path_str(raw),
        }
    }

    /// `self / name` — `posixpath.join`, then a re-parse. The join must not
    /// double the separator when `self` is the root: `Path("/") / ".claude"` is
    /// `/.claude`, not `//.claude`, and `//` would have been a *different root*.
    fn join(&self, name: &str) -> Self {
        let joined = if self.text.ends_with('/') {
            format!("{}{name}", self.text)
        } else {
            format!("{}/{name}", self.text)
        };
        Self::new(&joined)
    }

    fn as_path(&self) -> &Path {
        Path::new(&self.text)
    }

    fn into_string(self) -> String {
        self.text
    }
}

// ── `PurePath.suffix` / `.stem` ──────────────────────────────────────────────

/// `PurePath.suffix` — `name[i:]` where `0 < i < len(name) - 1`.
///
/// The bounds are why `.md` (a dotfile literally named that) has **no** suffix
/// and `notes.md.` has none either. Byte indices stand in for CPython's code
/// point indices safely: `.` is one byte, so `i > 0` and `i < len - 1` mean the
/// same thing in both units.
fn py_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[index..],
        _ => "",
    }
}

/// `PurePath.stem` — the name minus [`py_suffix`].
fn py_stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => &name[..index],
        _ => name,
    }
}

// ── slice builders ───────────────────────────────────────────────────────────

/// `_system_prompt_slice()` — the only slice with a `None` `source_path`.
fn system_prompt_slice() -> ContextSlice {
    ContextSlice::new("system_prompt", DEFAULT_SYSTEM_PROMPT_TOKENS, None)
}

/// `_memory_slice(name, path)`.
///
/// A missing file still produces a slice, with zero tokens and the path it
/// looked for — that visibility is the point, and dropping the slice would be a
/// shape change on every machine without a project `CLAUDE.md`.
fn memory_slice(name: &str, path: &PyPath) -> ContextSlice {
    if !path.as_path().exists() {
        return ContextSlice::new(name, 0, Some(path.text.clone()));
    }
    let text = read_text(path.as_path());
    ContextSlice::new(name, estimate_tokens(&text), Some(path.text.clone()))
}

/// `_mcp_servers_from_claude_json` / `_mcp_servers_from_settings`, which are
/// the same function twice — only the debug log line differs.
///
/// Returns the dict-valued entries **sorted by name**, which is the
/// `for name, defn in sorted(servers.items())` the callers do. Older configs
/// held stub strings under `mcpServers`, hence the `isinstance(v, dict)` filter.
fn mcp_servers(path: &PyPath) -> Vec<(String, Value)> {
    if !path.as_path().exists() {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(path.as_path()) else {
        return Vec::new();
    };
    let raw = universal_newlines(&String::from_utf8_lossy(&bytes));
    // `except json.JSONDecodeError: return {}`.
    let Ok(data) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    // DIV-100: Python's `data.get("mcpServers")` raises `AttributeError` when
    // the file parses to a non-dict (`[1,2]`, `5`, `null`), and that is NOT in
    // the `except` clause — it escapes to a 500. Narrowed here to "no servers".
    let Some(Value::Object(servers)) = data.get("mcpServers") else {
        // `if not isinstance(servers, dict): return {}`.
        return Vec::new();
    };
    let mut out: Vec<(String, Value)> = servers
        .iter()
        .filter(|(_, value)| value.is_object())
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    out.sort_by(|(left, _), (right, _)| left.cmp(right));
    out
}

/// `_mcp_server_slice(name, definition, source_path)`.
///
/// `isinstance(tools, list)` is the whole branch: an explicit array is priced
/// per element (even an empty one, which costs only the base), anything else —
/// absent, an object, a number — takes the flat unknown-tools fee.
fn mcp_server_slice(name: &str, definition: &Value, source_path: &PyPath) -> ContextSlice {
    let tool_cost = match definition.get("tools") {
        Some(Value::Array(tools)) => {
            MCP_PER_TOOL_TOKENS * i64::try_from(tools.len()).unwrap_or(i64::MAX)
        }
        _ => MCP_UNKNOWN_TOOLS_FALLBACK,
    };
    ContextSlice::new(
        format!("mcp:{name}"),
        MCP_BASE_TOKENS + tool_cost,
        Some(source_path.text.clone()),
    )
}

/// `sorted(dir.iterdir())` — the child names, ordered.
///
/// Python sorts `Path` objects, which compares `_parts_normcase` element-wise;
/// every child of one directory shares every part but the last, so the order
/// reduces to a comparison of the names. `None` is the `except OSError: return []`
/// leg, and it covers a mid-iteration failure too because Python's `iterdir` is
/// a generator consumed *inside* the `try` by `sorted`.
fn sorted_child_names(dir: &Path) -> Option<Vec<String>> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.ok()?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Some(names)
}

/// `_skill_slices(skills_dir)` — one slice per `<skill>/SKILL.md`.
///
/// A child that is not a directory, or a directory with no `SKILL.md`, is
/// skipped silently. `Path.is_dir()` swallows its own `OSError` into `False`,
/// which is why the loop body needs no guard even though it sits outside the
/// reference's `try`.
fn skill_slices(skills_dir: &PyPath) -> Vec<ContextSlice> {
    if !skills_dir.as_path().is_dir() {
        // `if not skills_dir.exists() or not skills_dir.is_dir()` — `is_dir()`
        // already implies `exists()`, so one call answers both.
        return Vec::new();
    }
    let Some(names) = sorted_child_names(skills_dir.as_path()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in names {
        let child = skills_dir.join(&name);
        if !child.as_path().is_dir() {
            continue;
        }
        let skill_md = child.join("SKILL.md");
        if !skill_md.as_path().exists() {
            continue;
        }
        let text = read_text(skill_md.as_path());
        out.push(ContextSlice::new(
            format!("skill:{name}"),
            estimate_tokens(&text),
            Some(skill_md.into_string()),
        ));
    }
    out
}

/// `_agent_slices(agents_dir, scope=…)` — one slice per `*.md` file.
///
/// `p.suffix == ".md"` is case-SENSITIVE, so `REVIEWER.MD` is not an agent, and
/// `p.is_file()` means a *directory* called `linter.md` is not one either.
fn agent_slices(agents_dir: &PyPath, scope: &str) -> Vec<ContextSlice> {
    if !agents_dir.as_path().is_dir() {
        return Vec::new();
    }
    let Some(names) = sorted_child_names(agents_dir.as_path()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in names {
        if py_suffix(&name) != ".md" {
            continue;
        }
        let file = agents_dir.join(&name);
        if !file.as_path().is_file() {
            continue;
        }
        let text = read_text(file.as_path());
        out.push(ContextSlice::new(
            format!("agent:{scope}:{}", py_stem(&name)),
            estimate_tokens(&text),
            Some(file.into_string()),
        ));
    }
    out
}

// ── cost projection ──────────────────────────────────────────────────────────

/// `_project_cost(total_tokens)` → `(per_session, per_month)`.
///
/// `(total / 1_000_000.0) * 3.0` then `* 100`, in that order and no other: the
/// two multiplications do not commute in binary floating point, and the result
/// goes out through CPython's `repr`. 3000 tokens is `0.009000000000000001`
/// per session and `0.9000000000000001` per month — the trailing `1` is the
/// contract, and folding the arithmetic differently loses it.
fn project_cost(total_tokens: i64) -> (f64, f64) {
    #[allow(
        clippy::cast_precision_loss,
        reason = "Python's `int / float` promotes the int to a float exactly here"
    )]
    let per_session = (total_tokens as f64 / 1_000_000.0) * DEFAULT_INPUT_USD_PER_MILLION;
    #[allow(
        clippy::cast_precision_loss,
        reason = "the sessions-per-month constant is 100"
    )]
    let per_month = per_session * DEFAULT_SESSIONS_PER_MONTH as f64;
    (per_session, per_month)
}

/// `sum(s.tokens for s in slices)` — an **int** sum.
///
/// LAW 3: Python's `sum([])` is the int `0`, which renders `0` and not `0.0`.
/// The addends are ints too, so no compensated accumulator is involved — the
/// Neumaier rule applies to `sum()` over *floats*, and this is not one.
fn total_tokens(slices: &[ContextSlice]) -> i64 {
    slices.iter().map(|slice| slice.tokens).sum()
}

// ── public API ───────────────────────────────────────────────────────────────

/// `estimate_context_budget(project_dir, home_dir=home)`.
///
/// The slice order is the build order and it is the response order: system
/// prompt, project memory, global memory, global MCP servers (name-sorted),
/// project MCP servers (name-sorted, minus anything the global config already
/// charged), skills, project agents, global agents.
#[must_use]
pub fn estimate_context_budget(project_dir: &Path, home: &Path) -> ContextBudget {
    let home = PyPath::new(&home.to_string_lossy());
    // `project_dir = Path(project_dir)` — the normalising re-parse.
    let project_dir = PyPath::new(&project_dir.to_string_lossy());

    let mut slices = vec![
        system_prompt_slice(),
        memory_slice("memory:project_CLAUDE.md", &project_dir.join("CLAUDE.md")),
        memory_slice(
            "memory:global_CLAUDE.md",
            &home.join(".claude").join("CLAUDE.md"),
        ),
    ];

    let claude_json = home.join(".claude.json");
    let global_servers = mcp_servers(&claude_json);
    for (name, definition) in &global_servers {
        slices.push(mcp_server_slice(name, definition, &claude_json));
    }
    let project_settings = project_dir.join(".claude").join("settings.json");
    for (name, definition) in &mcp_servers(&project_settings) {
        // `if name in global_servers: continue` — same name, same description
        // budget, charged once. The membership test is against the *filtered*
        // global map, so a global entry that was not a dict does not shadow.
        if global_servers.iter().any(|(known, _)| known == name) {
            continue;
        }
        slices.push(mcp_server_slice(name, definition, &project_settings));
    }

    slices.extend(skill_slices(&home.join(".claude").join("skills")));
    slices.extend(agent_slices(
        &project_dir.join(".claude").join("agents"),
        "project",
    ));
    slices.extend(agent_slices(&home.join(".claude").join("agents"), "global"));

    let total = total_tokens(&slices);
    let (per_session, per_month) = project_cost(total);
    ContextBudget::new(total, slices, per_session, per_month)
}

/// `estimate_global_budget(home_dir=home)`.
///
/// "Global" is everything that loads whatever project the user is in. The three
/// project-only sources — the project `CLAUDE.md`, `.claude/settings.json` and
/// `.claude/agents/` — are excluded, so a client can tell the two payloads
/// apart by the absence of `memory:project_CLAUDE.md`.
///
/// `routes/context_budget.py` answers with this shape in **two** situations,
/// not one: no `project` parameter at all, and a known slug whose stored path
/// is empty or gone. See `routes/context_budget.rs`.
#[must_use]
pub fn estimate_global_budget(home: &Path) -> ContextBudget {
    let home = PyPath::new(&home.to_string_lossy());

    let mut slices = vec![
        system_prompt_slice(),
        memory_slice(
            "memory:global_CLAUDE.md",
            &home.join(".claude").join("CLAUDE.md"),
        ),
    ];

    let claude_json = home.join(".claude.json");
    for (name, definition) in &mcp_servers(&claude_json) {
        slices.push(mcp_server_slice(name, definition, &claude_json));
    }

    slices.extend(skill_slices(&home.join(".claude").join("skills")));
    slices.extend(agent_slices(&home.join(".claude").join("agents"), "global"));

    let total = total_tokens(&slices);
    let (per_session, per_month) = project_cost(total);
    ContextBudget::new(total, slices, per_session, per_month)
}

/// `Path.home()` — the OS home, unrelated to `$STACKUNDERFLOW_HOME`.
///
/// The estimator takes the home as a parameter (the injection law), so somebody
/// has to resolve it; before wave 8 that was a private helper in
/// `stax_server::routes::context_budget` and `stax-cli` would have needed a
/// second copy. One owner per helper, so it lives with the estimator it feeds.
///
/// `std::env::home_dir` is deprecated and used anyway: it is the platform-correct
/// answer on the pinned toolchain and it is what `stax_core::settings` already
/// calls, so a hand-rolled `$HOME` read here would be a THIRD answer to "where is
/// home" in one workspace.
#[must_use]
pub fn os_home() -> std::path::PathBuf {
    #[allow(
        deprecated,
        reason = "matches stax_core::settings — the platform-correct answer on the pinned toolchain"
    )]
    std::env::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A scratch directory that no other test in the crate can collide with.
    fn scratch(tag: &str, line: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stax-ctxbudget-{tag}-{}-{line}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(path, bytes).expect("write");
    }

    fn names(budget: &ContextBudget) -> Vec<&str> {
        budget.slices.iter().map(|s| s.name.as_str()).collect()
    }

    fn tokens_of(budget: &ContextBudget, name: &str) -> i64 {
        budget
            .slices
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no slice named {name}"))
            .tokens
    }

    #[test]
    fn the_heuristic_string_is_the_f_string_over_the_constants() {
        assert_eq!(
            HEURISTIC,
            format!(
                "len(text) // {CHARS_PER_TOKEN}; per-MCP-server \
                 {MCP_BASE_TOKENS} + {MCP_PER_TOOL_TOKENS}/tool"
            )
        );
    }

    #[test]
    fn the_four_char_heuristic_floors_and_counts_code_points_not_bytes() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens(&"a".repeat(16)), 4);
        // 17 chars is still 4 — floored, never rounded.
        assert_eq!(estimate_tokens(&"a".repeat(17)), 4);
        assert_eq!(estimate_tokens(&"a".repeat(20)), 5);
        // 8 code points, 16 UTF-8 bytes. A byte count would say 4.
        assert_eq!("é".repeat(8).len(), 16);
        assert_eq!(estimate_tokens(&"é".repeat(8)), 2);
        // 8 code points, 32 bytes. A byte count would say 8.
        assert_eq!(estimate_tokens(&"🙂".repeat(8)), 2);
    }

    #[test]
    fn crlf_is_translated_to_one_newline_before_the_length_is_taken() {
        let dir = scratch("crlf", line!());
        let file = dir.join("CLAUDE.md");
        // 12 bytes on disk; `read_text` sees "a\nb\nc\nd\ne\nf\n" — 12 chars.
        write(&file, b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n");
        assert_eq!(read_text(&file), "a\nb\nc\nd\ne\nf\n");
        assert_eq!(estimate_tokens(&read_text(&file)), 3);
        // A bare CR is a newline too, and `\r\r\n` is TWO of them.
        write(&file, b"a\rb\r\r\nc");
        assert_eq!(read_text(&file), "a\nb\n\nc");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn invalid_utf8_becomes_one_replacement_char_per_maximal_subpart() {
        let dir = scratch("utf8", line!());
        let file = dir.join("CLAUDE.md");
        // Each of these was measured against CPython's `errors="replace"`.
        for (bytes, expected) in [
            (&b"\xff"[..], 1_usize),
            (&b"\xc3"[..], 1),
            (&b"\xe2\x82"[..], 1),
            (&b"\xf0\x9f\x92"[..], 1),
            (&b"\x80\x80"[..], 2),
            (&b"\xed\xa0\x80"[..], 3),
        ] {
            write(&file, bytes);
            assert_eq!(
                read_text(&file).chars().count(),
                expected,
                "{bytes:?} replaces into {expected} chars"
            );
        }
        // A BOM is NOT stripped — `encoding="utf-8"`, not `utf-8-sig`.
        write(&file, b"\xef\xbb\xbfabcd");
        assert_eq!(read_text(&file).chars().count(), 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_or_missing_file_is_zero_tokens_and_never_an_error() {
        let dir = scratch("missing", line!());
        // A directory where a file is expected: `open()` raises IsADirectoryError.
        std::fs::create_dir_all(dir.join("CLAUDE.md")).expect("mkdir");
        assert_eq!(read_text(&dir.join("CLAUDE.md")), "");
        assert_eq!(read_text(&dir.join("nope.md")), "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn source_paths_are_pathlib_normalised_not_string_concatenated() {
        assert_eq!(py_path_str("/a//b/./c/"), "/a/b/c");
        assert_eq!(py_path_str("/"), "/");
        assert_eq!(py_path_str(""), ".");
        assert_eq!(py_path_str("."), ".");
        // `..` is KEPT — pathlib resolves nothing.
        assert_eq!(py_path_str("/a/../b"), "/a/../b");
        // Exactly two leading slashes are a root of their own; three are not.
        assert_eq!(py_path_str("//srv/x"), "//srv/x");
        assert_eq!(py_path_str("///srv/x"), "/srv/x");
        // The root must not double when joined.
        assert_eq!(PyPath::new("/").join(".claude").text, "/.claude");
        assert_eq!(PyPath::new(".").join("x").text, "x");
        assert_eq!(PyPath::new("/a/").join("b").text, "/a/b");
    }

    #[test]
    fn a_dot_md_suffix_needs_a_stem_and_is_case_sensitive() {
        assert_eq!(py_suffix("linter.md"), ".md");
        assert_eq!(py_stem("linter.md"), "linter");
        assert_eq!(py_suffix("a.b.md"), ".md");
        assert_eq!(py_stem("a.b.md"), "a.b");
        // A dotfile literally named `.md` has NO suffix (`0 < i` fails).
        assert_eq!(py_suffix(".md"), "");
        // Nor does a trailing dot (`i < len - 1` fails).
        assert_eq!(py_suffix("notes.md."), "");
        assert_eq!(py_suffix("README"), "");
        assert_eq!(py_stem("README"), "README");
        assert_eq!(py_suffix("linter.MD"), ".MD");
    }

    #[test]
    fn an_empty_home_and_project_charge_only_the_system_prompt() {
        let root = scratch("empty", line!());
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&home).expect("mkdir");
        std::fs::create_dir_all(&project).expect("mkdir");

        let budget = estimate_context_budget(&project, &home);
        // The two memory slices are PRESENT with zero tokens — dropping them
        // would be a shape change on every unconfigured machine.
        assert_eq!(
            names(&budget),
            vec![
                "system_prompt",
                "memory:project_CLAUDE.md",
                "memory:global_CLAUDE.md",
            ]
        );
        assert_eq!(budget.total_tokens, DEFAULT_SYSTEM_PROMPT_TOKENS);
        assert_eq!(
            budget.slices[1].source_path.as_deref(),
            Some(project.join("CLAUDE.md").to_string_lossy().as_ref())
        );
        // The system prompt is the one slice with a null source.
        assert_eq!(budget.slices[0].source_path, None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_cost_projection_renders_cpythons_float_repr_not_ryus() {
        let root = scratch("cost", line!());
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&home).expect("mkdir");
        std::fs::create_dir_all(&project).expect("mkdir");

        let budget = estimate_context_budget(&project, &home);
        let rendered = stax_memory::pyjson::dumps_http(&budget.to_dict());
        // Both trailing digits are CPython's; `0.009` and `0.9` would be wrong,
        // and ryu would spell a small value `3e-6` where CPython says `3e-06`.
        assert!(
            rendered.contains(r#""cost_per_session_usd":0.009000000000000001"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#""estimated_monthly_cost_usd":0.9000000000000001"#),
            "{rendered}"
        );
        // `total_tokens` is an int sum, so it renders WITHOUT a `.0`.
        assert!(rendered.starts_with(r#"{"total_tokens":3000,"slices":["#));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn to_dict_key_order_and_slice_field_order_are_the_payload_contract() {
        let budget = ContextBudget::new(
            7,
            vec![ContextSlice::new("system_prompt", 7, None)],
            0.5,
            50.0,
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&budget.to_dict()),
            concat!(
                r#"{"total_tokens":7,"slices":[{"name":"system_prompt","tokens":7,"#,
                r#""source_path":null}],"cost_per_session_usd":0.5,"#,
                r#""estimated_monthly_cost_usd":50.0,"#,
                r#""heuristic":"len(text) // 4; per-MCP-server 200 + 50/tool"}"#
            )
        );
    }

    #[test]
    fn memory_files_are_charged_by_the_heuristic_at_both_scopes() {
        let root = scratch("memory", line!());
        let home = root.join("home");
        let project = root.join("project");
        write(&project.join("CLAUDE.md"), &b"a".repeat(400));
        write(&home.join(".claude").join("CLAUDE.md"), &b"b".repeat(800));

        let budget = estimate_context_budget(&project, &home);
        assert_eq!(tokens_of(&budget, "memory:project_CLAUDE.md"), 100);
        assert_eq!(tokens_of(&budget, "memory:global_CLAUDE.md"), 200);
        assert_eq!(budget.total_tokens, 3000 + 100 + 200);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mcp_servers_sort_by_name_and_only_an_explicit_list_is_priced_per_tool() {
        let root = scratch("mcp", line!());
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        // Deliberately written out of order, and with two entries that are not
        // objects — the `isinstance(v, dict)` filter drops both.
        write(
            &home.join(".claude.json"),
            br#"{"mcpServers": {"zulu": {"command": "z"},
                                "alpha": {"command": "a", "tools": ["t1","t2","t3"]},
                                "stub": "legacy-string",
                                "empty": {"tools": []},
                                "objtools": {"tools": {"a": 1}}}}"#,
        );
        let budget = estimate_context_budget(&project, &home);
        assert_eq!(
            names(&budget),
            vec![
                "system_prompt",
                "memory:project_CLAUDE.md",
                "memory:global_CLAUDE.md",
                "mcp:alpha",
                "mcp:empty",
                "mcp:objtools",
                "mcp:zulu",
            ]
        );
        assert_eq!(tokens_of(&budget, "mcp:alpha"), 200 + 3 * 50);
        // An empty list is still a list: base only, not the 200 fallback.
        assert_eq!(tokens_of(&budget, "mcp:empty"), 200);
        // An OBJECT under `tools` is not a list, so it takes the fallback.
        assert_eq!(tokens_of(&budget, "mcp:objtools"), 400);
        assert_eq!(tokens_of(&budget, "mcp:zulu"), 400);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_project_server_shadowed_by_a_global_one_is_not_double_charged() {
        let root = scratch("shadow", line!());
        let home = root.join("home");
        let project = root.join("project");
        write(
            &home.join(".claude.json"),
            br#"{"mcpServers": {"shared": {"command": "x"}}}"#,
        );
        write(
            &project.join(".claude").join("settings.json"),
            br#"{"mcpServers": {"shared": {"command": "x"}, "extra": {"command": "y"}}}"#,
        );
        let budget = estimate_context_budget(&project, &home);
        assert_eq!(
            names(&budget)
                .iter()
                .filter(|n| **n == "mcp:shared")
                .count(),
            1
        );
        // The project-only server IS charged, and lands after the global block.
        assert_eq!(
            names(&budget),
            vec![
                "system_prompt",
                "memory:project_CLAUDE.md",
                "memory:global_CLAUDE.md",
                "mcp:shared",
                "mcp:extra",
            ]
        );
        // …and it carries the PROJECT settings file as its source.
        let extra = budget
            .slices
            .iter()
            .find(|s| s.name == "mcp:extra")
            .expect("extra");
        assert!(
            extra
                .source_path
                .as_deref()
                .expect("source")
                .ends_with("/.claude/settings.json"),
            "{extra:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_malformed_or_non_object_mcp_config_contributes_no_slices() {
        let root = scratch("badjson", line!());
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        for raw in [
            &b"{not valid json"[..],
            // DIV-100: Python raises AttributeError on these three and 500s.
            b"[1, 2]",
            b"5",
            b"null",
            // Valid object, wrong type under the key.
            br#"{"mcpServers": ["alpha"]}"#,
            br#"{"other": {"alpha": {}}}"#,
        ] {
            write(&home.join(".claude.json"), raw);
            let budget = estimate_context_budget(&project, &home);
            assert!(
                !budget.slices.iter().any(|s| s.name.starts_with("mcp:")),
                "{raw:?} produced mcp slices"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn skills_need_a_skill_md_and_come_out_in_sorted_order() {
        let root = scratch("skills", line!());
        let home = root.join("home");
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        let skills = home.join(".claude").join("skills");
        write(&skills.join("zulu").join("SKILL.md"), &b"z".repeat(80));
        write(&skills.join("alpha").join("SKILL.md"), &b"x".repeat(400));
        write(&skills.join("beta").join("SKILL.md"), &b"y".repeat(200));
        std::fs::create_dir_all(skills.join("no-skill-md")).expect("mkdir");
        // A loose FILE in skills/ is not a skill — the `is_dir()` guard.
        write(&skills.join("README.md"), b"ignored");
        // A different filename is not a skill.  Do not use a case-only
        // spelling here: case-insensitive filesystems resolve it as SKILL.md.
        write(
            &skills.join("not-a-skill").join("INSTRUCTIONS.md"),
            b"ignored",
        );

        let budget = estimate_context_budget(&project, &home);
        assert_eq!(
            names(&budget),
            vec![
                "system_prompt",
                "memory:project_CLAUDE.md",
                "memory:global_CLAUDE.md",
                "skill:alpha",
                "skill:beta",
                "skill:zulu",
            ]
        );
        assert_eq!(tokens_of(&budget, "skill:alpha"), 100);
        assert_eq!(tokens_of(&budget, "skill:beta"), 50);
        assert_eq!(tokens_of(&budget, "skill:zulu"), 20);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn agents_are_scoped_project_before_global_and_only_dot_md_files_count() {
        let root = scratch("agents", line!());
        let home = root.join("home");
        let project = root.join("project");
        write(
            &project.join(".claude").join("agents").join("linter.md"),
            &b"p".repeat(80),
        );
        write(
            &home.join(".claude").join("agents").join("reviewer.md"),
            &b"g".repeat(160),
        );
        let globals = home.join(".claude").join("agents");
        // None of these are agents: wrong case, no stem, a directory.
        write(&globals.join("SHOUTY.MD"), b"x");
        write(&globals.join(".md"), b"x");
        std::fs::create_dir_all(globals.join("dir.md")).expect("mkdir");
        // …but a multi-dot name is, and its stem keeps every dot but the last.
        write(&globals.join("a.b.md"), &b"q".repeat(40));

        let budget = estimate_context_budget(&project, &home);
        assert_eq!(
            names(&budget),
            vec![
                "system_prompt",
                "memory:project_CLAUDE.md",
                "memory:global_CLAUDE.md",
                "agent:project:linter",
                "agent:global:a.b",
                "agent:global:reviewer",
            ]
        );
        assert_eq!(tokens_of(&budget, "agent:project:linter"), 20);
        assert_eq!(tokens_of(&budget, "agent:global:reviewer"), 40);
        assert_eq!(tokens_of(&budget, "agent:global:a.b"), 10);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_global_budget_omits_every_project_artefact() {
        let root = scratch("global", line!());
        let home = root.join("home");
        let project = root.join("project");
        write(&project.join("CLAUDE.md"), &b"a".repeat(4000));
        write(
            &project.join(".claude").join("settings.json"),
            br#"{"mcpServers": {"projectonly": {}}}"#,
        );
        write(
            &project.join(".claude").join("agents").join("linter.md"),
            &b"p".repeat(80),
        );
        write(&home.join(".claude").join("CLAUDE.md"), &b"b".repeat(800));
        write(
            &home
                .join(".claude")
                .join("skills")
                .join("demo")
                .join("SKILL.md"),
            &b"z".repeat(80),
        );
        write(
            &home.join(".claude").join("agents").join("reviewer.md"),
            &b"g".repeat(160),
        );

        let budget = estimate_global_budget(&home);
        assert_eq!(
            names(&budget),
            vec![
                "system_prompt",
                "memory:global_CLAUDE.md",
                "skill:demo",
                "agent:global:reviewer",
            ]
        );
        assert_eq!(budget.total_tokens, 3000 + 200 + 20 + 40);
        // The project budget over the same two trees is a strict superset.
        let project_budget = estimate_context_budget(&project, &home);
        assert_eq!(
            names(&project_budget),
            vec![
                "system_prompt",
                "memory:project_CLAUDE.md",
                "memory:global_CLAUDE.md",
                "mcp:projectonly",
                "skill:demo",
                "agent:project:linter",
                "agent:global:reviewer",
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_home_that_does_not_exist_still_answers_with_the_system_prompt() {
        // The endpoint must work on a machine with no `~/.claude` at all.
        let budget = estimate_global_budget(Path::new("/nonexistent/home"));
        assert_eq!(
            names(&budget),
            vec!["system_prompt", "memory:global_CLAUDE.md"]
        );
        assert_eq!(budget.total_tokens, DEFAULT_SYSTEM_PROMPT_TOKENS);
        assert_eq!(
            budget.slices[1].source_path.as_deref(),
            Some("/nonexistent/home/.claude/CLAUDE.md")
        );
    }
}
