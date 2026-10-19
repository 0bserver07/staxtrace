//! The discovery read path — `services/discovery.py` + `services/risk.py`, ported.
//!
//! This is the substrate under the four wave-1 `memory` verbs (`sessions`,
//! `decisions`, `worked`, `file`). It is a **bug-for-bug** port: the Python
//! reference gathers candidates with leading-wildcard `LIKE` scans over
//! `messages.content_text` / `messages.tools_json`, and so does this. That path
//! has known-wrong behavior — `content_text` is empty on tool-call turns, so
//! agent-heavy sessions are largely invisible to it, and a multi-word phrase
//! silently matches nothing because `LIKE '%a b%'` is a literal substring test.
//! Both are ported faithfully (`docs/specs/rust-port.md` §6b, disposition
//! `bug-for-bug`); wave 6 fixes the substrate, not this file.
//!
//! Two things the reference does that this module deliberately does not:
//!
//! * **Telemetry writes.** `discovery._record_loaded` bumps `loaded_count` in
//!   `discovery_telemetry` for every surfaced session. The store handle here is
//!   opened `SQLITE_OPEN_READ_ONLY` (`store.rs`), so the write cannot happen and
//!   is skipped. It never reaches stdout, so parity is unaffected; the ranking
//!   input it feeds is unused today (`_build_rank_fn` has three terms, not four).
//! * **The FTS5 content half.** The Python CLI injects a `SearchService` into
//!   `search_past_decisions` / `find_sessions_touching_file` /
//!   `find_sessions_where_action_worked`; when `search_index.db` is populated
//!   those three route their free-text half through bm25 instead of `LIKE`.
//!   That path is `stax-memory`'s (RS-1-007, bm25 read path) and is **not**
//!   implemented here — this module is the `search_service=None` branch, which
//!   is also what every non-CLI Python caller gets. See the divergence note in
//!   `rust/TASKS-RS.md`.
//!
//! SQL shapes are ported literally, spacing included (§6b): `messages` is a
//! UNION-ALL view over 16 monthly partitions and SQLite does not push join
//! predicates into the arms, so an "idiomatic" rewrite silently reintroduces the
//! hangs the July perf campaign killed.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::Connection;

// ── Python-compatible JSON ───────────────────────────────────────────────────

/// A JSON model that round-trips the way `json.dumps` / `json.loads` do.
///
/// `serde_json` is not used, and not because of taste: byte-parity needs
/// `ensure_ascii=True` (every non-ASCII character escaped as `\uXXXX`), Python's
/// `repr`-shortest float formatting, insertion-ordered object keys, and the
/// int/float distinction the reference relies on (`message_count` is an int,
/// `cost_usd` a float — `8` and `8.0` are different bytes). All four are
/// properties of Python's encoder, so the encoder is what gets ported.
pub mod pyjson {
    use std::fmt::Write as _;

    /// One JSON value, with Python's int/float split preserved.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        /// `null` / `None`.
        Null,
        /// `true` / `false`.
        Bool(bool),
        /// A Python `int`.
        Int(i64),
        /// A Python `float` — always rendered with a `.` or an exponent.
        Float(f64),
        /// A Python `str`.
        Str(String),
        /// A Python `list`.
        Array(Vec<Value>),
        /// A Python `dict`, in insertion order.
        Object(Vec<(String, Value)>),
    }

    impl From<&String> for Value {
        fn from(value: &String) -> Self {
            Self::Str(value.clone())
        }
    }

    impl From<&str> for Value {
        fn from(value: &str) -> Self {
            Self::Str(value.to_string())
        }
    }

    impl Value {
        /// Look a key up in an object; `None` for every other variant.
        #[must_use]
        pub fn get(&self, key: &str) -> Option<&Self> {
            match self {
                Self::Object(entries) => entries
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value),
                _ => None,
            }
        }

        /// The string inside a [`Value::Str`], if that is what this is.
        #[must_use]
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Self::Str(text) => Some(text),
                _ => None,
            }
        }

        /// Python truthiness: `None`, `False`, `0`, `""`, `[]`, `{}` are falsy.
        ///
        /// Load-bearing for the `entry.get("input") or entry.get("arguments") or
        /// entry` chains in the tool-arg matchers: an empty dict falls through.
        #[must_use]
        pub fn is_truthy(&self) -> bool {
            match self {
                Self::Null => false,
                Self::Bool(value) => *value,
                Self::Int(value) => *value != 0,
                Self::Float(value) => *value != 0.0,
                Self::Str(text) => !text.is_empty(),
                Self::Array(items) => !items.is_empty(),
                Self::Object(entries) => !entries.is_empty(),
            }
        }
    }

    /// `json.dumps(obj, separators=(",", ":"))` — the compact form the token
    /// estimator measures.
    #[must_use]
    pub fn dumps_compact(value: &Value) -> String {
        let mut out = String::new();
        write_value(&mut out, value, ",", ":", None, 0);
        out
    }

    /// `json.dumps(obj)` — the *default* separators, `", "` and `": "`.
    ///
    /// This is what `_tools_json_mentions_file`'s last-ditch substring check
    /// serialises with, so the spaces matter.
    #[must_use]
    pub fn dumps_default(value: &Value) -> String {
        let mut out = String::new();
        write_value(&mut out, value, ", ", ": ", None, 0);
        out
    }

    /// `json.dumps(obj, indent=2)` — the envelope's wire form.
    #[must_use]
    pub fn dumps_indent2(value: &Value) -> String {
        let mut out = String::new();
        write_value(&mut out, value, ",", ": ", Some(2), 0);
        out
    }

    fn write_value(
        out: &mut String,
        value: &Value,
        item_sep: &str,
        key_sep: &str,
        indent: Option<usize>,
        depth: usize,
    ) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Int(number) => {
                let _ = write!(out, "{number}");
            }
            Value::Float(number) => out.push_str(&repr_float(*number)),
            Value::Str(text) => write_string(out, text),
            Value::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(item_sep);
                    }
                    write_newline_indent(out, indent, depth + 1);
                    write_value(out, item, item_sep, key_sep, indent, depth + 1);
                }
                write_newline_indent(out, indent, depth);
                out.push(']');
            }
            Value::Object(entries) => {
                if entries.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (index, (key, item)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push_str(item_sep);
                    }
                    write_newline_indent(out, indent, depth + 1);
                    write_string(out, key);
                    out.push_str(key_sep);
                    write_value(out, item, item_sep, key_sep, indent, depth + 1);
                }
                write_newline_indent(out, indent, depth);
                out.push('}');
            }
        }
    }

    fn write_newline_indent(out: &mut String, indent: Option<usize>, depth: usize) {
        if let Some(width) = indent {
            out.push('\n');
            for _ in 0..(width * depth) {
                out.push(' ');
            }
        }
    }

    /// `json.encoder.py_encode_basestring_ascii` — every non-ASCII character
    /// escaped, astral planes as surrogate pairs.
    fn write_string(out: &mut String, text: &str) {
        out.push('"');
        for ch in text.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                ch if (ch as u32) < 0x20 => {
                    let _ = write!(out, "\\u{:04x}", ch as u32);
                }
                ch if (ch as u32) < 0x7f => out.push(ch),
                ch => {
                    let code = ch as u32;
                    if code <= 0xffff {
                        let _ = write!(out, "\\u{code:04x}");
                    } else {
                        let value = code - 0x1_0000;
                        let high = 0xd800 + (value >> 10);
                        let low = 0xdc00 + (value & 0x3ff);
                        let _ = write!(out, "\\u{high:04x}\\u{low:04x}");
                    }
                }
            }
        }
        out.push('"');
    }

    /// `repr(float)` — CPython's shortest round-trip formatting.
    ///
    /// Rust's `Display` never uses exponent notation and Rust's `LowerExp`
    /// always does; CPython switches at `decpt <= -4 || decpt > 16` and always
    /// leaves a `.0` on an integral value. The shortest digits come from
    /// `{:e}` (which is round-trip shortest) except for the halfway tie
    /// [`tie_break_to_even`] repairs (DIV-008), and the placement is redone
    /// here.
    #[must_use]
    pub fn repr_float(value: f64) -> String {
        if value.is_nan() {
            return "NaN".to_string();
        }
        if value.is_infinite() {
            return if value > 0.0 {
                "Infinity".to_string()
            } else {
                "-Infinity".to_string()
            };
        }
        // DIV-024: `-0.0` IS sign-negative and CPython renders it "-0.0" —
        // the old `value != 0.0` clause wrongly stripped the sign there.
        let sign = if value.is_sign_negative() { "-" } else { "" };
        let magnitude = value.abs();
        if magnitude == 0.0 {
            return format!("{sign}0.0");
        }
        // `{:e}` → "d.dddde±X"; digits are the shortest round-trip set.
        let scientific = format!("{magnitude:e}");
        let (mantissa, exponent) = scientific
            .split_once('e')
            .unwrap_or((scientific.as_str(), "0"));
        let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
        let digits = digits.trim_end_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        let exponent: i32 = exponent.parse().unwrap_or(0);
        let decpt = exponent + 1;
        // DIV-008: the one place the two digit generators disagree.
        let retied = tie_break_to_even(digits, decpt, magnitude);
        let digits = retied.as_deref().unwrap_or(digits);

        if decpt <= -4 || decpt > 16 {
            let mut out = String::from(sign);
            out.push_str(&digits[..1]);
            if digits.len() > 1 {
                out.push('.');
                out.push_str(&digits[1..]);
            }
            let exp = decpt - 1;
            let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
            return out;
        }
        let mut out = String::from(sign);
        let len = i32::try_from(digits.len()).unwrap_or(i32::MAX);
        if decpt <= 0 {
            out.push_str("0.");
            for _ in 0..(-decpt) {
                out.push('0');
            }
            out.push_str(digits);
        } else if decpt >= len {
            out.push_str(digits);
            for _ in 0..(decpt - len) {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            let split = usize::try_from(decpt).unwrap_or(0);
            out.push_str(&digits[..split]);
            out.push('.');
            out.push_str(&digits[split..]);
        }
        out
    }

    // ── DIV-008: the halfway decimal tie ────────────────────────────────────────
    //
    // Rust's shortest-round-trip writer and CPython's `_Py_dg_dtoa` agree on the
    // *length* of the shortest digit string, and on the digits themselves whenever
    // one candidate of that length is strictly closer to the double than its
    // neighbour. They part company on the measure-zero case where the double's
    // exact value sits *exactly* halfway between two candidates that both parse
    // back to it: CPython selects the even final digit (`Python/dtoa.c`, mode 0 —
    // the arm guarded by `word1(&u) & 1`), Rust rounds away from zero.
    //
    // `-1352070300110077.2` is the reference repro: that double is exactly
    // …077.25, so …077.2 and …077.3 are equidistant and both parse to it. CPython
    // prints the first, `{:e}` the second. Measured incidence before the fix:
    // 65,839 of 5,158,262 adversarial doubles, and 0 of the 144,187 distinct
    // `REAL`s in the live store — invisible on today's data, but `cost_usd` in the
    // envelope is a SUM, not a stored value.
    //
    // The repair is one digit, so it is expressed as an exact integer question
    // rather than a second digit generator: is the midpoint between the two
    // candidates *equal* to the double? Both sides factor as (odd) × 2^a × 5^b,
    // and `m < 2^53` bounds `b` far inside `u128`. No bignum, no new dependency.
    //
    // THIS BLOCK IS DUPLICATED, DELIBERATELY, IN:
    //   * crates/stax-memory/src/pyjson.rs      (python_float_repr)
    //   * crates/stax-core/src/queries.rs       (pyjson::repr_float)
    // The two writers serve the same wire contract and the crates share no
    // dependency edge (stax-memory is serde-only by design; stax-core drags in
    // bundled SQLite). Keep them byte-identical — each crate's test module carries
    // the same case table, so drift fails a test rather than a golden.

    /// Re-break a halfway decimal tie the way CPython does: to the even digit.
    ///
    /// `digits` and `decpt` are the shortest round-trip digits `{:e}` produced —
    /// the value is `digits × 10^(decpt - digits.len())` — and `magnitude` is the
    /// non-negative double they describe. Returns `Some(replacement)` only when the
    /// double is *exactly* the midpoint between `digits` and an adjacent
    /// same-length candidate that also round-trips, and that neighbour is the even
    /// one; `None` leaves `{:e}`'s answer alone, which is the overwhelmingly
    /// common path and costs one parity test.
    fn tie_break_to_even(digits: &str, decpt: i32, magnitude: f64) -> Option<String> {
        // An even final digit is already CPython's answer: either there was no tie
        // at all, or `{:e}` rounded up *into* the even digit and both agree.
        if *digits.as_bytes().last()? % 2 == 0 {
            return None;
        }
        // 17 digits is `f64`'s shortest-repr ceiling, so `10 * d + 5` cannot
        // overflow `u64`; anything longer did not come from this writer.
        let value: u64 = digits.parse().ok()?;
        let scale = decpt - i32::try_from(digits.len()).ok()?;

        for (midpoint, neighbour) in [(10 * value - 5, value - 1), (10 * value + 5, value + 1)] {
            if !is_exact_decimal(midpoint, scale - 1, magnitude) {
                continue;
            }
            // A genuine tie is between two candidates of the same length — a
            // shorter neighbour would already have been the shortest repr — and
            // the neighbour has to round-trip. At a binade floor the gap below the
            // double is half the gap above it, so a lower candidate can be an
            // exact midpoint and still parse to the *previous* double: `2^-24` is
            // the live case, where CPython prints 5.960464477539063e-08 and the
            // even-looking 5.960464477539062e-08 is a different float.
            let replacement = neighbour.to_string();
            if replacement.len() != digits.len() {
                break;
            }
            match format!("{replacement}e{scale}").parse::<f64>() {
                Ok(parsed) if parsed.to_bits() == magnitude.to_bits() => return Some(replacement),
                _ => break,
            }
        }
        None
    }

    /// Is `n × 10^exp10` *exactly* the value of the finite, non-negative
    /// `magnitude`?
    ///
    /// `magnitude` is `m × 2^e` with `m < 2^53`. Splitting both sides into an odd
    /// factor and a power of two reduces the question to two integer identities —
    /// the odd parts must match and the 2-exponents must match. The odd parts carry
    /// the `5^k` out of `10^k`, and `m < 2^53` caps the reachable `k` at roughly
    /// ±27 before one side can no longer equal the other, so the running product in
    /// [`times_pow5_eq`] stays well inside `u128`.
    fn is_exact_decimal(n: u64, exp10: i32, magnitude: f64) -> bool {
        let bits = magnitude.to_bits();
        let raw_exponent = ((bits >> 52) & 0x7ff) as i32;
        let fraction = bits & ((1 << 52) - 1);
        // Subnormals carry no implicit leading bit and a fixed exponent.
        let (m, e) = if raw_exponent == 0 {
            (fraction, -1074)
        } else {
            (fraction | (1 << 52), raw_exponent - 1075)
        };
        if m == 0 {
            return n == 0;
        }
        let m_twos = m.trailing_zeros() as i32;
        let m_odd = m >> m_twos;
        let n_twos = n.trailing_zeros() as i32;
        let n_odd = n >> n_twos;
        if exp10 >= 0 {
            // n_odd·5^exp10 · 2^(n_twos + exp10)  ==  m_odd · 2^(m_twos + e)
            n_twos + exp10 == m_twos + e && times_pow5_eq(n_odd, exp10.unsigned_abs(), m_odd)
        } else {
            // n_odd · 2^n_twos  ==  m_odd·5^(-exp10) · 2^(m_twos + e - exp10)
            n_twos == m_twos + e - exp10 && times_pow5_eq(m_odd, exp10.unsigned_abs(), n_odd)
        }
    }

    /// `a × 5^k == b`, without overflow: the running product is checked against `b`
    /// after every step, so it never climbs past `u64::MAX`.
    fn times_pow5_eq(a: u64, k: u32, b: u64) -> bool {
        let mut product = u128::from(a);
        let limit = u128::from(b);
        for _ in 0..k {
            product *= 5;
            if product > limit {
                return false;
            }
        }
        product == limit
    }

    /// `json.loads` for the subset the store's `tools_json` blobs contain.
    ///
    /// Returns `None` on anything Python's decoder would reject — the callers
    /// treat a `JSONDecodeError` as "no match", so the error detail is unused.
    #[must_use]
    pub fn loads(text: &str) -> Option<Value> {
        let bytes: Vec<char> = text.chars().collect();
        let mut cursor = 0usize;
        let value = parse_value(&bytes, &mut cursor)?;
        skip_whitespace(&bytes, &mut cursor);
        (cursor == bytes.len()).then_some(value)
    }

    fn skip_whitespace(chars: &[char], cursor: &mut usize) {
        while *cursor < chars.len() && matches!(chars[*cursor], ' ' | '\t' | '\n' | '\r') {
            *cursor += 1;
        }
    }

    fn parse_value(chars: &[char], cursor: &mut usize) -> Option<Value> {
        skip_whitespace(chars, cursor);
        match chars.get(*cursor)? {
            '{' => parse_object(chars, cursor),
            '[' => parse_array(chars, cursor),
            '"' => parse_string(chars, cursor).map(Value::Str),
            't' => parse_literal(chars, cursor, "true", Value::Bool(true)),
            'f' => parse_literal(chars, cursor, "false", Value::Bool(false)),
            'n' => parse_literal(chars, cursor, "null", Value::Null),
            _ => parse_number(chars, cursor),
        }
    }

    fn parse_literal(
        chars: &[char],
        cursor: &mut usize,
        word: &str,
        value: Value,
    ) -> Option<Value> {
        for (offset, expected) in word.chars().enumerate() {
            if chars.get(*cursor + offset) != Some(&expected) {
                return None;
            }
        }
        *cursor += word.chars().count();
        Some(value)
    }

    fn parse_object(chars: &[char], cursor: &mut usize) -> Option<Value> {
        *cursor += 1; // '{'
        let mut entries: Vec<(String, Value)> = Vec::new();
        skip_whitespace(chars, cursor);
        if chars.get(*cursor) == Some(&'}') {
            *cursor += 1;
            return Some(Value::Object(entries));
        }
        loop {
            skip_whitespace(chars, cursor);
            let key = parse_string(chars, cursor)?;
            skip_whitespace(chars, cursor);
            if chars.get(*cursor) != Some(&':') {
                return None;
            }
            *cursor += 1;
            let value = parse_value(chars, cursor)?;
            // Python keeps the LAST value for a duplicate key, at the original
            // key's position.
            match entries.iter_mut().find(|(name, _)| *name == key) {
                Some(slot) => slot.1 = value,
                None => entries.push((key, value)),
            }
            skip_whitespace(chars, cursor);
            match chars.get(*cursor) {
                Some(',') => *cursor += 1,
                Some('}') => {
                    *cursor += 1;
                    return Some(Value::Object(entries));
                }
                _ => return None,
            }
        }
    }

    fn parse_array(chars: &[char], cursor: &mut usize) -> Option<Value> {
        *cursor += 1; // '['
        let mut items: Vec<Value> = Vec::new();
        skip_whitespace(chars, cursor);
        if chars.get(*cursor) == Some(&']') {
            *cursor += 1;
            return Some(Value::Array(items));
        }
        loop {
            items.push(parse_value(chars, cursor)?);
            skip_whitespace(chars, cursor);
            match chars.get(*cursor) {
                Some(',') => *cursor += 1,
                Some(']') => {
                    *cursor += 1;
                    return Some(Value::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn parse_string(chars: &[char], cursor: &mut usize) -> Option<String> {
        if chars.get(*cursor) != Some(&'"') {
            return None;
        }
        *cursor += 1;
        let mut out = String::new();
        loop {
            let ch = *chars.get(*cursor)?;
            *cursor += 1;
            match ch {
                '"' => return Some(out),
                '\\' => {
                    let escape = *chars.get(*cursor)?;
                    *cursor += 1;
                    match escape {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\u{08}'),
                        'f' => out.push('\u{0c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let first = parse_hex4(chars, cursor)?;
                            let code = if (0xd800..0xdc00).contains(&first)
                                && chars.get(*cursor) == Some(&'\\')
                                && chars.get(*cursor + 1) == Some(&'u')
                            {
                                *cursor += 2;
                                let second = parse_hex4(chars, cursor)?;
                                0x1_0000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                            } else {
                                first
                            };
                            // Lone surrogates are legal in Python's decoder;
                            // Rust `char` cannot hold one, so it becomes U+FFFD.
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        _ => return None,
                    }
                }
                ch => out.push(ch),
            }
        }
    }

    fn parse_hex4(chars: &[char], cursor: &mut usize) -> Option<u32> {
        let mut code = 0u32;
        for _ in 0..4 {
            let digit = chars.get(*cursor)?.to_digit(16)?;
            code = code * 16 + digit;
            *cursor += 1;
        }
        Some(code)
    }

    fn parse_number(chars: &[char], cursor: &mut usize) -> Option<Value> {
        let start = *cursor;
        if chars.get(*cursor) == Some(&'-') {
            *cursor += 1;
        }
        let mut is_float = false;
        while let Some(ch) = chars.get(*cursor) {
            match ch {
                '0'..='9' => *cursor += 1,
                '.' | 'e' | 'E' | '+' | '-' => {
                    is_float = true;
                    *cursor += 1;
                }
                _ => break,
            }
        }
        if *cursor == start {
            return None;
        }
        let literal: String = chars[start..*cursor].iter().collect();
        if is_float {
            literal.parse::<f64>().ok().map(Value::Float)
        } else {
            match literal.parse::<i64>() {
                Ok(number) => Some(Value::Int(number)),
                Err(_) => literal.parse::<f64>().ok().map(Value::Float),
            }
        }
    }
}

// ── the exception the CLI actually catches ───────────────────────────────────

/// The Python `ValueError` that `cli._memory_fail`'s `except ValueError` catches.
///
/// Every `memory` verb wraps its query in `except ValueError` and *only* that:
/// a `sqlite3.DatabaseError` (a corrupt store, a `LIKE` pattern SQLite refuses)
/// propagates out of Click and exits 1 with a traceback. Before this type
/// existed every failure in this crate was an anonymous `anyhow` error, so the
/// CLI funnelled all of them into `Invalid value for --since` — blaming an
/// option the caller may never have passed, and turning a DB failure into
/// exit 2. Errors raised here carry this marker; errors from `rusqlite` do not,
/// and the CLI re-raises those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError(pub String);

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ValueError {}

impl ValueError {
    /// The message, when `error` is (or wraps) a Python-`ValueError` equivalent.
    #[must_use]
    pub fn of(error: &anyhow::Error) -> Option<&str> {
        error.downcast_ref::<Self>().map(|inner| inner.0.as_str())
    }
}

// ── Python-compatible integers ───────────────────────────────────────────────

/// `int(str)` and the arbitrary-precision value it produces.
///
/// Click's `type=int` is CPython's `int()`, which accepts a great deal more than
/// `str::parse::<i64>`: surrounding whitespace, a leading `+`, `_` separators
/// between digits, decimal digits from *any* script (`٧` is seven), and integers
/// of unbounded size. Every one of those was an exit-2 rejection here while
/// Click exited 0, so this module is the parser clap's `value_parser` uses.
///
/// Values are held as a sign plus canonical decimal digits, because the CLI
/// echoes the number back in the `--json` envelope (`"limit": 5`) and the echo
/// has to be the *normalised* form Python prints — `' 5'`, `+5` and `٥` all
/// print as `5`.
pub mod pyint {
    /// A parsed `int()` value: sign plus canonical decimal digits.
    ///
    /// `digits` never carries a leading zero (except the literal `"0"`), so
    /// [`Display`](std::fmt::Display) is exactly what CPython's `str(int)` gives.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PyInt {
        negative: bool,
        digits: String,
    }

    /// Every Unicode code point whose decimal digit value is 0.
    ///
    /// CPython's `int()` accepts any character with a `Numeric_Type=Decimal`
    /// property, whose code points come in contiguous runs of ten. Generated
    /// from the reference interpreter's own `unicodedata` (3.12.13, UCD 15.0.0)
    /// and verified to cover every decimal character with no irregular run —
    /// the table is data, not a guess.
    const DECIMAL_ZEROS: [u32; 68] = [
        0x0_0030, 0x0_0660, 0x0_06F0, 0x0_07C0, 0x0_0966, 0x0_09E6, 0x0_0A66, 0x0_0AE6, 0x0_0B66,
        0x0_0BE6, 0x0_0C66, 0x0_0CE6, 0x0_0D66, 0x0_0DE6, 0x0_0E50, 0x0_0ED0, 0x0_0F20, 0x0_1040,
        0x0_1090, 0x0_17E0, 0x0_1810, 0x0_1946, 0x0_19D0, 0x0_1A80, 0x0_1A90, 0x0_1B50, 0x0_1BB0,
        0x0_1C40, 0x0_1C50, 0x0_A620, 0x0_A8D0, 0x0_A900, 0x0_A9D0, 0x0_A9F0, 0x0_AA50, 0x0_ABF0,
        0x0_FF10, 0x1_04A0, 0x1_0D30, 0x1_1066, 0x1_10F0, 0x1_1136, 0x1_11D0, 0x1_12F0, 0x1_1450,
        0x1_14D0, 0x1_1650, 0x1_16C0, 0x1_1730, 0x1_18E0, 0x1_1950, 0x1_1C50, 0x1_1D50, 0x1_1DA0,
        0x1_1F50, 0x1_6A60, 0x1_6AC0, 0x1_6B50, 0x1_D7CE, 0x1_D7D8, 0x1_D7E2, 0x1_D7EC, 0x1_D7F6,
        0x1_E140, 0x1_E2F0, 0x1_E4F0, 0x1_E950, 0x1_FBF0,
    ];

    /// `Py_UNICODE_TODECIMAL` — the digit value of any decimal character.
    ///
    /// ASCII first because that is every real invocation; the table walk is the
    /// `٧`-shaped tail.
    #[must_use]
    pub fn decimal_value(ch: char) -> Option<u32> {
        if let Some(value) = ch.to_digit(10) {
            return Some(value);
        }
        let code = ch as u32;
        DECIMAL_ZEROS
            .iter()
            .find(|zero| (**zero..**zero + 10).contains(&code))
            .map(|zero| code - zero)
    }

    /// The whitespace `int()` strips — Unicode `White_Space`.
    ///
    /// Deliberately *not* `str.isspace()`: that set also holds U+001C–U+001F,
    /// and `int('\x1c5')` raises where `int('\u{a0}5')` returns 5. Measured
    /// against CPython 3.12 rather than reasoned about.
    #[must_use]
    pub fn is_int_space(ch: char) -> bool {
        ch.is_whitespace()
    }

    /// `str.isspace()` — what a `re` pattern's `\s` matches.
    ///
    /// The regex class is the wider one: `re.match(r'^\s*\d+', '\x1c7')` hits.
    /// `parse_since`'s relative-window regex needs this, `int()` needs the other.
    #[must_use]
    pub fn is_regex_space(ch: char) -> bool {
        ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
    }

    impl PyInt {
        /// `int(raw)` — `None` where CPython raises `ValueError`.
        ///
        /// Base 10 only, which is all Click's `type=int` uses.
        #[must_use]
        pub fn parse(raw: &str) -> Option<Self> {
            let text = raw.trim_matches(is_int_space);
            let mut chars = text.chars().peekable();
            let negative = match chars.peek() {
                Some('-') => {
                    chars.next();
                    true
                }
                Some('+') => {
                    chars.next();
                    false
                }
                _ => false,
            };
            let mut digits = String::new();
            let mut previous_was_underscore = true; // a leading `_` is an error
            for ch in chars {
                if ch == '_' {
                    if previous_was_underscore {
                        return None;
                    }
                    previous_was_underscore = true;
                    continue;
                }
                digits.push(char::from_digit(decimal_value(ch)?, 10)?);
                previous_was_underscore = false;
            }
            if digits.is_empty() || previous_was_underscore {
                return None; // "", "+", "5_"
            }
            let trimmed = digits.trim_start_matches('0');
            let digits = if trimmed.is_empty() {
                "0".to_string()
            } else {
                trimmed.to_string()
            };
            Some(Self {
                negative: negative && digits != "0",
                digits,
            })
        }

        /// The value as an `i64`, saturating at the bounds.
        ///
        /// Saturation is exact for every use inside the port: `--limit` is a cap
        /// compared against a result count and `--context-budget` a token budget
        /// compared against an estimate, and no result set approaches 2⁶³. The
        /// one place it is *not* free is a `LIMIT ?` bind — see [`super::Limit`].
        #[must_use]
        pub fn saturating_i64(&self) -> i64 {
            self.fits_i64()
                .unwrap_or(if self.negative { i64::MIN } else { i64::MAX })
        }

        /// The value when it fits an `i64`, else `None`.
        #[must_use]
        pub fn fits_i64(&self) -> Option<i64> {
            let magnitude = self.digits.parse::<i128>().ok()?;
            i64::try_from(if self.negative { -magnitude } else { magnitude }).ok()
        }

        /// The value when it fits a `u64`, else `None` — the widest integer
        /// `serde_json` can render exactly without `arbitrary_precision`.
        #[must_use]
        pub fn fits_u64(&self) -> Option<u64> {
            if self.negative {
                return None;
            }
            self.digits.parse::<u64>().ok()
        }

        /// True when this is `> 0` — Python's `limit and limit > 0` guard.
        #[must_use]
        pub fn is_positive(&self) -> bool {
            !self.negative && self.digits != "0"
        }
    }

    impl From<i64> for PyInt {
        fn from(value: i64) -> Self {
            Self {
                negative: value < 0,
                digits: value.unsigned_abs().to_string(),
            }
        }
    }

    impl std::fmt::Display for PyInt {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if self.negative {
                f.write_str("-")?;
            }
            f.write_str(&self.digits)
        }
    }
}

/// A `--limit` on its way to a `LIMIT ?` bind.
///
/// `sqlite3` raises `OverflowError: Python int too large to convert to SQLite
/// INTEGER` when a limit above 2⁶³−1 is bound, and that is **not** a
/// `ValueError`, so Click does not catch it: `memory sessions <path> --limit
/// 99999999999999999999` exits 1 with a traceback and an empty stdout while
/// `memory decisions … --limit 99999999999999999999` exits 0, because
/// `search_past_decisions` compares the cap in Python and never binds it.
/// Saturating silently would have turned that failure into a full result set —
/// a changed answer, which is the worst kind of divergence — so the overflow
/// travels with the value and fires at the bind, exactly where Python's does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    value: i64,
    overflowed: bool,
}

impl From<i64> for Limit {
    fn from(value: i64) -> Self {
        Self {
            value,
            overflowed: false,
        }
    }
}

impl From<&pyint::PyInt> for Limit {
    fn from(value: &pyint::PyInt) -> Self {
        Self {
            value: value.saturating_i64(),
            overflowed: value.fits_i64().is_none(),
        }
    }
}

impl Limit {
    /// The cap as compared in Python — saturated, which no result set reaches.
    #[must_use]
    pub fn as_i64(self) -> i64 {
        self.value
    }

    /// `limit and limit > 0` — whether a `LIMIT` clause is appended at all.
    #[must_use]
    pub fn is_positive(self) -> bool {
        self.value > 0
    }

    /// The bound parameter, or `sqlite3`'s `OverflowError`.
    ///
    /// # Errors
    /// When the original literal did not fit a 64-bit integer.
    pub fn bind(self) -> Result<i64> {
        if self.overflowed {
            anyhow::bail!("Python int too large to convert to SQLite INTEGER");
        }
        Ok(self.value)
    }
}

// ── Python-compatible time ───────────────────────────────────────────────────

/// The slice of `datetime` the discovery path needs: ISO parsing, `isoformat`
/// rendering, and `parse_since`'s relative-window arithmetic.
///
/// Injected-clock throughout (finding 5: `std::env::set_var` is `unsafe` in Rust
/// 2024 and this workspace forbids `unsafe`, so nothing here reads a global).
pub mod pytime {
    use std::fmt::Write as _;
    // wasm32 has no clock; `now_micros` is injected there instead (see below).
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::{Result, bail};

    /// Seconds since the Unix epoch, with microsecond resolution.
    pub type Epoch = f64;

    /// `datetime.now(UTC)` as microseconds since the epoch.
    #[must_use]
    #[cfg(not(target_arch = "wasm32"))]
    pub fn now_micros() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX)
            })
    }

    /// The clock the host injected — `wasm32-unknown-unknown` has none of its own.
    ///
    /// **WAVE 9, and it is a real platform limit, not a preference.** On
    /// `wasm32-unknown-unknown` `std::time::SystemTime::now()` does not return an
    /// error, it **panics** ("time not implemented on this platform"), and
    /// [`parse_since`] evaluates `now_micros()` *eagerly* — as an argument —
    /// even for `since = None`. So every discovery query, `--since` or not,
    /// aborted the module until this existed. The injected value is written once
    /// by [`set_now_micros`] before the first query, from JS's `Date.now()`.
    ///
    /// Zero until set, which reads as 1970: a `--since 7d` would then resolve to
    /// a 1969 lower bound and match everything. That is the *safe* direction for
    /// a read-only browser demo (it over-includes rather than silently hiding
    /// rows), and `crates/stax-wasm/src/js.rs` sets the clock in its constructor
    /// so the window never opens in practice.
    #[must_use]
    #[cfg(target_arch = "wasm32")]
    pub fn now_micros() -> i64 {
        WASM_NOW_MICROS.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The host's clock, in microseconds since the epoch.
    #[cfg(target_arch = "wasm32")]
    static WASM_NOW_MICROS: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);

    /// Tell the wasm build what time it is (`Date.now() * 1000`).
    ///
    /// An atomic rather than a parameter because the call site is seven levels
    /// down inside queries whose signatures are the parity contract — widening
    /// them to carry a clock would change the native surface to serve the
    /// browser, which is exactly backwards. Native builds do not have this
    /// function at all, so nothing on the gated path can call it.
    #[cfg(target_arch = "wasm32")]
    pub fn set_now_micros(micros: i64) {
        WASM_NOW_MICROS.store(micros, core::sync::atomic::Ordering::Relaxed);
    }

    /// `datetime.fromisoformat(...).isoformat()` for an aware UTC datetime.
    ///
    /// Microseconds are omitted when zero, exactly as `isoformat()` does.
    #[must_use]
    pub fn isoformat_utc(micros: i64) -> String {
        let seconds = micros.div_euclid(1_000_000);
        let fraction = micros.rem_euclid(1_000_000);
        let (year, month, day, hour, minute, second) = civil_from_epoch(seconds);
        let mut out = String::with_capacity(32);
        let _ = write!(
            out,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
        );
        if fraction != 0 {
            let _ = write!(out, ".{fraction:06}");
        }
        out.push_str("+00:00");
        out
    }

    /// Split epoch seconds into a civil UTC date-time (Hinnant's algorithm).
    fn civil_from_epoch(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
        let days = seconds.div_euclid(86_400);
        let time_of_day = seconds.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
        let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
        let year = if month <= 2 { year + 1 } else { year };
        (
            year,
            month,
            day,
            u32::try_from(time_of_day / 3_600).unwrap_or(0),
            u32::try_from((time_of_day % 3_600) / 60).unwrap_or(0),
            u32::try_from(time_of_day % 60).unwrap_or(0),
        )
    }

    /// Days since the epoch for a civil UTC date (Hinnant's algorithm).
    fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = year.div_euclid(400);
        let yoe = year - era * 400;
        let mp = if month > 2 { month - 3 } else { month + 9 };
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// `datetime.fromisoformat` — enough of it for stored timestamps.
    ///
    /// Accepts `YYYY-MM-DD`, an optional `T` or space separator, `HH:MM[:SS]`,
    /// an optional fraction, and an optional `Z` / `±HH:MM[:SS]` offset. A
    /// naive value is read as UTC, which is what `_parse_ts` does. `None` when
    /// the string is not a datetime — the reference swallows the `ValueError`.
    #[must_use]
    pub fn parse_iso(value: &str) -> Option<Epoch> {
        let text = value.trim();
        if text.len() < 10 {
            return None;
        }
        let bytes: Vec<char> = text.chars().collect();
        let year: i64 = text.get(0..4)?.parse().ok()?;
        if bytes.get(4) != Some(&'-') {
            return None;
        }
        let month: i64 = text.get(5..7)?.parse().ok()?;
        if bytes.get(7) != Some(&'-') {
            return None;
        }
        let day: i64 = text.get(8..10)?.parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let mut epoch = (days_from_civil(year, month, day) * 86_400) as Epoch;
        if bytes.len() == 10 {
            return Some(epoch);
        }
        if !matches!(bytes.get(10), Some('T' | 't' | ' ')) {
            return None;
        }
        let rest = &text[11..];
        // Split the offset off the time part.
        let (time_part, offset) = split_offset(rest)?;
        let mut fields = time_part.split(':');
        let hour: i64 = fields.next()?.parse().ok()?;
        let minute: i64 = fields.next().unwrap_or("0").parse().ok()?;
        let second_field = fields.next().unwrap_or("0");
        let (second, fraction) = match second_field.split_once('.') {
            Some((whole, frac)) => {
                let digits: String = frac.chars().take(6).collect();
                let scale = 10f64.powi(6 - i32::try_from(digits.len()).unwrap_or(6));
                let micros: f64 = digits.parse::<f64>().ok()? * scale;
                (whole.parse::<i64>().ok()?, micros / 1_000_000.0)
            }
            None => (second_field.parse::<i64>().ok()?, 0.0),
        };
        epoch += (hour * 3_600 + minute * 60 + second) as Epoch + fraction;
        Some(epoch - offset.unwrap_or(0.0))
    }

    /// Peel a trailing `Z` / `±HH:MM[:SS]` off a time string, in seconds east.
    ///
    /// The offset is `None` for a naive value and `Some(0.0)` for an explicit
    /// UTC one — a distinction `parse_since` needs, because Python re-renders a
    /// naive value with `+00:00` but keeps whatever an aware value carried.
    fn split_offset(rest: &str) -> Option<(&str, Option<Epoch>)> {
        if let Some(stripped) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
            return Some((stripped, Some(0.0)));
        }
        for (index, ch) in rest.char_indices() {
            if index == 0 {
                continue;
            }
            if ch == '+' || ch == '-' {
                let (time_part, offset_part) = rest.split_at(index);
                let sign = if ch == '-' { -1.0 } else { 1.0 };
                let magnitude = offset_magnitude_seconds(&offset_part[1..])?;
                return Some((time_part, Some(sign * magnitude)));
            }
        }
        Some((rest, None))
    }

    /// The unsigned size of an ISO offset, in seconds.
    ///
    /// `fromisoformat` takes `HH`, `HHMM`, `HH:MM`, `HHMMSS`, `HH:MM:SS` and any
    /// of those plus `.ffffff`. The colon-less forms are not decoration: reading
    /// `+0530` field-by-field on `:` gives *530 hours*, which is how a
    /// `--since 2026-07-30T00:00:00+0530` used to land in the year 2026-08-21.
    fn offset_magnitude_seconds(body: &str) -> Option<Epoch> {
        let (whole, fraction) = match body.split_once('.') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (body, None),
        };
        let compact: String = whole.chars().filter(|ch| *ch != ':').collect();
        if !compact.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }
        let field = |start: usize| compact.get(start..start + 2)?.parse::<f64>().ok();
        let (hours, minutes, seconds) = match compact.len() {
            2 => (field(0)?, 0.0, 0.0),
            4 => (field(0)?, field(2)?, 0.0),
            6 => (field(0)?, field(2)?, field(4)?),
            _ => return None,
        };
        let micros = match fraction {
            Some(digits) => {
                let taken: String = digits.chars().take(6).collect();
                if taken.is_empty() || !taken.chars().all(|ch| ch.is_ascii_digit()) {
                    return None;
                }
                let scale = 10f64.powi(6 - i32::try_from(taken.len()).unwrap_or(6));
                taken.parse::<f64>().ok()? * scale / 1_000_000.0
            }
            None => 0.0,
        };
        Some(hours * 3_600.0 + minutes * 60.0 + seconds + micros)
    }

    /// `discovery.parse_since` with the clock injected.
    ///
    /// # Errors
    /// On an unrecognised string — the `ValueError` the CLI turns into either a
    /// `--since` parameter error or a JSON error envelope.
    pub fn parse_since_at(since: Option<&str>, now_micros: i64) -> Result<Option<String>> {
        let Some(raw) = since else {
            return Ok(None);
        };
        let text = raw.trim();
        if text.is_empty() {
            return Ok(None);
        }
        if let Some((count, unit)) = parse_relative(text) {
            // `timedelta` and `datetime` are bounded (years 1–9999); Python
            // raises `OverflowError` past that, which is NOT a `ValueError`, so
            // the CLI does not catch it. A plain error here reproduces that:
            // exit 1, empty stdout, never the `--since` parameter error.
            let delta_micros = count
                .checked_mul(match unit {
                    'h' => 3_600 * 1_000_000,
                    'd' => 86_400 * 1_000_000,
                    'w' => 7 * 86_400 * 1_000_000,
                    // "1m" is 30 days, not a calendar month — documented in the
                    // CLI help and ported as-is.
                    _ => 30 * 86_400 * 1_000_000,
                })
                .and_then(|delta| now_micros.checked_sub(delta))
                .filter(|micros| in_datetime_range(*micros));
            let Some(micros) = delta_micros else {
                bail!("date value out of range");
            };
            return Ok(Some(isoformat_utc(micros)));
        }
        match parse_iso_for_since(text) {
            Some(rendered) => Ok(Some(rendered)),
            None => Err(anyhow::Error::new(super::ValueError(format!(
                "Invalid since value {}: expected '7d'/'1w'/'1m'/'24h' or an ISO date/datetime.",
                super::paths::py_repr(text)
            )))),
        }
    }

    /// `datetime.MINYEAR`–`datetime.MAXYEAR` as epoch microseconds.
    fn in_datetime_range(micros: i64) -> bool {
        const MIN: i64 = -62_135_596_800_000_000; // 0001-01-01T00:00:00Z
        const MAX: i64 = 253_402_300_799_999_999; // 9999-12-31T23:59:59.999999Z
        (MIN..=MAX).contains(&micros)
    }

    /// `discovery.parse_since` against the real clock.
    ///
    /// # Errors
    /// As [`parse_since_at`].
    pub fn parse_since(since: Option<&str>) -> Result<Option<String>> {
        parse_since_at(since, now_micros())
    }

    /// `^\s*(\d+)\s*([dwmh])\s*$` — the relative-window form.
    ///
    /// `\d` and `\s` are Python's, not ASCII's: `re` matches every Unicode
    /// decimal digit, so `--since '٧d'` is seven days and `int('٧')` is 7. The
    /// whitespace class is `str.isspace()`'s, which is *wider* than the one
    /// `int()` strips (see [`super::pyint::is_regex_space`]).
    fn parse_relative(text: &str) -> Option<(i64, char)> {
        let trimmed = text.trim_matches(super::pyint::is_regex_space);
        let mut digits = String::new();
        let mut chars = trimmed.chars().peekable();
        while let Some(value) = chars.peek().copied().and_then(super::pyint::decimal_value) {
            digits.push(char::from_digit(value, 10)?);
            chars.next();
        }
        if digits.is_empty() {
            return None;
        }
        while chars
            .peek()
            .copied()
            .is_some_and(super::pyint::is_regex_space)
        {
            chars.next();
        }
        let unit = chars.next()?.to_ascii_lowercase();
        if !matches!(unit, 'd' | 'w' | 'm' | 'h') {
            return None;
        }
        if chars.any(|ch| !super::pyint::is_regex_space(ch)) {
            return None;
        }
        // `int(...)` is unbounded; a count past `i64` cannot survive the
        // `timedelta` multiply either, so `None` here lands on the same
        // out-of-range error the caller raises.
        digits.parse().ok().map(|count| (count, unit))
    }

    /// The ISO leg of `parse_since`: parse, default to UTC, re-render.
    ///
    /// **The offset is preserved, not normalised.** `datetime.fromisoformat`
    /// keeps whatever `tzinfo` the literal carried and `isoformat()` prints it
    /// back, so `--since 2026-07-30T00:00:00+05:30` stays that string — and the
    /// SQL comparison is a *string* comparison against `messages.timestamp`, so
    /// re-rendering it in UTC (`2026-07-29T18:30:00+00:00`) silently selects a
    /// different set of rows. Ledger row B-8, upgraded from cosmetic to
    /// result-set-changing and ported bug-for-bug.
    fn parse_iso_for_since(text: &str) -> Option<String> {
        let epoch = parse_iso(text)?;
        let micros = (epoch * 1_000_000.0).round() as i64;
        let Some(offset) = iso_offset_micros(text) else {
            // `parsed.tzinfo is None` → `replace(tzinfo=UTC)` → `+00:00`.
            return Some(isoformat_utc(micros));
        };
        // `isoformat()` prints the wall-clock fields as written, then the
        // offset; `micros` is the UTC instant, so add the offset back to
        // recover the literal's own fields.
        let local = isoformat_utc(micros.checked_add(offset)?);
        let naive = local.strip_suffix("+00:00")?;
        Some(format!("{naive}{}", render_offset(offset)))
    }

    /// The trailing `Z` / `±HH:MM[:SS[.ffffff]]` of an ISO string, in
    /// microseconds east. `None` when the value is naive.
    fn iso_offset_micros(text: &str) -> Option<i64> {
        let text = text.trim();
        if text.len() <= 11 {
            return None; // a bare date carries no offset
        }
        let (_, offset) = split_offset(&text[11..])?;
        Some((offset? * 1_000_000.0).round() as i64)
    }

    /// `timezone(timedelta(...)).__str__()`'s suffix — what `isoformat()` prints.
    ///
    /// `+HH:MM`, widening to `:SS` and `.ffffff` only when those fields are
    /// non-zero, and always `+00:00` for UTC (Python normalises `-00:00`).
    fn render_offset(micros: i64) -> String {
        let sign = if micros < 0 { '-' } else { '+' };
        let magnitude = micros.unsigned_abs();
        let fraction = magnitude % 1_000_000;
        let seconds = magnitude / 1_000_000;
        let mut out = String::with_capacity(16);
        let _ = write!(
            out,
            "{sign}{:02}:{:02}",
            seconds / 3_600,
            (seconds % 3_600) / 60
        );
        if !seconds.is_multiple_of(60) || fraction != 0 {
            let _ = write!(out, ":{:02}", seconds % 60);
        }
        if fraction != 0 {
            let _ = write!(out, ".{fraction:06}");
        }
        out
    }
}

// ── path arithmetic ──────────────────────────────────────────────────────────

/// `pathlib` and the discovery path helpers, ported as string arithmetic.
pub mod paths {
    use std::path::{Component, Path, PathBuf};

    /// `discovery.decode_slug_to_path` — the lossy slug → path reconstruction.
    #[must_use]
    pub fn decode_slug_to_path(slug: &str) -> String {
        if slug.is_empty() || !slug.starts_with('-') {
            return String::new();
        }
        format!("/{}", slug.trim_start_matches('-').replace('-', "/"))
    }

    /// `discovery._project_fs_path` — stored path wins, else decode the slug.
    #[must_use]
    pub fn project_fs_path(stored_path: Option<&str>, slug: &str) -> String {
        match stored_path.filter(|value| !value.is_empty()) {
            Some(path) => path.to_string(),
            None => decode_slug_to_path(slug),
        }
    }

    /// `discovery._normalize_path`.
    #[must_use]
    pub fn normalize_path(path: &str) -> String {
        if path.is_empty() {
            return String::new();
        }
        path.replace('\\', "/").trim_end_matches('/').to_string()
    }

    /// `discovery._is_ancestor`.
    #[must_use]
    pub fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
        if ancestor.is_empty() || descendant.is_empty() {
            return false;
        }
        let ancestor = normalize_path(ancestor);
        let descendant = normalize_path(descendant);
        ancestor == descendant || descendant.starts_with(&format!("{ancestor}/"))
    }

    /// `Path.expanduser()` — a leading `~` only.
    ///
    /// `~other` is left literal (RS-1-034, disposition `fixed-in-rust`), which
    /// matches `settings::resolve_app_dir`.
    #[must_use]
    pub fn expanduser(path: &str, home: Option<&Path>) -> String {
        let Some(home) = home else {
            return path.to_string();
        };
        if path == "~" {
            return path_to_string(home);
        }
        match path.strip_prefix("~/") {
            Some(rest) => path_to_string(&home.join(rest)),
            None => path.to_string(),
        }
    }

    /// The user's home directory.
    #[must_use]
    pub fn home_dir() -> Option<PathBuf> {
        #[allow(
            deprecated,
            reason = "std::env::home_dir is the platform-correct answer on the \
            1.97.1 pin; the 2018-era deprecation is scheduled for removal upstream"
        )]
        std::env::home_dir()
    }

    /// `str(PurePosixPath(p))` — the normalisation `Path()` applies on
    /// construction: `.` components and duplicate/trailing separators go, `..`
    /// stays, and an empty path becomes `.`.
    #[must_use]
    pub fn purepath_str(path: &str) -> String {
        if path.is_empty() {
            return ".".to_string();
        }
        let absolute = path.starts_with('/');
        let parts: Vec<&str> = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        if parts.is_empty() {
            return if absolute { "/".into() } else { ".".into() };
        }
        let joined = parts.join("/");
        if absolute {
            format!("/{joined}")
        } else {
            joined
        }
    }

    /// A `Path` rendered the way Python renders one.
    #[must_use]
    pub fn path_to_string(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// `Path(p).expanduser().resolve(strict=False)` — `discovery._resolve_input_path`.
    ///
    /// Symlinks are resolved for the longest existing prefix and the remainder
    /// is appended lexically, which is what `resolve(strict=False)` does.
    #[must_use]
    pub fn resolve_input_path_with(path: &str, home: Option<&Path>, cwd: &Path) -> String {
        let expanded = expanduser(path, home);
        let candidate = if expanded.is_empty() {
            cwd.to_path_buf()
        } else {
            let as_path = PathBuf::from(&expanded);
            if as_path.is_absolute() {
                as_path
            } else {
                cwd.join(as_path)
            }
        };
        let lexical = lexically_normalize(&candidate);
        // Resolve symlinks over the longest existing prefix.
        let mut prefix = lexical.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if let Ok(resolved) = std::fs::canonicalize(&prefix) {
                let mut out = resolved;
                for part in tail.iter().rev() {
                    out.push(part);
                }
                return path_to_string(&out);
            }
            let Some(name) = prefix.file_name().map(std::ffi::OsStr::to_os_string) else {
                break;
            };
            tail.push(name);
            if !prefix.pop() {
                break;
            }
        }
        path_to_string(&lexical)
    }

    /// [`resolve_input_path_with`] against the process's home and cwd.
    #[must_use]
    pub fn resolve_input_path(path: &str) -> String {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        resolve_input_path_with(path, home_dir().as_deref(), &cwd)
    }

    /// Collapse `.` and `..` without touching the filesystem.
    fn lexically_normalize(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !out.pop() {
                        out.push("..");
                    }
                }
                other => out.push(other.as_os_str()),
            }
        }
        if out.as_os_str().is_empty() {
            out.push(".");
        }
        out
    }

    /// `repr(str)` — the quoting the `memory` titles interpolate with `{!r}`.
    ///
    /// Python prefers single quotes and switches to double quotes only when the
    /// string contains a `'` and no `"`.
    #[must_use]
    pub fn py_repr(text: &str) -> String {
        let quote = if text.contains('\'') && !text.contains('"') {
            '"'
        } else {
            '\''
        };
        let mut out = String::with_capacity(text.len() + 2);
        out.push(quote);
        for ch in text.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                ch if ch == quote => {
                    out.push('\\');
                    out.push(ch);
                }
                ch if (ch as u32) < 0x20 || ch as u32 == 0x7f => {
                    out.push_str(&format!("\\x{:02x}", ch as u32));
                }
                ch => out.push(ch),
            }
        }
        out.push(quote);
        out
    }
}

// ── the match shapes ─────────────────────────────────────────────────────────

/// The four outcome fields `OutcomeMatch` adds to a [`SessionMatch`].
///
/// Kept as a nested struct rather than a second row type because the Python
/// dataclass inherits: `asdict()` emits the base fields first, then these four,
/// and [`SessionMatch::to_dict`] reproduces exactly that order.
#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeFields {
    /// `"worked"` | `"failed"` | `"reverted"` | `"uncertain"`.
    pub outcome: String,
    /// Short human-readable justification, with the message excerpt.
    pub outcome_evidence: String,
    /// `messages.id` of the row that established the outcome.
    pub outcome_msg_id: i64,
    /// Confidence in `[0.0, 1.0]` — see `_classify_outcome`'s ladder.
    pub outcome_confidence: f64,
}

/// A session that matched a discovery query.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMatch {
    /// Provider-facing session id.
    pub session_id: String,
    /// `projects.slug`.
    pub project_slug: String,
    /// `projects.path` when populated, else the slug decoded back to a path.
    pub project_path: String,
    /// `projects.provider`.
    pub provider: String,
    /// `sessions.first_ts`, `""` when null.
    pub first_ts: String,
    /// `sessions.last_ts`, `""` when null.
    pub last_ts: String,
    /// `sessions.message_count`.
    pub message_count: i64,
    /// `session_mart.cost_usd`, `0.0` when the mart has no row.
    pub cost_usd: f64,
    /// Content excerpt — only `search_past_decisions` populates it.
    pub snippet: Option<String>,
    /// Cosine similarity — only the `use_embeddings` path populates it, which
    /// no wave-1 command uses; kept so the `to_dict` shape can grow without a
    /// schema bump.
    pub embedding_score: Option<f64>,
    /// Further matching messages in the same session, when > 0.
    pub more_matches_in_session: Option<i64>,
    /// Present on `OutcomeMatch` rows only.
    pub outcome: Option<OutcomeFields>,
}

impl SessionMatch {
    /// Serialise exactly as `SessionMatch.to_dict()` / `asdict(OutcomeMatch)`.
    ///
    /// `embedding_score` and `more_matches_in_session` are dropped when unset,
    /// so single-hit substring-mode rows keep the original 9-key shape.
    #[must_use]
    pub fn to_dict(&self) -> pyjson::Value {
        let mut out: Vec<(String, pyjson::Value)> = Vec::with_capacity(15);
        out.push(("session_id".into(), pyjson::Value::from(&self.session_id)));
        out.push((
            "project_slug".into(),
            pyjson::Value::from(&self.project_slug),
        ));
        out.push((
            "project_path".into(),
            pyjson::Value::from(&self.project_path),
        ));
        out.push(("provider".into(), pyjson::Value::from(&self.provider)));
        out.push(("first_ts".into(), pyjson::Value::from(&self.first_ts)));
        out.push(("last_ts".into(), pyjson::Value::from(&self.last_ts)));
        out.push((
            "message_count".into(),
            pyjson::Value::Int(self.message_count),
        ));
        out.push(("cost_usd".into(), pyjson::Value::Float(self.cost_usd)));
        out.push((
            "snippet".into(),
            match &self.snippet {
                Some(text) => pyjson::Value::from(text),
                None => pyjson::Value::Null,
            },
        ));
        if let Some(score) = self.embedding_score {
            out.push(("embedding_score".into(), pyjson::Value::Float(score)));
        }
        if let Some(more) = self.more_matches_in_session {
            out.push(("more_matches_in_session".into(), pyjson::Value::Int(more)));
        }
        if let Some(outcome) = &self.outcome {
            out.push(("outcome".into(), pyjson::Value::from(&outcome.outcome)));
            out.push((
                "outcome_evidence".into(),
                pyjson::Value::from(&outcome.outcome_evidence),
            ));
            out.push((
                "outcome_msg_id".into(),
                pyjson::Value::Int(outcome.outcome_msg_id),
            ));
            out.push((
                "outcome_confidence".into(),
                pyjson::Value::Float(outcome.outcome_confidence),
            ));
        }
        pyjson::Value::Object(out)
    }
}

/// Outcome of running a discovery query with a token budget applied.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetedResult {
    /// Rank-ordered, budget-packed rows.
    pub sessions: Vec<SessionMatch>,
    /// At least one matched session was dropped to fit.
    pub truncated: bool,
    /// How many were dropped.
    pub more_available: usize,
    /// Σ of the chars/4 estimate over the kept rows.
    pub budget_used_tokens: i64,
    /// The budget that was enforced (`<= 0` means "no enforcement").
    pub budget_max_tokens: i64,
}

// ── ranking + budget packing ─────────────────────────────────────────────────

/// The rank-and-pack machinery behind `--context-budget`.
///
/// A rank is a weighted sum of three terms in `[0, 1]` — recency, cost, and a
/// command-specific relevance — and the packer is greedy and strict: the first
/// row that does not fit ends the pack (it does not skip ahead to a smaller
/// one, which would reorder by size and defeat the ranking).
pub mod rank {
    use std::collections::HashMap;

    use super::{BudgetedResult, SessionMatch, pyjson, pytime};

    /// `discovery._DEFAULT_RANK_WEIGHTS` — recency, cost, relevance.
    pub const DEFAULT_RANK_WEIGHTS: (f64, f64, f64) = (0.5, 0.2, 0.3);
    /// `discovery._COST_SATURATION_USD`.
    pub const COST_SATURATION_USD: f64 = 5.0;
    /// `discovery._DECISIONS_OCCURRENCE_SATURATION`.
    pub const DECISIONS_OCCURRENCE_SATURATION: f64 = 5.0;

    /// A command-specific relevance term.
    pub type Relevance = Box<dyn Fn(&SessionMatch) -> f64>;

    /// `discovery._estimate_tokens` — `len(compact json) // 4 + 1`.
    ///
    /// Python measures the *string* length in characters; with `ensure_ascii`
    /// on, every character is ASCII, so bytes and characters agree.
    #[must_use]
    pub fn estimate_tokens(value: &pyjson::Value) -> i64 {
        let serialized = pyjson::dumps_compact(value);
        i64::try_from(serialized.chars().count() / 4).unwrap_or(i64::MAX) + 1
    }

    /// `discovery._parse_rank_weights` — lenient, defaults on anything odd.
    #[must_use]
    pub fn parse_rank_weights(raw: Option<&str>) -> (f64, f64, f64) {
        let Some(raw) = raw else {
            return DEFAULT_RANK_WEIGHTS;
        };
        let parts: Vec<&str> = raw
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        let mut values: Vec<f64> = Vec::with_capacity(parts.len());
        for part in parts {
            match parse_python_float(part) {
                Some(value) => values.push(value),
                None => return DEFAULT_RANK_WEIGHTS,
            }
        }
        if values.len() < 3 || values[..3].iter().any(|value| *value < 0.0) {
            return DEFAULT_RANK_WEIGHTS;
        }
        (values[0], values[1], values[2])
    }

    /// `float(str)` — Rust's parser plus Python's acceptance of `inf`/`nan`.
    fn parse_python_float(text: &str) -> Option<f64> {
        text.parse::<f64>().ok()
    }

    /// `discovery._recency_score` — `1 / (1 + days_since_last_ts)`.
    #[must_use]
    pub fn recency_score(session_match: &SessionMatch, now_epoch: f64) -> f64 {
        let Some(timestamp) = pytime::parse_iso(&session_match.last_ts) else {
            return 0.0;
        };
        let days = ((now_epoch - timestamp) / 86_400.0).max(0.0);
        1.0 / (1.0 + days)
    }

    /// `discovery._cost_score` — `min(1.0, cost_usd / 5.0)`.
    #[must_use]
    pub fn cost_score(session_match: &SessionMatch) -> f64 {
        (session_match.cost_usd.max(0.0) / COST_SATURATION_USD).min(1.0)
    }

    /// `discovery._relevance_in_path`.
    #[must_use]
    pub fn relevance_in_path(resolved: &str) -> Relevance {
        let target = resolved.trim_end_matches('/').to_string();
        Box::new(move |session_match: &SessionMatch| {
            let project = session_match.project_path.trim_end_matches('/');
            if project.is_empty() {
                return 0.0;
            }
            if project == target {
                return 1.0;
            }
            if target.starts_with(&format!("{project}/")) {
                return 0.5;
            }
            if project.starts_with(&format!("{target}/")) {
                return 0.25;
            }
            0.0
        })
    }

    /// `discovery._relevance_touching_file` without the bm25 leg (that leg only
    /// exists on the FTS path, which is `stax-memory`'s).
    #[must_use]
    pub fn relevance_touching_file(match_kind_by_sid: HashMap<String, &'static str>) -> Relevance {
        Box::new(move |session_match: &SessionMatch| {
            match match_kind_by_sid
                .get(&session_match.session_id)
                .copied()
                .unwrap_or("")
            {
                "tool" => 1.0,
                "content" => 0.5,
                _ => 0.25,
            }
        })
    }

    /// `discovery._relevance_decisions` — LIKE-match density, saturating at 5.
    #[must_use]
    pub fn relevance_decisions(occurrences_by_sid: HashMap<String, i64>) -> Relevance {
        Box::new(move |session_match: &SessionMatch| {
            let occurrences = occurrences_by_sid
                .get(&session_match.session_id)
                .copied()
                .unwrap_or(0);
            (occurrences as f64 / DECISIONS_OCCURRENCE_SATURATION).min(1.0)
        })
    }

    /// `discovery._relevance_embeddings` — the cosine already on the match.
    ///
    /// `0.0` for a row that has no score (Ollama down, an unembeddable message,
    /// an orphan row). Those rows sink to the bottom of the rank but stay in the
    /// surface, which is why `--use-embeddings` with no daemon still answers.
    #[must_use]
    pub fn relevance_embeddings() -> Relevance {
        Box::new(|session_match: &SessionMatch| {
            session_match
                .embedding_score
                .map_or(0.0, |score| score.clamp(0.0, 1.0))
        })
    }

    /// `discovery._build_rank_fn` with the clock and the weights injected.
    #[must_use]
    pub fn rank_of(
        session_match: &SessionMatch,
        relevance: &Relevance,
        weights: (f64, f64, f64),
        now_epoch: f64,
    ) -> f64 {
        let (w_recency, w_cost, w_relevance) = weights;
        w_recency * recency_score(session_match, now_epoch)
            + w_cost * cost_score(session_match)
            + w_relevance * relevance(session_match)
    }

    /// `discovery.pack_within_budget` — `(kept, dropped, used)`.
    ///
    /// `rank` of `None` keeps the input order (the caller already sorted).
    #[must_use]
    pub fn pack_within_budget(
        sessions: Vec<SessionMatch>,
        budget_tokens: i64,
        rank: Option<(&Relevance, (f64, f64, f64), f64)>,
    ) -> (Vec<SessionMatch>, usize, i64) {
        let mut ordered = sessions;
        if let Some((relevance, weights, now_epoch)) = rank {
            // Python's `sorted(..., reverse=True)` is stable: equal ranks keep
            // their input order, which for these queries is `last_ts DESC`.
            let mut keyed: Vec<(f64, SessionMatch)> = ordered
                .into_iter()
                .map(|session_match| {
                    (
                        rank_of(&session_match, relevance, weights, now_epoch),
                        session_match,
                    )
                })
                .collect();
            keyed.sort_by(|left, right| right.0.total_cmp(&left.0));
            ordered = keyed.into_iter().map(|(_, value)| value).collect();
        }

        if budget_tokens <= 0 {
            let used = ordered
                .iter()
                .map(|session_match| estimate_tokens(&session_match.to_dict()))
                .sum();
            return (ordered, 0, used);
        }

        let total = ordered.len();
        let mut kept: Vec<SessionMatch> = Vec::new();
        let mut used: i64 = 0;
        for session_match in ordered {
            let cost = estimate_tokens(&session_match.to_dict());
            if used + cost > budget_tokens {
                break;
            }
            used += cost;
            kept.push(session_match);
        }
        let dropped = total - kept.len();
        (kept, dropped, used)
    }

    /// Everything the budget path needs, injected rather than read.
    ///
    /// The clock lives here because `_recency_score` calls `datetime.now(UTC)`
    /// and the weights because they come from a setting
    /// (`STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS`); reading either from ambient
    /// state would make the rank untestable and, for the env var, would need
    /// the `unsafe` `set_var` this workspace forbids (finding 5).
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Budget {
        /// The `--context-budget` in estimated tokens; `<= 0` disables packing.
        pub tokens: i64,
        /// `recency, cost, relevance`.
        pub weights: (f64, f64, f64),
        /// `datetime.now(UTC)` as epoch seconds.
        pub now_epoch: f64,
    }

    impl Budget {
        /// A budget with the default weights and the real clock.
        #[must_use]
        pub fn new(tokens: i64) -> Self {
            Self {
                tokens,
                weights: DEFAULT_RANK_WEIGHTS,
                now_epoch: now_epoch(),
            }
        }

        /// A fully injected budget — what tests and the parity harness use.
        #[must_use]
        pub fn at(tokens: i64, weights: (f64, f64, f64), now_epoch: f64) -> Self {
            Self {
                tokens,
                weights,
                now_epoch,
            }
        }
    }

    /// `discovery._budgeted` — pack and wrap.
    #[must_use]
    pub fn budgeted(
        matches: Vec<SessionMatch>,
        budget: &Budget,
        relevance: &Relevance,
    ) -> BudgetedResult {
        let (kept, dropped, used) = pack_within_budget(
            matches,
            budget.tokens,
            Some((relevance, budget.weights, budget.now_epoch)),
        );
        BudgetedResult {
            sessions: kept,
            truncated: dropped > 0,
            more_available: dropped,
            budget_used_tokens: used,
            budget_max_tokens: budget.tokens,
        }
    }

    /// `datetime.now(UTC)` as epoch seconds.
    #[must_use]
    pub fn now_epoch() -> f64 {
        pytime::now_micros() as f64 / 1_000_000.0
    }
}

// ── outcome inference ────────────────────────────────────────────────────────

/// The outcome heuristic — "did the thing work?" — read off the transcript.
///
/// Ported keyword-for-keyword from `discovery.py`'s `OUTCOME_KEYWORDS` +
/// `_classify_outcome`. The Python side compiles one alternation regex per class
/// (longest phrase first, `\b` on the alphanumeric ends); this scans the same
/// alternatives at each position in the same order, which is what the regex
/// engine does, without pulling in a regex dependency.
pub mod outcome {
    use std::collections::HashMap;
    use std::sync::OnceLock;

    use anyhow::Result;
    use rusqlite::Connection;

    use super::{OutcomeFields, SessionMatch, pyjson, row_to_match};

    /// `discovery.DEFAULT_MIN_OUTCOME_CONFIDENCE`.
    pub const DEFAULT_MIN_OUTCOME_CONFIDENCE: f64 = 0.5;
    /// In-vocabulary phrase from a user turn.
    pub const CONF_EXPLICIT: f64 = 0.8;
    /// The agent ran a revert command.
    pub const CONF_TOOL_REVERT: f64 = 0.5;
    /// "No complaint before the session ended".
    pub const CONF_SILENCE: f64 = 0.3;
    /// No signal at all.
    pub const CONF_NONE: f64 = 0.0;
    /// `discovery._OUTCOME_LOOKAHEAD`.
    pub const LOOKAHEAD: usize = 5;

    /// Which tool family a file mention has to come from.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        /// `Read`.
        Read,
        /// `Edit` / `Write` / `MultiEdit` / `NotebookEdit`.
        Write,
        /// Either of the above.
        Any,
    }

    const READ_TOOL_NAMES: &[&str] = &["Read"];
    const WRITE_TOOL_NAMES: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
    const SHELL_TOOL_NAMES: &[&str] = &[
        "Bash",
        "shell",
        "run_command",
        "execute_command",
        "RunCommand",
    ];
    const REVERT_COMMAND_PATTERNS: &[&str] = &[
        "git revert",
        "git reset --hard",
        "git reset --merge",
        "git reset head",
        "git checkout --",
        "git checkout .",
        "git restore ",
        "git stash",
    ];
    const BENIGN_NO_SUFFIXES: &[&str] = &[
        "problem", "problems", "worries", "worry", "rush", "biggie", "prob", "issue", "issues",
        "need", "need to", "doubt",
    ];

    const REVERT_KEYWORDS: &[&str] = &[
        "undo",
        "undo that",
        "undo it",
        "revert",
        "revert that",
        "revert it",
        "roll back",
        "rollback",
        "roll that back",
        "take that back",
        "back it out",
        "back that out",
        "try again",
        "try a different",
        "git revert",
        "git reset --hard",
        "git checkout --",
    ];
    const NEGATIVE_KEYWORDS: &[&str] = &[
        "no",
        "nope",
        "that broke",
        "you broke",
        "broke it",
        "broke the build",
        "broke the tests",
        "broke",
        "broken",
        "still broken",
        "still failing",
        "still fails",
        "still errors",
        "doesn't work",
        "does not work",
        "didn't work",
        "did not work",
        "not working",
        "isn't working",
        "won't work",
        "wont work",
        "stopped working",
        "failing",
        "tests fail",
        "test failed",
        "build failed",
        "wrong",
        "that's wrong",
        "thats wrong",
        "incorrect",
        "mistake",
        "error",
        "not what i asked",
        "not what i wanted",
        "not what i meant",
        "that's not right",
        "thats not right",
        "that's not it",
        "thats not it",
        "no good",
        "doesn't help",
        "didn't help",
        "regression",
        "regressed",
        "❌",
        "👎",
    ];
    const POSITIVE_KEYWORDS: &[&str] = &[
        "thanks",
        "thank you",
        "thx",
        "ty",
        "that worked",
        "it worked",
        "works now",
        "working now",
        "that works",
        "it works",
        "works great",
        "works perfectly",
        "tests pass",
        "tests passed",
        "tests passing",
        "passes",
        "fixed",
        "solved",
        "perfect",
        "nice",
        "great",
        "awesome",
        "excellent",
        "ship it",
        "lgtm",
        "looks good",
        "looks great",
        "love it",
        "exactly right",
        "that's it",
        "thats it",
        "nailed it",
        "correct",
        "+1",
        "👍",
        "🎉",
        "✅",
        "✓",
    ];

    /// One keyword hit: where it started and what matched.
    struct KeywordHit {
        start: usize,
        end: usize,
        text: String,
    }

    /// The phrases of one class, deduped and sorted longest-first, as chars.
    ///
    /// Built once per process: `_compile_keyword_re` compiles its alternation
    /// once at import too, and rebuilding this per message turned
    /// `memory worked` into the slowest verb in the port (885ms → 250ms once
    /// the table stopped being rebuilt inside the scan loop).
    fn phrase_table(keywords: &[&'static str]) -> Vec<Vec<char>> {
        let mut deduped: Vec<&str> = Vec::with_capacity(keywords.len());
        for keyword in keywords {
            let trimmed = keyword.trim();
            if !trimmed.is_empty() && !deduped.contains(&trimmed) {
                deduped.push(trimmed);
            }
        }
        deduped.sort_by_key(|phrase| std::cmp::Reverse(phrase.chars().count()));
        deduped
            .into_iter()
            .map(|phrase| phrase.chars().collect())
            .collect()
    }

    fn revert_phrases() -> &'static [Vec<char>] {
        static TABLE: OnceLock<Vec<Vec<char>>> = OnceLock::new();
        TABLE.get_or_init(|| phrase_table(REVERT_KEYWORDS))
    }

    fn negative_phrases() -> &'static [Vec<char>] {
        static TABLE: OnceLock<Vec<Vec<char>>> = OnceLock::new();
        TABLE.get_or_init(|| phrase_table(NEGATIVE_KEYWORDS))
    }

    fn positive_phrases() -> &'static [Vec<char>] {
        static TABLE: OnceLock<Vec<Vec<char>>> = OnceLock::new();
        TABLE.get_or_init(|| phrase_table(POSITIVE_KEYWORDS))
    }

    /// Scan `text` for a class's phrases, longest-first at each position.
    ///
    /// Mirrors `re.finditer` over `_compile_keyword_re(words)`: alternatives are
    /// ordered longest-first, matching is case-insensitive, and a phrase whose
    /// first/last character is alphanumeric needs a `\b` on that side.
    fn find_keywords(text: &str, phrases: &[Vec<char>], first_only: bool) -> Vec<KeywordHit> {
        let chars: Vec<char> = text.chars().collect();
        let lowered: Vec<char> = chars
            .iter()
            .map(|ch| ch.to_lowercase().next().unwrap_or(*ch))
            .collect();

        let mut hits: Vec<KeywordHit> = Vec::new();
        let mut position = 0usize;
        while position < lowered.len() {
            let head = lowered[position];
            let mut matched: Option<(usize, String)> = None;
            for phrase in phrases {
                if phrase[0] != head {
                    continue; // cheap prefilter; the regex engine's first-byte test
                }
                let end = position + phrase.len();
                if end > lowered.len() || lowered[position..end] != phrase[..] {
                    continue;
                }
                let left_ok = !is_word_char(phrase[0])
                    || position == 0
                    || !is_word_char(lowered[position - 1]);
                let right_ok = !is_word_char(phrase[phrase.len() - 1])
                    || end == lowered.len()
                    || !is_word_char(lowered[end]);
                if left_ok && right_ok {
                    matched = Some((end, chars[position..end].iter().collect()));
                    break;
                }
            }
            match matched {
                Some((end, text)) => {
                    hits.push(KeywordHit {
                        start: position,
                        end,
                        text,
                    });
                    if first_only {
                        return hits;
                    }
                    position = end;
                }
                None => position += 1,
            }
        }
        hits
    }

    /// Python's `\w`: alphanumeric (Unicode) or underscore.
    fn is_word_char(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    /// `discovery._classify_user_text` — revert > negative > positive.
    #[must_use]
    pub fn classify_user_text(text: &str) -> Option<&'static str> {
        if text.is_empty() {
            return None;
        }
        if !find_keywords(text, revert_phrases(), true).is_empty() {
            return Some("revert");
        }
        let chars: Vec<char> = text.chars().collect();
        for hit in find_keywords(text, negative_phrases(), false) {
            if hit.text.to_lowercase() == "no" {
                let tail: String = chars
                    .iter()
                    .skip(hit.end)
                    .take(16)
                    .collect::<String>()
                    .to_lowercase()
                    .trim_start_matches([' ', ',', '.', '!', ':', ';', '-'])
                    .to_string();
                if BENIGN_NO_SUFFIXES
                    .iter()
                    .any(|suffix| tail.starts_with(suffix))
                {
                    continue; // "no problem" / "no worries" — not a complaint
                }
            }
            let _ = hit.start;
            return Some("negative");
        }
        if !find_keywords(text, positive_phrases(), true).is_empty() {
            return Some("positive");
        }
        None
    }

    /// `discovery._trim_inline` — collapse whitespace, clip with an ellipsis.
    #[must_use]
    pub fn trim_inline(text: &str, limit: usize) -> String {
        let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if one_line.chars().count() <= limit {
            return one_line;
        }
        let keep = limit.saturating_sub(1).max(1);
        let clipped: String = one_line.chars().take(keep).collect();
        format!("{}…", clipped.trim_end())
    }

    /// `discovery._tools_json_mentions_file`.
    #[must_use]
    pub fn tools_json_mentions_file(tools_json: Option<&str>, file_path: &str, mode: Mode) -> bool {
        let Some(tools_json) = tools_json.filter(|raw| !raw.is_empty() && *raw != "[]") else {
            return false;
        };
        let Some(pyjson::Value::Array(tools)) = pyjson::loads(tools_json) else {
            return false;
        };
        let wanted: Vec<&str> = match mode {
            Mode::Read => READ_TOOL_NAMES.to_vec(),
            Mode::Write => WRITE_TOOL_NAMES.to_vec(),
            Mode::Any => READ_TOOL_NAMES
                .iter()
                .chain(WRITE_TOOL_NAMES.iter())
                .copied()
                .collect(),
        };
        for entry in &tools {
            if !matches!(entry, pyjson::Value::Object(_)) {
                continue;
            }
            let name = truthy_key(entry, "name")
                .or_else(|| truthy_key(entry, "tool"))
                .and_then(pyjson::Value::as_str)
                .unwrap_or("");
            if !wanted.contains(&name) {
                continue;
            }
            let candidate = truthy_key(entry, "input")
                .or_else(|| truthy_key(entry, "arguments"))
                .unwrap_or(entry);
            if matches!(candidate, pyjson::Value::Object(_)) {
                for key in ["file_path", "path", "filename", "notebook_path"] {
                    if let Some(value) = candidate.get(key).and_then(pyjson::Value::as_str)
                        && value.contains(file_path)
                    {
                        return true;
                    }
                }
            }
            if pyjson::dumps_default(entry).contains(file_path) {
                return true;
            }
        }
        false
    }

    /// `entry.get(key)` under Python truthiness — a falsy value is skipped.
    fn truthy_key<'a>(entry: &'a pyjson::Value, key: &str) -> Option<&'a pyjson::Value> {
        entry.get(key).filter(|value| value.is_truthy())
    }

    /// `discovery._revert_command_in_tools`.
    #[must_use]
    pub fn revert_command_in_tools(tools_json: Option<&str>) -> Option<String> {
        let tools_json = tools_json.filter(|raw| !raw.is_empty() && *raw != "[]")?;
        let pyjson::Value::Array(tools) = pyjson::loads(tools_json)? else {
            return None;
        };
        for entry in &tools {
            if !matches!(entry, pyjson::Value::Object(_)) {
                continue;
            }
            let name = truthy_key(entry, "name")
                .or_else(|| truthy_key(entry, "tool"))
                .and_then(pyjson::Value::as_str)
                .unwrap_or("");
            if !SHELL_TOOL_NAMES.contains(&name) {
                continue;
            }
            let candidate = truthy_key(entry, "input")
                .or_else(|| truthy_key(entry, "arguments"))
                .unwrap_or(entry);
            let command = if matches!(candidate, pyjson::Value::Object(_)) {
                truthy_key(candidate, "command")
                    .or_else(|| truthy_key(candidate, "cmd"))
                    .and_then(pyjson::Value::as_str)
                    .unwrap_or("")
            } else {
                ""
            };
            if command.is_empty() {
                continue;
            }
            let normalized = command
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if REVERT_COMMAND_PATTERNS
                .iter()
                .any(|pattern| normalized.contains(pattern))
            {
                return Some(command.to_string());
            }
        }
        None
    }

    /// One transcript row, as `_classify_outcome` reads it.
    #[derive(Debug, Clone)]
    pub struct MessageRow {
        /// `messages.id`.
        pub id: i64,
        /// `messages.role`.
        pub role: String,
        /// `messages.content_text`.
        pub content_text: String,
        /// `messages.tools_json`.
        pub tools_json: Option<String>,
        /// `messages.is_sidechain`.
        pub is_sidechain: bool,
    }

    /// `discovery._classify_outcome` — `(outcome, evidence, msg_id, confidence)`.
    #[must_use]
    pub fn classify_outcome(messages: &[MessageRow], anchor_idx: usize) -> OutcomeFields {
        let anchor_id = messages.get(anchor_idx).map_or(0, |row| row.id);
        let tail: Vec<&MessageRow> = messages
            .iter()
            .skip(anchor_idx + 1)
            .filter(|row| !row.is_sidechain)
            .collect();
        if tail.is_empty() {
            return OutcomeFields {
                outcome: "uncertain".into(),
                outcome_evidence:
                    "action is the last recorded turn in the session — no follow-up to judge".into(),
                outcome_msg_id: anchor_id,
                outcome_confidence: CONF_NONE,
            };
        }

        let mut user_turns_seen = 0usize;
        let mut last_user_id = anchor_id;
        for row in tail {
            let role = row.role.to_lowercase();
            if role == "assistant" {
                if let Some(command) = revert_command_in_tools(row.tools_json.as_deref()) {
                    return OutcomeFields {
                        outcome: "reverted".into(),
                        outcome_evidence: format!(
                            "agent ran `{}` after the action",
                            trim_inline(&command, 120)
                        ),
                        outcome_msg_id: row.id,
                        outcome_confidence: CONF_TOOL_REVERT,
                    };
                }
                continue;
            }
            if role != "user" {
                continue;
            }
            let text = row.content_text.trim();
            if text.is_empty() {
                continue; // a tool-result user message — not a real turn
            }
            let Some(class) = classify_user_text(text) else {
                user_turns_seen += 1;
                last_user_id = row.id;
                if user_turns_seen >= LOOKAHEAD {
                    break;
                }
                continue;
            };
            let excerpt = trim_inline(text, 160);
            let outcome = match class {
                "revert" => "reverted",
                "negative" => "failed",
                _ => "worked",
            };
            return OutcomeFields {
                outcome: outcome.into(),
                outcome_evidence: format!("user wrote: '{excerpt}'"),
                outcome_msg_id: row.id,
                outcome_confidence: CONF_EXPLICIT,
            };
        }

        if user_turns_seen == 0 {
            return OutcomeFields {
                outcome: "worked".into(),
                outcome_evidence:
                    "session continued after the action with no user complaint or revert".into(),
                outcome_msg_id: messages.last().map_or(anchor_id, |row| row.id),
                outcome_confidence: CONF_SILENCE,
            };
        }
        OutcomeFields {
            outcome: "uncertain".into(),
            outcome_evidence: format!(
                "{user_turns_seen} follow-up user turn(s) but none confirmed or rejected the action"
            ),
            outcome_msg_id: last_user_id,
            outcome_confidence: CONF_NONE,
        }
    }

    /// `discovery._outcome_matches_for` — the back half both outcome queries share.
    ///
    /// # Errors
    /// When a query fails.
    pub fn outcome_matches_for(
        conn: &Connection,
        anchor_seq_by_fk: &[(i64, i64)],
        wanted_outcomes: &[&str],
        limit: i64,
        min_confidence: f64,
    ) -> Result<Vec<SessionMatch>> {
        outcome_matches_for_clustered(
            conn,
            anchor_seq_by_fk,
            wanted_outcomes,
            limit,
            min_confidence,
            &HashMap::new(),
        )
    }

    /// `_outcome_matches_for(..., more_by_fk=…)` — the FTS branch's variant.
    ///
    /// `more_by_fk` carries the bm25 content half's clustering count per
    /// `sessions.id` (how many *further* content-mention messages that session
    /// had). It rides onto each match's `more_matches_in_session`; absent or
    /// zero leaves the field `None`, so the JSON shape is unchanged on the
    /// `LIKE` path.
    ///
    /// # Errors
    /// When a query fails.
    pub fn outcome_matches_for_clustered(
        conn: &Connection,
        anchor_seq_by_fk: &[(i64, i64)],
        wanted_outcomes: &[&str],
        limit: i64,
        min_confidence: f64,
        more_by_fk: &HashMap<i64, i64>,
    ) -> Result<Vec<SessionMatch>> {
        if anchor_seq_by_fk.is_empty() {
            return Ok(Vec::new());
        }
        let anchors: HashMap<i64, i64> = anchor_seq_by_fk.iter().copied().collect();
        let sql = format!(
            "SELECT {}, s.id AS session_fk {} WHERE s.id IN ({}) ORDER BY s.last_ts DESC",
            super::SESSION_SELECT,
            super::SESSION_FROM,
            super::placeholders(anchor_seq_by_fk.len())
        );
        let params: Vec<rusqlite::types::Value> = anchor_seq_by_fk
            .iter()
            .map(|(fk, _)| rusqlite::types::Value::Integer(*fk))
            .collect();
        let mut stmt = conn.prepare(&sql)?;
        let meta_rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((row.get::<_, i64>(8)?, row_to_match(row)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut out: Vec<SessionMatch> = Vec::new();
        let mut messages_stmt = conn.prepare(
            "SELECT id, seq, role, content_text, tools_json, is_sidechain \
             FROM messages WHERE session_fk = ? AND seq >= ? ORDER BY seq, id",
        )?;
        for (session_fk, mut session_match) in meta_rows {
            let anchor_seq = anchors.get(&session_fk).copied().unwrap_or(0);
            let msg_rows = messages_stmt
                .query_map(rusqlite::params![session_fk, anchor_seq], |row| {
                    Ok(MessageRow {
                        id: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        role: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        content_text: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        tools_json: row.get::<_, Option<String>>(4)?,
                        is_sidechain: row.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if msg_rows.is_empty() {
                continue;
            }
            let fields = classify_outcome(&msg_rows, 0);
            if !wanted_outcomes.contains(&fields.outcome.as_str()) {
                continue;
            }
            if fields.outcome_confidence < min_confidence {
                continue;
            }
            session_match.outcome = Some(fields);
            // `more_matches_in_session=more_by_fk.get(sfk) or None` — a zero is
            // falsy in Python, so it lands as `None`, not as `0`.
            let more = more_by_fk.get(&session_fk).copied().unwrap_or(0);
            session_match.more_matches_in_session = (more != 0).then_some(more);
            out.push(session_match);
            if limit > 0 && i64::try_from(out.len()).unwrap_or(i64::MAX) >= limit {
                break;
            }
        }
        Ok(out)
    }
}

// ── SQL fragments, ported literally ──────────────────────────────────────────

/// `discovery._SESSION_SELECT`, spacing included.
pub(crate) const SESSION_SELECT: &str = "  s.session_id           AS session_id,\
  p.slug                 AS project_slug,\
  p.path                 AS stored_path,\
  p.provider             AS provider,\
  s.first_ts             AS first_ts,\
  s.last_ts              AS last_ts,\
  s.message_count        AS message_count,\
  COALESCE(sm.cost_usd, 0.0) AS cost_usd";

/// `discovery._SESSION_FROM`.
pub(crate) const SESSION_FROM: &str = "FROM sessions s \
JOIN projects p ON p.id = s.project_id \
LEFT JOIN session_mart sm ON sm.session_id = s.session_id";

/// Column indices into a `SESSION_SELECT` row.
const COL_SESSION_ID: usize = 0;
const COL_PROJECT_SLUG: usize = 1;
const COL_STORED_PATH: usize = 2;
const COL_PROVIDER: usize = 3;
const COL_FIRST_TS: usize = 4;
const COL_LAST_TS: usize = 5;
const COL_MESSAGE_COUNT: usize = 6;
const COL_COST_USD: usize = 7;

/// Build a [`SessionMatch`] from a `SESSION_SELECT` row — `discovery._row_to_match`.
pub(crate) fn row_to_match(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMatch> {
    let project_slug: String = row
        .get::<_, Option<String>>(COL_PROJECT_SLUG)?
        .unwrap_or_default();
    let stored_path: Option<String> = row.get(COL_STORED_PATH)?;
    Ok(SessionMatch {
        session_id: row
            .get::<_, Option<String>>(COL_SESSION_ID)?
            .unwrap_or_default(),
        project_path: paths::project_fs_path(stored_path.as_deref(), &project_slug),
        project_slug,
        provider: row
            .get::<_, Option<String>>(COL_PROVIDER)?
            .unwrap_or_default(),
        first_ts: row
            .get::<_, Option<String>>(COL_FIRST_TS)?
            .unwrap_or_default(),
        last_ts: row
            .get::<_, Option<String>>(COL_LAST_TS)?
            .unwrap_or_default(),
        message_count: row.get::<_, Option<i64>>(COL_MESSAGE_COUNT)?.unwrap_or(0),
        cost_usd: row.get::<_, Option<f64>>(COL_COST_USD)?.unwrap_or(0.0),
        snippet: None,
        embedding_score: None,
        more_matches_in_session: None,
        outcome: None,
    })
}

/// `if project:` — Python's truthiness, where `""` is *no* filter at all.
///
/// `--project ''` reaches these queries as an empty string (`cli.py` only tests
/// `slug is None` before falling back to the cwd), and every `WHERE` builder in
/// `discovery.py` then guards with `if project:` — so an empty slug searches
/// **every** project. Matching on `p.slug = ''` instead returns nothing, which
/// is a changed answer on a command that exits 0 either way.
fn project_filter(project: Option<&str>) -> Option<&str> {
    project.filter(|slug| !slug.is_empty())
}

/// `",?,?,…"` — one placeholder per element, the idiom every list-scoped query
/// in the reference uses (§6b: the list-subquery shape is load-bearing).
pub(crate) fn placeholders(count: usize) -> String {
    let mut out = String::with_capacity(count * 2);
    for index in 0..count {
        if index > 0 {
            out.push(',');
        }
        out.push('?');
    }
    out
}

// ── queries ──────────────────────────────────────────────────────────────────

/// `discovery.find_sessions_in_path` — sessions whose project is `path` or an
/// ancestor of it.
///
/// `context_budget` is always applied (the CLI always passes one); the plain
/// `list[SessionMatch]` overload of the Python function has no wave-1 caller.
///
/// # Errors
/// When a query fails, or `since` is malformed (`ValueError` in Python).
pub fn find_sessions_in_path(
    conn: &Connection,
    path: &str,
    since: Option<&str>,
    limit: impl Into<Limit>,
    provider: Option<&str>,
    budget: &rank::Budget,
) -> Result<BudgetedResult> {
    let limit: Limit = limit.into();
    let resolved = paths::resolve_input_path(path);
    let relevance = rank::relevance_in_path(&resolved);

    let mut stmt = conn.prepare("SELECT id, provider, slug, path FROM projects")?;
    let project_rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut matched_ids: Vec<i64> = Vec::new();
    for (id, row_provider, slug, stored_path) in &project_rows {
        if let Some(wanted) = provider
            && row_provider != wanted
        {
            continue;
        }
        let fs_path = paths::project_fs_path(stored_path.as_deref(), slug);
        if fs_path.is_empty() {
            continue;
        }
        if paths::is_ancestor(&fs_path, &resolved) {
            matched_ids.push(*id);
        }
    }

    if matched_ids.is_empty() {
        return Ok(rank::budgeted(Vec::new(), budget, &relevance));
    }

    let since_iso = pytime::parse_since(since)?;

    let mut sql = format!(
        "SELECT {SESSION_SELECT} {SESSION_FROM} WHERE s.project_id IN ({})",
        placeholders(matched_ids.len())
    );
    let mut params: Vec<rusqlite::types::Value> = matched_ids
        .iter()
        .map(|id| rusqlite::types::Value::Integer(*id))
        .collect();
    if let Some(iso) = &since_iso {
        sql.push_str(" AND s.last_ts >= ?");
        params.push(rusqlite::types::Value::Text(iso.clone()));
    }
    sql.push_str(" ORDER BY s.last_ts DESC");
    if limit.is_positive() {
        sql.push_str(" LIMIT ?");
        params.push(rusqlite::types::Value::Integer(limit.bind()?));
    }

    let mut stmt = conn.prepare(&sql)?;
    let matches = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), row_to_match)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rank::budgeted(matches, budget, &relevance))
}

/// `discovery.find_sessions_touching_file(context_budget=None)` — a plain list.
///
/// # Errors
/// When a query fails.
pub fn find_sessions_touching_file(
    conn: &Connection,
    file_path: &str,
    limit: impl Into<Limit>,
) -> Result<Vec<SessionMatch>> {
    Ok(touching_file(conn, file_path, limit.into(), outcome::Mode::Any, None)?.0)
}

/// `discovery.find_sessions_touching_file(context_budget=N)` — a [`BudgetedResult`].
///
/// # Errors
/// When a query fails.
pub fn find_sessions_touching_file_budgeted(
    conn: &Connection,
    file_path: &str,
    limit: impl Into<Limit>,
    budget: &rank::Budget,
) -> Result<BudgetedResult> {
    find_sessions_touching_file_budgeted_mode(conn, file_path, limit, outcome::Mode::Any, budget)
}

/// `discovery.find_sessions_touching_file(mode=…, context_budget=N)`.
///
/// The `read` / `write` modes are the `find-sessions-touching-file --mode`
/// surface: the SQL filter narrows to `tools_json LIKE ?` alone (no
/// `content_text` half at all) and the Python refinement pass only asks the
/// tool-arg matcher, so a free-form mention of the path in prose never matches.
///
/// # Errors
/// When a query fails.
pub fn find_sessions_touching_file_budgeted_mode(
    conn: &Connection,
    file_path: &str,
    limit: impl Into<Limit>,
    mode: outcome::Mode,
    budget: &rank::Budget,
) -> Result<BudgetedResult> {
    let (_, budgeted) = touching_file(conn, file_path, limit.into(), mode, Some(budget))?;
    budgeted.context("a budgeted touching-file query must produce a budgeted result")
}

/// The shared body — `search_service=None`.
fn touching_file(
    conn: &Connection,
    file_path: &str,
    limit: Limit,
    mode: outcome::Mode,
    budget: Option<&rank::Budget>,
) -> Result<(Vec<SessionMatch>, Option<BudgetedResult>)> {
    let resolved = paths::resolve_input_path(file_path);
    let pattern = format!("%{resolved}%");

    // Stage 1 — one combined scan, refined in Python (here: in Rust). Both
    // halves of the OR are leading-wildcard, which is the full-scan §6b warns
    // about; it is the reference's shape and stays. `read`/`write` drop the
    // `content_text` half from the SQL entirely, exactly as the reference does.
    let any_mode = mode == outcome::Mode::Any;
    let sql = if any_mode {
        "SELECT s.id AS sfk, s.session_id AS sid, m.tools_json, m.content_text \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         WHERE (m.tools_json LIKE ? OR m.content_text LIKE ?)"
    } else {
        "SELECT s.id AS sfk, s.session_id AS sid, m.tools_json, m.content_text \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         WHERE m.tools_json LIKE ?"
    };
    let params: Vec<rusqlite::types::Value> = if any_mode {
        vec![
            rusqlite::types::Value::Text(pattern.clone()),
            rusqlite::types::Value::Text(pattern),
        ]
    } else {
        vec![rusqlite::types::Value::Text(pattern)]
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut matched_session_fks: Vec<i64> = Vec::new();
    let mut seen_fks: HashSet<i64> = HashSet::new();
    let mut match_kind_by_sid: HashMap<String, &'static str> = HashMap::new();
    for (sfk, sid, tools_json, content_text) in &rows {
        if seen_fks.contains(sfk) {
            continue;
        }
        if !any_mode {
            // `read` / `write`: only the tool-arg matcher decides, and a row
            // that fails it is skipped outright (`continue` in the reference).
            if outcome::tools_json_mentions_file(tools_json.as_deref(), &resolved, mode) {
                seen_fks.insert(*sfk);
                matched_session_fks.push(*sfk);
                match_kind_by_sid.insert(sid.clone(), "tool");
            }
            continue;
        }
        if outcome::tools_json_mentions_file(tools_json.as_deref(), &resolved, outcome::Mode::Any) {
            seen_fks.insert(*sfk);
            matched_session_fks.push(*sfk);
            match_kind_by_sid.insert(sid.clone(), "tool");
        } else if content_text.as_deref().unwrap_or("").contains(&resolved) {
            seen_fks.insert(*sfk);
            matched_session_fks.push(*sfk);
            match_kind_by_sid.insert(sid.clone(), "content");
        }
    }

    let relevance = rank::relevance_touching_file(match_kind_by_sid);

    if matched_session_fks.is_empty() {
        return Ok(match budget {
            Some(budget) => (
                Vec::new(),
                Some(rank::budgeted(Vec::new(), budget, &relevance)),
            ),
            None => (Vec::new(), None),
        });
    }

    // Python iterates a `set` here, so the id order handed to SQLite is
    // arbitrary — but the query is `ORDER BY s.last_ts DESC`, so the result
    // order is not. Insertion order is used instead: same rows, same order out.
    let mut sql = format!(
        "SELECT {SESSION_SELECT} {SESSION_FROM} WHERE s.id IN ({}) ORDER BY s.last_ts DESC",
        placeholders(matched_session_fks.len())
    );
    let mut params: Vec<rusqlite::types::Value> = matched_session_fks
        .iter()
        .map(|fk| rusqlite::types::Value::Integer(*fk))
        .collect();
    if limit.is_positive() {
        sql.push_str(" LIMIT ?");
        params.push(rusqlite::types::Value::Integer(limit.bind()?));
    }
    let mut stmt = conn.prepare(&sql)?;
    let matches = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), row_to_match)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(match budget {
        Some(budget) => {
            let budgeted = rank::budgeted(matches, budget, &relevance);
            (budgeted.sessions.clone(), Some(budgeted))
        }
        None => (matches, None),
    })
}

/// `discovery.search_past_decisions` — the `LIKE` branch (`search_service=None`).
///
/// # Errors
/// When a query fails, or `since` is malformed.
pub fn search_past_decisions(
    conn: &Connection,
    query: &str,
    project: Option<&str>,
    since: Option<&str>,
    limit: i64,
    budget: &rank::Budget,
) -> Result<BudgetedResult> {
    search_past_decisions_scored(conn, query, project, since, limit, budget, None)
}

/// `{session_fk: cosine in [0, 1]}` for the first-hit `(session_fk, message_id)`
/// pairs the substring scan collected, in insertion order.
///
/// A callback because the scorer talks HTTP to Ollama and `stax-core::queries`
/// is the store layer — the CLI injects [`crate::ask::embed_texts`] behind it.
pub type EmbeddingScorer<'a> = &'a dyn Fn(&Connection, &str, &[(i64, i64)]) -> HashMap<i64, f64>;

/// `discovery.search_past_decisions(use_embeddings=True)` — the `--use-embeddings`
/// re-rank of `search-past-decisions`.
///
/// Same substring pre-filter, same rows; what changes is the *relevance* term
/// (cosine instead of needle density) and the `embedding_score` key each row
/// gains. When the scorer returns nothing — Ollama down, which is the common
/// case — every relevance is `0.0` and every row keeps the 9-key shape, so the
/// only visible effect is a recency+cost ordering under a budget. That silent
/// degradation is the reference's design, not an accident.
///
/// # Errors
/// When a query fails, or `since` is malformed.
pub fn search_past_decisions_embeddings(
    conn: &Connection,
    query: &str,
    project: Option<&str>,
    since: Option<&str>,
    limit: i64,
    budget: &rank::Budget,
    scorer: EmbeddingScorer<'_>,
) -> Result<BudgetedResult> {
    search_past_decisions_scored(conn, query, project, since, limit, budget, Some(scorer))
}

fn search_past_decisions_scored(
    conn: &Connection,
    query: &str,
    project: Option<&str>,
    since: Option<&str>,
    limit: i64,
    budget: &rank::Budget,
    scorer: Option<EmbeddingScorer<'_>>,
) -> Result<BudgetedResult> {
    if query.trim().is_empty() {
        // `rank_fn` is chosen before the empty-query floor in the reference, so
        // the embeddings branch's floor uses the embeddings relevance too. No
        // observable difference on an empty result — recorded for the reader.
        let relevance = match scorer {
            Some(_) => rank::relevance_embeddings(),
            None => rank::relevance_decisions(HashMap::new()),
        };
        return Ok(rank::budgeted(Vec::new(), budget, &relevance));
    }
    let needle = query.trim();
    let since_iso = pytime::parse_since(since)?;

    let mut sql = String::from(
        "SELECT m.id AS mid, m.session_fk AS sfk, s.session_id AS sid, \
         m.content_text AS content_text \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         JOIN projects p ON p.id = s.project_id \
         WHERE m.content_text LIKE ?",
    );
    let mut params: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(format!("%{needle}%"))];
    if let Some(slug) = project_filter(project) {
        sql.push_str(" AND p.slug = ?");
        params.push(rusqlite::types::Value::Text(slug.to_string()));
    }
    if let Some(iso) = &since_iso {
        sql.push_str(" AND m.timestamp >= ?");
        params.push(rusqlite::types::Value::Text(iso.clone()));
    }
    sql.push_str(" ORDER BY m.timestamp DESC");

    let mut stmt = conn.prepare(&sql)?;
    let hit_rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let needle_lower = needle.to_lowercase();
    // Insertion-ordered so the `IN (…)` list matches Python's dict order.
    let mut snippet_order: Vec<i64> = Vec::new();
    let mut snippet_by_sfk: HashMap<i64, Option<String>> = HashMap::new();
    let mut occ_by_sid: HashMap<String, i64> = HashMap::new();
    let mut msg_count_by_sfk: HashMap<i64, i64> = HashMap::new();
    // `first_mid_by_sfk` in the reference: one `(session_fk, message_id)` pair
    // per session, in first-hit order, which is the embed batch's order.
    let mut first_mid_by_sfk: Vec<(i64, i64)> = Vec::new();
    for (mid, sfk, sid, content_text) in &hit_rows {
        let content = content_text.as_deref().unwrap_or("");
        *occ_by_sid.entry(sid.clone()).or_insert(0) +=
            count_occurrences(&content.to_lowercase(), &needle_lower);
        *msg_count_by_sfk.entry(*sfk).or_insert(0) += 1;
        if snippet_by_sfk.contains_key(sfk) {
            continue;
        }
        snippet_order.push(*sfk);
        snippet_by_sfk.insert(*sfk, build_snippet(content, needle));
        first_mid_by_sfk.push((*sfk, *mid));
    }

    // `if use_embeddings and first_mid_by_sfk:` — the scores are computed before
    // the rows are hydrated, because they replace the relevance term as well as
    // populating `embedding_score`.
    let score_by_sfk: HashMap<i64, f64> = match scorer {
        Some(scorer) if !first_mid_by_sfk.is_empty() => scorer(conn, needle, &first_mid_by_sfk),
        _ => HashMap::new(),
    };

    let relevance = match scorer {
        Some(_) => rank::relevance_embeddings(),
        None => rank::relevance_decisions(occ_by_sid),
    };
    if snippet_order.is_empty() {
        return Ok(rank::budgeted(Vec::new(), budget, &relevance));
    }

    let sql = format!(
        "SELECT {SESSION_SELECT}, s.id AS session_fk {SESSION_FROM} \
         WHERE s.id IN ({}) ORDER BY s.last_ts DESC",
        placeholders(snippet_order.len())
    );
    let params: Vec<rusqlite::types::Value> = snippet_order
        .iter()
        .map(|fk| rusqlite::types::Value::Integer(*fk))
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            let session_fk: i64 = row.get(8)?;
            Ok((session_fk, row_to_match(row)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out: Vec<SessionMatch> = Vec::new();
    for (session_fk, mut session_match) in rows {
        session_match.snippet = snippet_by_sfk.get(&session_fk).cloned().flatten();
        // `score_by_sfk.get(sfk) if use_embeddings else None` — an absent score
        // stays `None`, so the row keeps its 9-key shape and ranks 0.0.
        session_match.embedding_score = scorer
            .is_some()
            .then(|| score_by_sfk.get(&session_fk).copied())
            .flatten();
        let more = msg_count_by_sfk.get(&session_fk).copied().unwrap_or(1) - 1;
        session_match.more_matches_in_session = (more != 0).then_some(more);
        out.push(session_match);
        if limit > 0 && i64::try_from(out.len()).unwrap_or(i64::MAX) >= limit {
            break;
        }
    }

    Ok(rank::budgeted(out, budget, &relevance))
}

/// Everything `discovery.find_sessions_where_action_worked` takes past `action`.
///
/// A struct rather than five positional arguments because the indexed variant
/// would otherwise cross clippy's argument bound — and because `project` and
/// `file_path` are both `Option<&str>` and swapping them at a call site would
/// compile silently.
#[derive(Debug, Clone, Copy)]
pub struct ActionWorked<'a> {
    /// `projects.slug` filter.
    pub project: Option<&'a str>,
    /// Narrow to sessions that *also* mention this file, anywhere.
    pub file_path: Option<&'a str>,
    /// `--since`.
    pub since: Option<&'a str>,
    /// `--limit`; `<= 0` means no cap.
    pub limit: i64,
    /// `--min-confidence`, clamped into `[0.0, 1.0]` here as the reference does.
    pub min_confidence: f64,
}

impl<'a> ActionWorked<'a> {
    /// The defaults every `memory worked` call uses: no file narrowing.
    #[must_use]
    pub fn new(
        project: Option<&'a str>,
        since: Option<&'a str>,
        limit: i64,
        min_confidence: f64,
    ) -> Self {
        Self {
            project,
            file_path: None,
            since,
            limit,
            min_confidence,
        }
    }
}

/// `discovery.find_sessions_where_action_worked` — the `search_service=None` branch.
///
/// # Errors
/// When a query fails, or `since` is malformed.
pub fn find_sessions_where_action_worked(
    conn: &Connection,
    action: &str,
    filters: &ActionWorked<'_>,
) -> Result<Vec<SessionMatch>> {
    if action.trim().is_empty() {
        return Ok(Vec::new());
    }
    let min_confidence = filters.min_confidence.clamp(0.0, 1.0);
    let needle = action.trim();
    let since_iso = pytime::parse_since(filters.since)?;

    let mut where_clauses = vec!["(m.tools_json LIKE ? OR m.content_text LIKE ?)".to_string()];
    let mut params: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Text(format!("%{needle}%")),
        rusqlite::types::Value::Text(format!("%{needle}%")),
    ];
    if let Some(slug) = project_filter(filters.project) {
        where_clauses.push("p.slug = ?".to_string());
        params.push(rusqlite::types::Value::Text(slug.to_string()));
    }
    if let Some(iso) = &since_iso {
        where_clauses.push("m.timestamp >= ?".to_string());
        params.push(rusqlite::types::Value::Text(iso.clone()));
    }
    let sql = format!(
        "SELECT s.id AS sfk, MAX(m.seq) AS anchor_seq \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         JOIN projects p ON p.id = s.project_id \
         WHERE {} GROUP BY s.id",
        where_clauses.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut anchor_seq_by_fk = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    narrow_to_file(conn, &mut anchor_seq_by_fk, filters.file_path, &mut None)?;

    outcome::outcome_matches_for(
        conn,
        &anchor_seq_by_fk,
        &["worked"],
        filters.limit,
        min_confidence,
    )
}

/// `if file_path and anchor_seq_by_fk:` — intersect with the sessions that
/// mention the resolved path anywhere (tool args *or* free text).
///
/// A no-op when no `--file` was given or the candidate set is already empty,
/// which is load-bearing: the reference guards on both, so `--file` never runs
/// its own full scan for a query that matched nothing.
fn narrow_to_file(
    conn: &Connection,
    anchors: &mut Vec<(i64, i64)>,
    file_path: Option<&str>,
    more_by_fk: &mut Option<&mut HashMap<i64, i64>>,
) -> Result<()> {
    let Some(file_path) = file_path.filter(|path| !path.is_empty()) else {
        return Ok(());
    };
    if anchors.is_empty() {
        return Ok(());
    }
    let resolved = paths::resolve_input_path(file_path);
    let pattern = format!("%{resolved}%");
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.id AS sfk FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         WHERE m.tools_json LIKE ? OR m.content_text LIKE ?",
    )?;
    let touched: HashSet<i64> = stmt
        .query_map([&pattern, &pattern], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    anchors.retain(|(fk, _)| touched.contains(fk));
    if let Some(more) = more_by_fk.as_mut() {
        let kept: HashSet<i64> = anchors.iter().map(|(fk, _)| *fk).collect();
        more.retain(|fk, _| kept.contains(fk));
    }
    Ok(())
}

/// `discovery.find_failure_modes_for_file`.
///
/// # Errors
/// When a query fails, or `since` is malformed.
pub fn find_failure_modes_for_file(
    conn: &Connection,
    file_path: &str,
    since: Option<&str>,
    limit: i64,
    min_confidence: f64,
) -> Result<Vec<SessionMatch>> {
    let resolved = paths::resolve_input_path(file_path);
    let since_iso = pytime::parse_since(since)?;
    let min_confidence = min_confidence.clamp(0.0, 1.0);
    let anchors = write_mode_anchors(conn, &resolved, since_iso.as_deref())?;
    outcome::outcome_matches_for(
        conn,
        &anchors,
        &["failed", "reverted"],
        limit,
        min_confidence,
    )
}

/// The "last write-mode mention per session" anchor pass shared by
/// `find_failure_modes_for_file` and `risk.file_risk_summary`.
fn write_mode_anchors(
    conn: &Connection,
    resolved: &str,
    since_iso: Option<&str>,
) -> Result<Vec<(i64, i64)>> {
    let mut sql = String::from(
        "SELECT s.id AS sfk, m.seq AS seq, m.tools_json AS tools_json \
         FROM messages m JOIN sessions s ON s.id = m.session_fk \
         WHERE m.tools_json LIKE ?",
    );
    let mut params: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(format!("%{resolved}%"))];
    if let Some(iso) = since_iso {
        sql.push_str(" AND m.timestamp >= ?");
        params.push(rusqlite::types::Value::Text(iso.to_string()));
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut order: Vec<i64> = Vec::new();
    let mut anchor_by_fk: HashMap<i64, i64> = HashMap::new();
    for (sfk, seq, tools_json) in &rows {
        if !outcome::tools_json_mentions_file(tools_json.as_deref(), resolved, outcome::Mode::Write)
        {
            continue;
        }
        match anchor_by_fk.get_mut(sfk) {
            Some(current) => {
                if *seq > *current {
                    *current = *seq;
                }
            }
            None => {
                order.push(*sfk);
                anchor_by_fk.insert(*sfk, *seq);
            }
        }
    }
    Ok(order
        .into_iter()
        .map(|fk| (fk, anchor_by_fk[&fk]))
        .collect())
}

/// `risk.file_risk_summary`'s return shape.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskSummary {
    /// The absolute path the heuristic actually looked at.
    pub path: String,
    /// The `--since` string, echoed verbatim (not the parsed ISO form).
    pub since: Option<String>,
    /// Distinct sessions touching the file at all.
    pub total_sessions: i64,
    /// Sessions whose last write-mode mention was reverted.
    pub reverted: i64,
    /// …whose last write-mode mention failed.
    pub failed: i64,
    /// …whose last write-mode mention worked.
    pub worked: i64,
    /// Failure-mode session ids, newest first, capped at `recent_limit`.
    pub recent_session_ids: Vec<String>,
}

impl RiskSummary {
    /// Serialise in `file_risk_summary`'s key order.
    #[must_use]
    pub fn to_dict(&self) -> pyjson::Value {
        pyjson::Value::Object(vec![
            ("path".into(), pyjson::Value::from(&self.path)),
            (
                "since".into(),
                match &self.since {
                    Some(raw) => pyjson::Value::from(raw),
                    None => pyjson::Value::Null,
                },
            ),
            (
                "total_sessions".into(),
                pyjson::Value::Int(self.total_sessions),
            ),
            ("reverted".into(), pyjson::Value::Int(self.reverted)),
            ("failed".into(), pyjson::Value::Int(self.failed)),
            ("worked".into(), pyjson::Value::Int(self.worked)),
            (
                "recent_session_ids".into(),
                pyjson::Value::Array(
                    self.recent_session_ids
                        .iter()
                        .map(pyjson::Value::from)
                        .collect(),
                ),
            ),
        ])
    }
}

/// `risk.file_risk_summary`.
///
/// # Errors
/// When a query fails, or `since` is malformed.
pub fn file_risk_summary(
    conn: &Connection,
    path: &str,
    since: Option<&str>,
    recent_limit: i64,
) -> Result<RiskSummary> {
    let since_iso = pytime::parse_since(since)?;
    let resolved = paths::resolve_input_path(path);
    let pattern = format!("%{resolved}%");

    let mut sql = String::from(
        "SELECT DISTINCT s.id AS sfk \
         FROM messages m JOIN sessions s ON s.id = m.session_fk \
         WHERE (m.tools_json LIKE ? OR m.content_text LIKE ?)",
    );
    let mut params: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Text(pattern.clone()),
        rusqlite::types::Value::Text(pattern.clone()),
    ];
    if let Some(iso) = &since_iso {
        sql.push_str(" AND m.timestamp >= ?");
        params.push(rusqlite::types::Value::Text(iso.clone()));
    }
    let mut stmt = conn.prepare(&sql)?;
    let total_sessions = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .len();

    // The Python helper re-enters `find_failure_modes_for_file` with the raw
    // `since` (not the parsed ISO) and an unbounded limit.
    let fail_mode = find_failure_modes_for_file(
        conn,
        &resolved,
        since,
        0,
        outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE,
    )?;
    let reverted = fail_mode
        .iter()
        .filter(|m| m.outcome.as_ref().is_some_and(|o| o.outcome == "reverted"))
        .count();
    let failed = fail_mode
        .iter()
        .filter(|m| m.outcome.as_ref().is_some_and(|o| o.outcome == "failed"))
        .count();

    let anchors = write_mode_anchors(conn, &resolved, since_iso.as_deref())?;
    let worked = outcome::outcome_matches_for(
        conn,
        &anchors,
        &["worked"],
        0,
        outcome::DEFAULT_MIN_OUTCOME_CONFIDENCE,
    )?
    .len();

    let mut recent: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for session_match in &fail_mode {
        if seen.contains(&session_match.session_id) {
            continue;
        }
        recent.push(session_match.session_id.clone());
        seen.insert(session_match.session_id.clone());
        if recent_limit > 0 && i64::try_from(recent.len()).unwrap_or(i64::MAX) >= recent_limit {
            break;
        }
    }

    Ok(RiskSummary {
        path: resolved,
        since: since.map(str::to_string),
        total_sessions: i64::try_from(total_sessions).unwrap_or(i64::MAX),
        reverted: i64::try_from(reverted).unwrap_or(i64::MAX),
        failed: i64::try_from(failed).unwrap_or(i64::MAX),
        worked: i64::try_from(worked).unwrap_or(i64::MAX),
        recent_session_ids: recent,
    })
}

/// `cli._detect_cwd_project_slug` — which project does `cwd` belong to?
///
/// This is the cwd-scoping the `memory` verbs apply when `--project` is absent.
/// It is a **ledgered behavior**: on a store whose slugs came from a different
/// machine nothing matches, the scope stays `None`, and the query silently runs
/// unscoped. Ported as-is.
///
/// # Errors
/// Never — a failing query yields `None`, as the Python `except` does.
pub fn detect_cwd_project_slug(conn: &Connection, cwd: &str) -> Option<String> {
    let cwd_slug: String = cwd
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let mut stmt = conn
        .prepare("SELECT DISTINCT slug, path FROM projects")
        .ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok()?;

    let mut best_slug: Option<String> = None;
    let mut best_score: i64 = -1;
    for (slug, path) in rows {
        let Some(slug) = slug.filter(|value| !value.is_empty()) else {
            continue;
        };
        let (matched, score) = match path.filter(|value| !value.is_empty()) {
            Some(path) => {
                let anchor = path.trim_end_matches('/');
                (
                    cwd == anchor || cwd.starts_with(&format!("{anchor}/")),
                    i64::try_from(anchor.chars().count()).unwrap_or(i64::MAX),
                )
            }
            None => (
                cwd_slug == slug || cwd_slug.starts_with(&format!("{slug}-")),
                i64::try_from(slug.chars().count()).unwrap_or(i64::MAX),
            ),
        };
        if matched && score > best_score {
            best_score = score;
            best_slug = Some(slug);
        }
    }
    best_slug
}

// ── the FTS5 / bm25 branch (RS-1-007) ────────────────────────────────────────
//
// Everything above is `search_service=None`. The `memory` CLI does NOT pass
// `None`: `cli._lexical_search_service()` builds a `SearchService` on every
// invocation of `decisions`, `worked`, `file` and `sessions <file>`, and when
// `search_index.db` has rows those three discovery functions take a different
// path — bm25 candidate gathering, per-session clustering, and a relevance term
// that carries the FTS score. On the maintainer's machine the index holds
// 250,998 messages, so this is the live path and the `LIKE` scan above is the
// fallback that runs on a fresh install.
//
// The fallback is signalled by exactly one value: `LexicalIndex::session_hits`
// returning `None`. A populated index that matched nothing returns
// `Some(vec![])` and the caller must NOT re-run the full scan — reintroducing
// it on a genuine no-match is the anti-pattern the FTS path exists to remove.
//
// Two behaviours here are bug-for-bug and look like defects because they are:
//
// * `memory file` contradicts itself. Its `risk.total_sessions` comes from a
//   `LIKE` scan while its session list comes from bm25, so on a tool-heavy file
//   Python prints "sessions touching the file: 0" directly above a list of 20 of
//   them. That is the current contract and it is reproduced exactly.
// * The clustering count `more_matches_in_session` means different things on the
//   two paths — messages-minus-one on `LIKE`, further-FTS-hits on bm25 — and the
//   two disagree on the same query. Ported as found.

/// `discovery._sessions_by_id` — hydrate store provenance for FTS-matched ids.
///
/// Mirrors the `LIKE` path's `sessions ⨯ projects ⨯ session_mart` join but keys
/// on the provider-facing `session_id` (what the index stores), chunked under
/// SQLite's 999-parameter limit. Later duplicates keep the first row seen.
fn sessions_by_id(
    conn: &Connection,
    session_ids: &[String],
    project: Option<&str>,
) -> Result<HashMap<String, SessionMatch>> {
    let mut out: HashMap<String, SessionMatch> = HashMap::new();
    // `dict.fromkeys(s for s in session_ids if s)` — dedupe, keep first order.
    let mut ids: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for session_id in session_ids {
        if session_id.is_empty() || !seen.insert(session_id.as_str()) {
            continue;
        }
        ids.push(session_id.as_str());
    }
    if ids.is_empty() {
        return Ok(out);
    }
    for chunk in ids.chunks(500) {
        let mut sql = format!(
            "SELECT {SESSION_SELECT} {SESSION_FROM} WHERE s.session_id IN ({})",
            placeholders(chunk.len())
        );
        let mut params: Vec<rusqlite::types::Value> = chunk
            .iter()
            .map(|id| rusqlite::types::Value::Text((*id).to_owned()))
            .collect();
        // `if project:` — the empty slug is no filter at all, as everywhere else.
        if let Some(slug) = project_filter(project) {
            sql.push_str(" AND p.slug = ?");
            params.push(rusqlite::types::Value::Text(slug.to_owned()));
        }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), row_to_match)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for session_match in rows {
            out.entry(session_match.session_id.clone())
                .or_insert(session_match);
        }
    }
    Ok(out)
}

/// A `[0, 1]` relevance lookup over session ids, as a closure the packer takes.
fn relevance_from_scores(scores: HashMap<String, f64>, missing: f64) -> rank::Relevance {
    Box::new(move |session_match: &SessionMatch| {
        scores
            .get(&session_match.session_id)
            .copied()
            .unwrap_or(missing)
    })
}

/// `discovery._relevance_touching_file` **with** its bm25 leg.
///
/// An exact tool-arg match scores `1.0`. A content mention scores `0.5` scaled
/// by its normalised bm25, so an exact match always outranks any content
/// mention while the content mentions still order among themselves. A session
/// with no recorded kind scores `0.25` — that is the reference's default and it
/// is *higher* than a poorly-ranked content hit, which is odd and is kept.
fn relevance_touching_file_bm25(
    match_kind_by_sid: HashMap<String, &'static str>,
    bm25_by_sid: HashMap<String, f64>,
) -> rank::Relevance {
    Box::new(move |session_match: &SessionMatch| {
        let kind = match_kind_by_sid
            .get(&session_match.session_id)
            .copied()
            .unwrap_or("");
        if kind == "content" && !bm25_by_sid.is_empty() {
            return 0.5
                * bm25_by_sid
                    .get(&session_match.session_id)
                    .copied()
                    .unwrap_or(1.0);
        }
        match kind {
            "tool" => 1.0,
            "content" => 0.5,
            _ => 0.25,
        }
    })
}

/// `discovery.search_past_decisions` with a `SearchService` injected.
///
/// Delegates to the `LIKE` port when no index is configured or the index is not
/// populated, so a fresh install is byte-identical to wave 1's proven path.
///
/// # Errors
/// When a query fails, or `since` is malformed.
pub fn search_past_decisions_indexed(
    conn: &Connection,
    index: Option<&crate::lexical::LexicalIndex>,
    query: &str,
    project: Option<&str>,
    since: Option<&str>,
    limit: i64,
    budget: &rank::Budget,
) -> Result<BudgetedResult> {
    let Some(index) = index else {
        return search_past_decisions(conn, query, project, since, limit, budget);
    };
    // The order matters: the empty-query floor and `parse_since` both run
    // *before* the FTS branch in the reference, so a malformed `--since` is a
    // `ValueError` on both paths rather than an FTS miss on one of them.
    if query.trim().is_empty() {
        let relevance = rank::relevance_decisions(HashMap::new());
        return Ok(rank::budgeted(Vec::new(), budget, &relevance));
    }
    let needle = query.trim();
    let since_iso = pytime::parse_since(since)?;

    match fts_decisions(conn, index, needle, project, since_iso.as_deref(), limit)? {
        Some((matches, relevance)) => Ok(rank::budgeted(matches, budget, &relevance)),
        None => search_past_decisions(conn, query, project, since, limit, budget),
    }
}

/// `discovery._fts_decisions` — bm25 candidates, store provenance, Python
/// snippets. `None` means "index not populated, fall back to the `LIKE` scan".
fn fts_decisions(
    conn: &Connection,
    index: &crate::lexical::LexicalIndex,
    needle: &str,
    project: Option<&str>,
    since_iso: Option<&str>,
    limit: i64,
) -> Result<Option<(Vec<SessionMatch>, rank::Relevance)>> {
    // `None` here is the `LIMIT ?` bind that Python's unbounded int makes
    // overflow — swallowed by the `except Exception` around the call, so it
    // reaches the caller as "no lexical half" exactly like an empty index.
    let Some(candidate_k) = crate::lexical::candidate_k(limit) else {
        return Ok(None);
    };
    let Some(hits) = index.session_hits(needle, project, since_iso, candidate_k) else {
        return Ok(None);
    };

    let scored: Vec<(String, f64)> = hits
        .iter()
        .map(|hit| (hit.session_id.clone(), hit.bm25))
        .collect();
    let relevance = relevance_from_scores(
        crate::lexical::bm25_relevance(&scored)
            .into_iter()
            .collect(),
        0.0,
    );
    if hits.is_empty() {
        // Populated and genuinely no match — never a `LIKE` scan.
        return Ok(Some((Vec::new(), relevance)));
    }

    let order: Vec<String> = hits.iter().map(|hit| hit.session_id.clone()).collect();
    let rows_by_sid = sessions_by_id(conn, &order, project)?;

    let mut out: Vec<SessionMatch> = Vec::new();
    for hit in &hits {
        // An FTS hit with no store provenance (index/store drift) is skipped.
        let Some(row) = rows_by_sid.get(&hit.session_id) else {
            continue;
        };
        let mut session_match = row.clone();
        // The snippet is built from the FTS row's content by the *same* Python
        // snippet builder the LIKE path uses, so the format is identical across
        // both paths even though the source message differs.
        session_match.snippet = build_snippet(&hit.content, needle);
        session_match.more_matches_in_session =
            (hit.more_matches_in_session != 0).then_some(hit.more_matches_in_session);
        out.push(session_match);
    }
    // `sort(key=…, reverse=True)` on CPython is stable, so sessions sharing a
    // `last_ts` keep their bm25 order. `sort_by` is stable too.
    out.sort_by(|left, right| right.last_ts.cmp(&left.last_ts));
    if limit > 0 {
        out.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    Ok(Some((out, relevance)))
}

/// `discovery.find_sessions_touching_file(context_budget=None)` with an index.
///
/// # Errors
/// When a query fails.
pub fn find_sessions_touching_file_indexed(
    conn: &Connection,
    index: Option<&crate::lexical::LexicalIndex>,
    file_path: &str,
    limit: impl Into<Limit>,
) -> Result<Vec<SessionMatch>> {
    let limit: Limit = limit.into();
    let Some(index) = index else {
        return Ok(touching_file(conn, file_path, limit, outcome::Mode::Any, None)?.0);
    };
    let resolved = paths::resolve_input_path(file_path);
    match touching_file_fts(conn, index, &resolved, limit, None)? {
        Some((matches, _)) => Ok(matches),
        None => Ok(touching_file(conn, file_path, limit, outcome::Mode::Any, None)?.0),
    }
}

/// `discovery.find_sessions_touching_file(context_budget=N)` with an index.
///
/// # Errors
/// When a query fails.
pub fn find_sessions_touching_file_budgeted_indexed(
    conn: &Connection,
    index: Option<&crate::lexical::LexicalIndex>,
    file_path: &str,
    limit: impl Into<Limit>,
    budget: &rank::Budget,
) -> Result<BudgetedResult> {
    find_sessions_touching_file_budgeted_indexed_mode(
        conn,
        index,
        file_path,
        limit,
        outcome::Mode::Any,
        budget,
    )
}

/// `discovery.find_sessions_touching_file(mode=…, context_budget=N)` with an index.
///
/// The FTS content half is `mode == "any"` only — the reference gates the
/// routing on it (`if mode == "any" and search_service is not None`), so
/// `--mode read` / `--mode write` take the store path even on a populated
/// index. That asymmetry is the contract, not an oversight.
///
/// # Errors
/// When a query fails.
pub fn find_sessions_touching_file_budgeted_indexed_mode(
    conn: &Connection,
    index: Option<&crate::lexical::LexicalIndex>,
    file_path: &str,
    limit: impl Into<Limit>,
    mode: outcome::Mode,
    budget: &rank::Budget,
) -> Result<BudgetedResult> {
    let limit: Limit = limit.into();
    let fallback = |conn: &Connection| -> Result<BudgetedResult> {
        touching_file(conn, file_path, limit, mode, Some(budget))?
            .1
            .context("a budgeted touching-file query must produce a budgeted result")
    };
    let Some(index) = index.filter(|_| mode == outcome::Mode::Any) else {
        return fallback(conn);
    };
    let resolved = paths::resolve_input_path(file_path);
    match touching_file_fts(conn, index, &resolved, limit, Some(budget))? {
        Some((_, Some(budgeted))) => Ok(budgeted),
        Some((_, None)) => fallback(conn),
        None => fallback(conn),
    }
}

/// `discovery._touching_file_fts` — exact tool half on the store, free-text
/// content half through bm25, merged exact-first.
///
/// The FTS half is **gated on the exact half being thin** (fewer than `limit`
/// sessions): a well-worn file keeps its fast exact path and never pays the
/// second-database open. That gate is why `memory file` on a heavily-edited
/// file can stay on the pure-store path even with a populated index.
fn touching_file_fts(
    conn: &Connection,
    index: &crate::lexical::LexicalIndex,
    resolved: &str,
    limit: Limit,
    budget: Option<&rank::Budget>,
) -> Result<Option<(Vec<SessionMatch>, Option<BudgetedResult>)>> {
    let pattern = format!("%{resolved}%");

    // Exact tool-arg half — one store scan, refined in Rust. Note this query
    // selects `s.session_id`, not `s.id`: the FTS half speaks session ids, so
    // the union is keyed on them.
    let mut stmt = conn.prepare(
        "SELECT s.session_id AS sid, m.tools_json AS tools_json \
         FROM messages m JOIN sessions s ON s.id = m.session_fk \
         WHERE m.tools_json LIKE ?",
    )?;
    let tool_rows = stmt
        .query_map([&pattern], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, Option<String>>(1)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut tool_sids: Vec<String> = Vec::new();
    let mut tool_seen: HashSet<String> = HashSet::new();
    for (sid, tools_json) in &tool_rows {
        if tool_seen.contains(sid) {
            continue;
        }
        if outcome::tools_json_mentions_file(tools_json.as_deref(), resolved, outcome::Mode::Any) {
            tool_seen.insert(sid.clone());
            tool_sids.push(sid.clone());
        }
    }

    let mut match_kind_by_sid: HashMap<String, &'static str> =
        tool_sids.iter().map(|sid| (sid.clone(), "tool")).collect();
    let mut content_sids: Vec<String> = Vec::new();
    let mut bm25_by_sid: Vec<(String, f64)> = Vec::new();
    let mut more_by_sid: HashMap<String, i64> = HashMap::new();

    // `exact_thin = (not limit) or (limit <= 0) or (len(tool_sids) < limit)`.
    let exact_thin =
        !limit.is_positive() || i64::try_from(tool_sids.len()).unwrap_or(i64::MAX) < limit.as_i64();
    if exact_thin {
        // The content half takes no project or date filter — the reference
        // passes only the path and `candidate_k`. A `None` cap is the
        // overflowing `LIMIT ?` bind, which `_touching_content_half` swallows
        // into the same `None` an unpopulated index produces.
        let Some(candidate_k) = crate::lexical::candidate_k(limit.as_i64()) else {
            return Ok(None);
        };
        let Some(hits) = index.session_hits(resolved, None, None, candidate_k) else {
            return Ok(None); // index unpopulated → caller runs the LIKE scan
        };
        for hit in &hits {
            if hit.session_id.is_empty()
                || tool_seen.contains(&hit.session_id)
                || more_by_sid.contains_key(&hit.session_id)
            {
                continue;
            }
            content_sids.push(hit.session_id.clone());
            bm25_by_sid.push((hit.session_id.clone(), hit.bm25));
            more_by_sid.insert(hit.session_id.clone(), hit.more_matches_in_session);
            match_kind_by_sid.insert(hit.session_id.clone(), "content");
        }
    }

    let mut ordered_sids = tool_sids;
    ordered_sids.extend(content_sids);
    let bm25_relevance: HashMap<String, f64> = if bm25_by_sid.is_empty() {
        HashMap::new()
    } else {
        crate::lexical::bm25_relevance(&bm25_by_sid)
            .into_iter()
            .collect()
    };
    let relevance = relevance_touching_file_bm25(match_kind_by_sid, bm25_relevance);

    if ordered_sids.is_empty() {
        return Ok(Some(match budget {
            Some(budget) => (
                Vec::new(),
                Some(rank::budgeted(Vec::new(), budget, &relevance)),
            ),
            None => (Vec::new(), None),
        }));
    }

    let rows_by_sid = sessions_by_id(conn, &ordered_sids, None)?;
    let mut out: Vec<SessionMatch> = Vec::new();
    for sid in &ordered_sids {
        let Some(row) = rows_by_sid.get(sid) else {
            continue; // FTS/tool hit with no store provenance — skip
        };
        let mut session_match = row.clone();
        let more = more_by_sid.get(sid).copied().unwrap_or(0);
        session_match.more_matches_in_session = (more != 0).then_some(more);
        out.push(session_match);
        if limit.is_positive() && i64::try_from(out.len()).unwrap_or(i64::MAX) >= limit.as_i64() {
            break;
        }
    }

    Ok(Some(match budget {
        Some(budget) => {
            let budgeted = rank::budgeted(out, budget, &relevance);
            (budgeted.sessions.clone(), Some(budgeted))
        }
        None => (out, None),
    }))
}

/// `discovery.find_sessions_where_action_worked` with a `SearchService`.
///
/// # Errors
/// When a query fails, or `since` is malformed.
pub fn find_sessions_where_action_worked_indexed(
    conn: &Connection,
    index: Option<&crate::lexical::LexicalIndex>,
    action: &str,
    filters: &ActionWorked<'_>,
) -> Result<Vec<SessionMatch>> {
    let Some(index) = index else {
        return find_sessions_where_action_worked(conn, action, filters);
    };
    if action.trim().is_empty() {
        return Ok(Vec::new());
    }
    let clamped = filters.min_confidence.clamp(0.0, 1.0);
    let needle = action.trim();
    let since_iso = pytime::parse_since(filters.since)?;

    match action_worked_fts(conn, index, needle, filters, since_iso.as_deref(), clamped)? {
        Some(matches) => Ok(matches),
        None => find_sessions_where_action_worked(conn, action, filters),
    }
}

/// `discovery._action_worked_fts` — the tool half keeps its `MAX(seq)` anchor
/// scan; the content half is FTS-selected and its anchor resolved by a
/// `content_text LIKE` **bounded to those sessions**.
///
/// On a session both halves matched, the anchor is the *later* `seq` — exactly
/// the `MAX(seq)` over the union the single combined scan computes, so the
/// classified outcome is unchanged by the routing.
fn action_worked_fts(
    conn: &Connection,
    index: &crate::lexical::LexicalIndex,
    needle: &str,
    filters: &ActionWorked<'_>,
    since_iso: Option<&str>,
    min_confidence: f64,
) -> Result<Option<Vec<SessionMatch>>> {
    let (project, limit) = (filters.project, filters.limit);
    let Some(candidate_k) = crate::lexical::candidate_k(limit) else {
        return Ok(None); // the overflowing bind → caller runs the combined scan
    };
    let Some(hits) = index.session_hits(needle, project, since_iso, candidate_k) else {
        return Ok(None); // index unpopulated → caller runs the combined scan
    };

    // ── exact tool-arg half (unchanged: LIKE + MAX(seq) anchor) ──
    let mut where_clauses = vec!["m.tools_json LIKE ?".to_string()];
    let mut params: Vec<rusqlite::types::Value> =
        vec![rusqlite::types::Value::Text(format!("%{needle}%"))];
    if let Some(slug) = project_filter(project) {
        where_clauses.push("p.slug = ?".to_string());
        params.push(rusqlite::types::Value::Text(slug.to_owned()));
    }
    if let Some(iso) = since_iso {
        where_clauses.push("m.timestamp >= ?".to_string());
        params.push(rusqlite::types::Value::Text(iso.to_owned()));
    }
    let tool_sql = format!(
        "SELECT s.id AS sfk, MAX(m.seq) AS anchor_seq \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         JOIN projects p ON p.id = s.project_id \
         WHERE {} GROUP BY s.id",
        where_clauses.join(" AND ")
    );
    let mut stmt = conn.prepare(&tool_sql)?;
    let tool_anchors = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // A dict in Python: insertion-ordered, and `d[k] = max(...)` on an existing
    // key does not move it. A Vec plus a position index reproduces both.
    let mut anchors: Vec<(i64, i64)> = Vec::new();
    let mut position: HashMap<i64, usize> = HashMap::new();
    for (fk, seq) in tool_anchors {
        position.entry(fk).or_insert_with(|| {
            anchors.push((fk, seq));
            anchors.len() - 1
        });
    }

    // ── content half: FTS-selected sessions → bounded anchor lookup ──
    let content_sids: Vec<String> = hits
        .iter()
        .filter(|hit| !hit.session_id.is_empty())
        .map(|hit| hit.session_id.clone())
        .collect();
    let more_by_sid: HashMap<String, i64> = hits
        .iter()
        .filter(|hit| !hit.session_id.is_empty())
        .map(|hit| (hit.session_id.clone(), hit.more_matches_in_session))
        .collect();
    let mut more_by_fk: HashMap<i64, i64> = HashMap::new();

    for chunk in content_sids.chunks(500) {
        let mut clauses = vec![
            format!("s.session_id IN ({})", placeholders(chunk.len())),
            "m.content_text LIKE ?".to_string(),
        ];
        let mut chunk_params: Vec<rusqlite::types::Value> = chunk
            .iter()
            .map(|sid| rusqlite::types::Value::Text(sid.clone()))
            .collect();
        chunk_params.push(rusqlite::types::Value::Text(format!("%{needle}%")));
        if let Some(iso) = since_iso {
            clauses.push("m.timestamp >= ?".to_string());
            chunk_params.push(rusqlite::types::Value::Text(iso.to_owned()));
        }
        let sql = format!(
            "SELECT s.id AS sfk, s.session_id AS sid, MAX(m.seq) AS anchor_seq \
             FROM messages m JOIN sessions s ON s.id = m.session_fk \
             WHERE {} GROUP BY s.id",
            clauses.join(" AND ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(chunk_params.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (fk, sid, content_anchor) in rows {
            match position.get(&fk) {
                Some(index) => anchors[*index].1 = anchors[*index].1.max(content_anchor),
                None => {
                    position.insert(fk, anchors.len());
                    anchors.push((fk, content_anchor.max(-1)));
                }
            }
            more_by_fk.insert(fk, more_by_sid.get(&sid).copied().unwrap_or(0));
        }
    }

    // `--file` narrowing, after both halves are unioned — the reference does it
    // here too, so a file filter never changes which half found a session.
    narrow_to_file(
        conn,
        &mut anchors,
        filters.file_path,
        &mut Some(&mut more_by_fk),
    )?;

    Ok(Some(outcome::outcome_matches_for_clustered(
        conn,
        &anchors,
        &["worked"],
        limit,
        min_confidence,
        &more_by_fk,
    )?))
}

/// `discovery._load_message_texts` — `{message_id: content_text}`.
///
/// Chunked at 500 to stay under SQLite's 999-parameter default even though the
/// candidate set is normally a few dozen ids; a `NULL` reads as `""`, and an id
/// with no row is simply absent.
///
/// # Errors
/// When a query fails.
pub fn load_message_texts(conn: &Connection, message_ids: &[i64]) -> Result<HashMap<i64, String>> {
    let mut out: HashMap<i64, String> = HashMap::new();
    if message_ids.is_empty() {
        return Ok(out);
    }
    for chunk in message_ids.chunks(500) {
        let sql = format!(
            "SELECT id, content_text FROM messages WHERE id IN ({})",
            placeholders(chunk.len())
        );
        let params: Vec<rusqlite::types::Value> = chunk
            .iter()
            .map(|id| rusqlite::types::Value::Integer(*id))
            .collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.extend(rows);
    }
    Ok(out)
}

// ── snippets ─────────────────────────────────────────────────────────────────

/// Characters either side of the match — `discovery._SNIPPET_RADIUS`.
const SNIPPET_RADIUS: usize = 100;

/// `discovery._build_snippet` — a ~200-char excerpt around the first hit.
///
/// Python slices by **characters**, so this does too; the ellipses are U+2026.
#[must_use]
pub fn build_snippet(content: &str, query: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let chars: Vec<char> = content.chars().collect();
    let needle_len = query.chars().count();
    let excerpt: String = match find_char_index(&content.to_lowercase(), &query.to_lowercase()) {
        None => chars.iter().take(SNIPPET_RADIUS * 2).collect(),
        Some(idx) => {
            let start = idx.saturating_sub(SNIPPET_RADIUS);
            let end = chars.len().min(idx + needle_len + SNIPPET_RADIUS);
            let mut excerpt: String = chars[start..end].iter().collect();
            if start > 0 {
                excerpt.insert(0, '…');
            }
            if end < chars.len() {
                excerpt.push('…');
            }
            excerpt
        }
    };
    Some(excerpt.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// `str.find` in **character** units, as Python returns.
///
/// The lowercased haystack can differ in length from the original (`ß` → `ss`),
/// exactly as it does in Python; the reference has the same wart.
fn find_char_index(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .find(needle)
        .map(|byte_idx| haystack[..byte_idx].chars().count())
}

/// Non-overlapping occurrence count — Python's `str.count`.
fn count_occurrences(haystack: &str, needle: &str) -> i64 {
    if needle.is_empty() {
        return i64::try_from(haystack.chars().count() + 1).unwrap_or(i64::MAX);
    }
    i64::try_from(haystack.matches(needle).count()).unwrap_or(i64::MAX)
}

/// The store's canonical location, for callers that open it themselves.
///
/// # Errors
/// When the current directory cannot be read.
pub fn current_dir_string() -> Result<String> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    Ok(paths::path_to_string(&cwd))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    // ── fixtures ─────────────────────────────────────────────────────────────

    /// A scratch directory that removes itself (no `tempfile` dependency).
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before the epoch")
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("stax-queries-{}-{nanos}-{seq}", std::process::id()));
            fs::create_dir_all(&path).expect("creating the scratch directory");
            Self { path }
        }

        fn db(&self) -> PathBuf {
            self.path.join("store.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// The real schema, trimmed to the columns the discovery path reads.
    ///
    /// `messages` is a UNION-ALL view over monthly partitions because that is
    /// what schema v030 has (`store/migrations/v008_messages_partitioning.py`),
    /// and the partitioning is exactly what makes the ported SQL shapes
    /// load-bearing (§6b).
    const FIXTURE_SCHEMA: &str = "
        CREATE TABLE projects (
          id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
          path TEXT, display_name TEXT NOT NULL, first_seen REAL NOT NULL,
          last_modified REAL NOT NULL, worktree_of TEXT,
          UNIQUE (provider, slug));
        CREATE TABLE sessions (
          id INTEGER PRIMARY KEY,
          project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
          session_id TEXT NOT NULL, first_ts TEXT, last_ts TEXT,
          message_count INTEGER NOT NULL DEFAULT 0,
          UNIQUE (project_id, session_id));
        CREATE TABLE session_mart (
          session_id TEXT PRIMARY KEY, project_id INTEGER NOT NULL,
          provider TEXT NOT NULL, first_ts TEXT NOT NULL, last_ts TEXT NOT NULL,
          message_count INTEGER NOT NULL DEFAULT 0,
          cost_usd REAL NOT NULL DEFAULT 0.0);
        CREATE TABLE messages_202601 (
          id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
          timestamp TEXT NOT NULL, role TEXT NOT NULL, model TEXT,
          content_text TEXT NOT NULL DEFAULT '', tools_json TEXT NOT NULL DEFAULT '[]',
          raw_json TEXT NOT NULL DEFAULT '{}', is_sidechain INTEGER NOT NULL DEFAULT 0,
          UNIQUE (session_fk, seq));
        CREATE TABLE messages_202602 (
          id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
          timestamp TEXT NOT NULL, role TEXT NOT NULL, model TEXT,
          content_text TEXT NOT NULL DEFAULT '', tools_json TEXT NOT NULL DEFAULT '[]',
          raw_json TEXT NOT NULL DEFAULT '{}', is_sidechain INTEGER NOT NULL DEFAULT 0,
          UNIQUE (session_fk, seq));
        CREATE VIEW messages AS
          SELECT id, session_fk, seq, timestamp, role, model, content_text,
                 tools_json, raw_json, is_sidechain FROM messages_202601
          UNION ALL
          SELECT id, session_fk, seq, timestamp, role, model, content_text,
                 tools_json, raw_json, is_sidechain FROM messages_202602;
        CREATE INDEX idx_messages_202601_session_seq ON messages_202601(session_fk, seq);
        CREATE INDEX idx_messages_202602_session_seq ON messages_202602(session_fk, seq);
        CREATE INDEX idx_sessions_last_ts ON sessions(last_ts);
    ";

    fn open_fixture(scratch: &Scratch) -> Connection {
        let conn = Connection::open(scratch.db()).expect("creating the fixture store");
        conn.execute_batch(FIXTURE_SCHEMA)
            .expect("applying the fixture schema");
        conn.pragma_update(None, "user_version", 30_i64)
            .expect("stamping user_version");
        conn
    }

    /// Two projects, three sessions, a handful of messages — enough to exercise
    /// every wave-1 read path including the empty-`content_text` blind spot.
    fn seed(conn: &Connection) {
        conn.execute_batch(
            "
            INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified)
            VALUES (1, 'claude', '-home-dev-alpha', NULL, 'alpha', 0, 0),
                   (2, 'codex',  '-home-dev-beta',  NULL, 'beta',  0, 0);

            INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count)
            VALUES (1, 1, 'aaaaaaaa-1111-4111-8111-111111111111',
                    '2026-01-02T09:00:00+00:00', '2026-01-02T10:00:00+00:00', 6),
                   (2, 1, 'bbbbbbbb-2222-4222-8222-222222222222',
                    '2026-01-03T09:00:00+00:00', '2026-01-03T10:00:00+00:00', 4),
                   (3, 2, 'cccccccc-3333-4333-8333-333333333333',
                    '2026-02-01T09:00:00+00:00', '2026-02-01T10:00:00+00:00', 2);

            INSERT INTO session_mart (session_id, project_id, provider, first_ts, last_ts,
                                      message_count, cost_usd)
            VALUES ('aaaaaaaa-1111-4111-8111-111111111111', 1, 'claude',
                    '2026-01-02T09:00:00+00:00', '2026-01-02T10:00:00+00:00', 6, 1.25),
                   ('cccccccc-3333-4333-8333-333333333333', 2, 'codex',
                    '2026-02-01T09:00:00+00:00', '2026-02-01T10:00:00+00:00', 2, 0.5);
            ",
        )
        .expect("seeding projects and sessions");

        // Session 1: an Edit of alpha/main.py the user then says broke.
        conn.execute_batch(
            r#"
            INSERT INTO messages_202601
              (id, session_fk, seq, timestamp, role, content_text, tools_json)
            VALUES
              (1, 1, 1, '2026-01-02T09:10:00+00:00', 'user',
               'we should cache the watermark lookup', '[]'),
              (2, 1, 2, '2026-01-02T09:20:00+00:00', 'assistant', '',
               '[{"name": "Edit", "input": {"file_path": "/home/dev/alpha/main.py"}}]'),
              (3, 1, 3, '2026-01-02T09:30:00+00:00', 'user',
               'that broke the build', '[]'),
              (4, 1, 4, '2026-01-02T09:40:00+00:00', 'assistant', 'sorry, reverting', '[]');

            INSERT INTO messages_202601
              (id, session_fk, seq, timestamp, role, content_text, tools_json)
            VALUES
              (5, 2, 1, '2026-01-03T09:10:00+00:00', 'user',
               'add the watermark cache to the reader', '[]'),
              (6, 2, 2, '2026-01-03T09:20:00+00:00', 'assistant', '',
               '[{"name": "Write", "input": {"file_path": "/home/dev/alpha/reader.py"}}]'),
              (7, 2, 3, '2026-01-03T09:30:00+00:00', 'user', 'that worked, thanks', '[]');

            INSERT INTO messages_202602
              (id, session_fk, seq, timestamp, role, content_text, tools_json)
            VALUES
              (8, 3, 1, '2026-02-01T09:10:00+00:00', 'user',
               'unrelated beta work on the watermark', '[]'),
              (9, 3, 2, '2026-02-01T09:20:00+00:00', 'assistant', 'done', '[]');
            "#,
        )
        .expect("seeding messages");

        // `/home` can be a symlink/firmlink on macOS. Store the same canonical
        // spelling the query path resolver uses, rather than Linux-only paths.
        for (id, path) in [(1, "/home/dev/alpha"), (2, "/home/dev/beta")] {
            conn.execute(
                "UPDATE projects SET path=?1 WHERE id=?2",
                rusqlite::params![paths::resolve_input_path(path), id],
            )
            .expect("canonical fixture project path");
        }
        conn.execute(
            "UPDATE messages_202601 SET tools_json=replace(tools_json,'/home/dev/alpha',?1)",
            [paths::resolve_input_path("/home/dev/alpha")],
        )
        .expect("canonical fixture tool paths");
    }

    /// A budget with the default weights and a pinned clock — 2026-07-31T00:00Z.
    fn budget(tokens: i64) -> rank::Budget {
        rank::Budget::at(tokens, rank::DEFAULT_RANK_WEIGHTS, 1_785_456_000.0)
    }

    fn seeded() -> (Scratch, Connection) {
        let scratch = Scratch::new();
        let conn = open_fixture(&scratch);
        seed(&conn);
        (scratch, conn)
    }

    // ── pyjson ───────────────────────────────────────────────────────────────

    #[test]
    fn float_repr_matches_cpython() {
        // Each expectation is what `repr(x)` prints in CPython 3.12.
        for (value, expected) in [
            (0.0_f64, "0.0"),
            (-0.0_f64, "-0.0"),
            (1.0, "1.0"),
            (0.5, "0.5"),
            (1.25, "1.25"),
            (0.4269, "0.4269"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (6.3e-5, "6.3e-05"),
            (-2.5, "-2.5"),
            (123.456, "123.456"),
        ] {
            assert_eq!(pyjson::repr_float(value), expected, "repr({value})");
        }
    }

    /// DIV-008's case table — the drift alarm for the two copies of
    /// `tie_break_to_even`.
    ///
    /// `(bit pattern, CPython repr)`, every expectation produced by
    /// `../StackUnderflow/.venv/bin/python -c 'print(repr(x))'` on the reference
    /// interpreter. The same table lives in the other crate; if only one copy of
    /// the algorithm is edited, one of the two fails.
    ///
    /// Four groups, in order:
    ///
    /// 1. **Ties CPython breaks downward** — the divergence itself. Rust's
    ///    shortest writer rounds these up; the double is exactly `…25`, so both
    ///    spellings parse back to it and only a byte comparison can see it.
    /// 2. **Ties CPython breaks upward** — the same tie, where the even digit
    ///    happens to be the higher one and the two writers already agreed. Pins
    ///    that the fix did not simply invert the rounding.
    /// 3. **Binade floors** — an exact midpoint that must *not* be taken. At the
    ///    bottom of a binade the gap below the double is half the gap above, so
    ///    the lower candidate is equidistant in decimal yet parses to the previous
    ///    double. `2^-24` is the live case: CPython prints `…063`, and `…062` is a
    ///    different float.
    /// 4. **Odd last digit, no tie** — the fast path must not misfire on the
    ///    ordinary values that reach it constantly.
    const DIV008_CASES: &[(u64, &str)] = &[
        // 1. CPython rounds the tie down to the even digit; `{:e}` rounds up.
        (0xc313_36cd_97cc_33f5, "-1352070300110077.2"),
        (0x430e_1c6d_958d_7b72, "1059438285926254.2"),
        (0xc314_41f2_33f6_3165, "-1425502010969177.2"),
        (0x42b7_fa57_c450_e950, "26363981746409.312"),
        (0x4319_7469_53fb_86ad, "1791217536786859.2"),
        (0xc2d8_c0fe_9119_c088, "-108868734838530.12"),
        (0xc2bf_0dc9_05b0_7310, "-34144067629171.062"),
        // 2. CPython rounds the tie up to the even digit; both writers agreed already.
        (0xc310_c44c_b563_4c1f, "-1179858341778183.8"),
        (0xc2ed_1a52_01a8_fb4c, "-255991057565658.38"),
        (0x430d_bf9a_9de0_10e6, "1046680639898140.8"),
        (0xc31b_4e7d_6d57_492b, "-1921531245875786.8"),
        (0x4304_b413_2881_dc6e, "728436738898829.8"),
        (0xc31c_dda2_f0f2_6be3, "-2031247811189496.8"),
        // 3. Binade floors: the even-looking neighbour is a different double.
        (0x3e70_0000_0000_0000, "5.960464477539063e-08"),
        (0x3e60_0000_0000_0000, "2.9802322387695312e-08"),
        (0x3e10_0000_0000_0000, "9.313225746154785e-10"),
        (0x3ca0_0000_0000_0000, "1.1102230246251565e-16"),
        (0x0010_0000_0000_0000, "2.2250738585072014e-308"),
        (0x0000_0000_0000_0001, "5e-324"),
        (0x43e0_0000_0000_0000, "9.223372036854776e+18"),
        (0x4330_0000_0000_0000, "4503599627370496.0"),
        (0x4340_0000_0000_0000, "9007199254740992.0"),
        // 4. Odd final digit, no tie anywhere near it.
        (0x3fb9_9999_9999_999a, "0.1"),
        (0x4009_21fb_5444_2d18, "3.141592653589793"),
        (0x4005_bf0a_8b14_5769, "2.718281828459045"),
        (0xc02e_0000_0000_0000, "-15.0"),
        (0x3f84_7ae1_47ae_147b, "0.01"),
    ];

    /// DIV-008: halfway ties render exactly as CPython's `repr` does.
    #[test]
    fn halfway_ties_break_to_even_like_cpython() {
        for (bits, want) in DIV008_CASES {
            let value = f64::from_bits(*bits);
            assert_eq!(&pyjson::repr_float(value), want, "repr({bits:#018x})");
        }
    }

    /// Both spellings of a tie parse to the same double, so a round-trip test
    /// cannot see the fix — only these bytes can. Guards against anyone
    /// "simplifying" the table away into a parse check.
    #[test]
    fn the_tie_repair_is_invisible_to_round_tripping() {
        for (bits, want) in DIV008_CASES {
            assert_eq!(
                want.parse::<f64>()
                    .expect("CPython prints a parseable float"),
                f64::from_bits(*bits),
                "{bits:#018x} must round-trip whatever the spelling"
            );
        }
    }

    #[test]
    fn strings_are_escaped_the_way_ensure_ascii_does() {
        // `json.dumps("… ok\n\"q\" \\ 🎉")` in CPython, byte for byte: the BMP
        // ellipsis becomes one escape, the astral party popper a surrogate pair.
        let value = pyjson::Value::Str("… ok\n\"q\" \\ 🎉".into());
        assert_eq!(
            pyjson::dumps_compact(&value),
            r#""\u2026 ok\n\"q\" \\ \ud83c\udf89""#
        );
    }

    #[test]
    fn indent_two_matches_json_dumps() {
        let value = pyjson::Value::Object(vec![
            ("schema".into(), pyjson::Value::from("staxtrace.memory/1")),
            ("results".into(), pyjson::Value::Array(vec![])),
            (
                "query".into(),
                pyjson::Value::Object(vec![
                    ("limit".into(), pyjson::Value::Int(20)),
                    ("project".into(), pyjson::Value::Null),
                ]),
            ),
            ("truncated".into(), pyjson::Value::Bool(false)),
        ]);
        assert_eq!(
            pyjson::dumps_indent2(&value),
            "{\n  \"schema\": \"staxtrace.memory/1\",\n  \"results\": [],\n  \
             \"query\": {\n    \"limit\": 20,\n    \"project\": null\n  },\n  \
             \"truncated\": false\n}"
        );
    }

    #[test]
    fn default_separators_carry_the_spaces() {
        let value = pyjson::Value::Object(vec![
            ("name".into(), pyjson::Value::from("Edit")),
            (
                "input".into(),
                pyjson::Value::Object(vec![("file_path".into(), pyjson::Value::from("/tmp/x.py"))]),
            ),
        ]);
        assert_eq!(
            pyjson::dumps_default(&value),
            r#"{"name": "Edit", "input": {"file_path": "/tmp/x.py"}}"#
        );
    }

    #[test]
    fn json_round_trips_the_shapes_tools_json_uses() {
        let parsed = pyjson::loads(
            r#"[{"name": "Bash", "input": {"command": "git reset --hard"}, "n": 3, "f": 1.5}]"#,
        )
        .expect("valid JSON");
        let pyjson::Value::Array(items) = &parsed else {
            panic!("expected an array");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].get("input").and_then(|v| v.get("command")),
            Some(&pyjson::Value::from("git reset --hard"))
        );
        assert_eq!(items[0].get("n"), Some(&pyjson::Value::Int(3)));
        assert_eq!(items[0].get("f"), Some(&pyjson::Value::Float(1.5)));
        assert!(pyjson::loads("{bad json").is_none());
        assert!(pyjson::loads("[1, 2] trailing").is_none());
    }

    #[test]
    fn python_truthiness_drives_the_arg_fallback_chain() {
        assert!(!pyjson::Value::Object(vec![]).is_truthy());
        assert!(!pyjson::Value::Str(String::new()).is_truthy());
        assert!(!pyjson::Value::Int(0).is_truthy());
        assert!(pyjson::Value::Int(1).is_truthy());
    }

    // ── pytime ───────────────────────────────────────────────────────────────

    #[test]
    fn iso_timestamps_parse_the_way_fromisoformat_does() {
        let base = pytime::parse_iso("2026-01-02T00:00:00+00:00").expect("parses");
        assert!((base - 1_767_312_000.0).abs() < 1e-6, "{base}");
        assert_eq!(pytime::parse_iso("2026-01-02"), Some(base));
        assert_eq!(pytime::parse_iso("2026-01-02T00:00:00Z"), Some(base));
        assert_eq!(pytime::parse_iso("2026-01-02T00:00:00"), Some(base));
        assert_eq!(
            pytime::parse_iso("2026-01-02T02:00:00+02:00"),
            Some(base),
            "an offset is applied, not ignored"
        );
        let fractional = pytime::parse_iso("2026-01-02T00:00:00.500000+00:00").expect("parses");
        assert!((fractional - base - 0.5).abs() < 1e-9);
        assert_eq!(pytime::parse_iso("not a date"), None);
    }

    #[test]
    fn isoformat_matches_datetime_isoformat() {
        assert_eq!(
            pytime::isoformat_utc(1_767_312_000_000_000),
            "2026-01-02T00:00:00+00:00"
        );
        assert_eq!(
            pytime::isoformat_utc(1_767_312_000_123_456),
            "2026-01-02T00:00:00.123456+00:00"
        );
    }

    #[test]
    fn relative_since_windows_match_the_reference() {
        let now = 1_767_312_000_000_000; // 2026-01-02T00:00:00Z
        assert_eq!(
            pytime::parse_since_at(Some("7d"), now).expect("valid"),
            Some("2025-12-26T00:00:00+00:00".to_string())
        );
        assert_eq!(
            pytime::parse_since_at(Some("24h"), now).expect("valid"),
            Some("2026-01-01T00:00:00+00:00".to_string())
        );
        assert_eq!(
            pytime::parse_since_at(Some(" 1 W "), now).expect("valid"),
            Some("2025-12-26T00:00:00+00:00".to_string())
        );
        // "1m" is 30 days, not a calendar month — ported as-is.
        assert_eq!(
            pytime::parse_since_at(Some("1m"), now).expect("valid"),
            Some("2025-12-03T00:00:00+00:00".to_string())
        );
        assert_eq!(pytime::parse_since_at(None, now).expect("valid"), None);
        assert_eq!(
            pytime::parse_since_at(Some("  "), now).expect("valid"),
            None
        );
        assert_eq!(
            pytime::parse_since_at(Some("2026-01-01"), now).expect("valid"),
            Some("2026-01-01T00:00:00+00:00".to_string())
        );
        let error = pytime::parse_since_at(Some("yesterday"), now).expect_err("rejected");
        assert_eq!(
            error.to_string(),
            "Invalid since value 'yesterday': expected '7d'/'1w'/'1m'/'24h' \
             or an ISO date/datetime."
        );
    }

    /// The B-8 fix. `datetime.fromisoformat` keeps the literal's own `tzinfo`
    /// and `isoformat()` prints it back, and the SQL comparison against
    /// `messages.timestamp` is a **string** comparison — so re-rendering an
    /// offset in UTC changes which rows come back, with exit 0 on both sides.
    /// Measured against the live store: `--since 2026-07-30T00:00:00+05:30`
    /// selected 0 sessions in Python and 1 here before this.
    #[test]
    fn a_since_offset_survives_verbatim_because_sql_compares_the_string() {
        let now = 1_767_312_000_000_000;
        let since = |value: &str| {
            pytime::parse_since_at(Some(value), now)
                .expect("valid")
                .expect("a value")
        };
        assert_eq!(
            since("2026-07-30T00:00:00+05:30"),
            "2026-07-30T00:00:00+05:30",
            "NOT 2026-07-29T18:30:00+00:00 — that string selects different rows"
        );
        assert_eq!(
            since("2026-07-20T00:00:00-07:00"),
            "2026-07-20T00:00:00-07:00"
        );
        // `+HHMM` is normalised to `+HH:MM`, `Z` and `-00:00` both to `+00:00`,
        // and a naive value is stamped UTC — all four are `isoformat()`'s doing.
        assert_eq!(
            since("2026-07-30T00:00:00+0530"),
            "2026-07-30T00:00:00+05:30"
        );
        assert_eq!(since("2026-07-30T00:00:00Z"), "2026-07-30T00:00:00+00:00");
        assert_eq!(
            since("2026-07-30T00:00:00-00:00"),
            "2026-07-30T00:00:00+00:00"
        );
        assert_eq!(since("2026-07-30T00:00:00"), "2026-07-30T00:00:00+00:00");
        // Sub-minute offsets and microseconds widen the rendering, exactly as
        // `timezone.__str__` does.
        assert_eq!(
            since("2026-07-30T00:00:00+05:30:30"),
            "2026-07-30T00:00:00+05:30:30"
        );
        assert_eq!(
            since("2026-07-30T00:00:00.5+05:30"),
            "2026-07-30T00:00:00.500000+05:30"
        );
    }

    /// `re`'s `\d` is Unicode, so `--since '٧d'` is seven days — Click exits 0
    /// where an `is_ascii_digit` scan fell through to the ISO leg and exited 2.
    #[test]
    fn a_relative_window_accepts_unicode_decimal_digits() {
        let now = 1_767_312_000_000_000;
        let seven_days = Some("2025-12-26T00:00:00+00:00".to_string());
        assert_eq!(
            pytime::parse_since_at(Some("\u{667}d"), now).expect("valid"),
            seven_days,
            "U+0667 ARABIC-INDIC DIGIT SEVEN"
        );
        assert_eq!(
            pytime::parse_since_at(Some("\u{966}\u{967}d"), now).expect("valid"),
            Some("2026-01-01T00:00:00+00:00".to_string()),
            "Devanagari 0 then 1 reads as 01 — one day"
        );
        // `\s` is `str.isspace()`'s, which is wider than the set `int()` strips.
        assert_eq!(
            pytime::parse_since_at(Some("\u{1c}7d\u{1c}"), now).expect("valid"),
            seven_days
        );
    }

    /// `timedelta` is bounded and raises `OverflowError`, which is **not** a
    /// `ValueError` — so Click does not catch it and the `--since` parameter
    /// error is the wrong shape for it.
    #[test]
    fn an_out_of_range_relative_window_is_not_a_value_error() {
        let now = 1_767_312_000_000_000;
        let error = pytime::parse_since_at(Some("99999999999d"), now).expect_err("rejected");
        assert_eq!(error.to_string(), "date value out of range");
        assert_eq!(ValueError::of(&error), None, "not catchable by the CLI");

        let caught = pytime::parse_since_at(Some("yesterday"), now).expect_err("rejected");
        assert!(ValueError::of(&caught).is_some(), "this one IS catchable");
    }

    // ── pyint ────────────────────────────────────────────────────────────────

    /// Every expectation here is what CPython 3.12's `int()` returns for the
    /// same literal, checked against the reference interpreter.
    #[test]
    fn int_parses_what_cpython_int_parses() {
        let parse = |raw: &str| pyint::PyInt::parse(raw).map(|value| value.to_string());
        for (raw, expected) in [
            ("5", "5"),
            (" 5", "5"),
            ("5 ", "5"),
            ("  5  ", "5"),
            ("\u{a0}5", "5"),   // NO-BREAK SPACE
            ("\u{3000}5", "5"), // IDEOGRAPHIC SPACE
            ("+5", "5"),
            ("-1", "-1"),
            ("1_000", "1000"),
            ("\u{667}", "7"), // ARABIC-INDIC SEVEN
            ("-\u{667}", "-7"),
            ("\u{661}_\u{662}", "12"),
            ("1\u{660}", "10"), // scripts may be mixed, digit by digit
            ("\u{660}\u{660}5", "5"),
            ("007", "7"),
            ("-0", "0"), // CPython has no negative zero
            ("99999999999999999999", "99999999999999999999"),
        ] {
            assert_eq!(parse(raw).as_deref(), Some(expected), "int({raw:?})");
        }
        for raw in [
            "",
            " ",
            "+",
            "-",
            "0x10",
            "5.0",
            "_1",
            "1_",
            "1__0",
            "+_1",
            "abc",
            "--5",
            "5-",
            "\u{1c}5",   // isspace() but NOT stripped by int()
            "\u{200b}5", // ZERO WIDTH SPACE is not whitespace at all
            "\u{bd}",    // VULGAR FRACTION ONE HALF is numeric, not decimal
        ] {
            assert_eq!(parse(raw), None, "int({raw:?}) must raise");
        }
    }

    #[test]
    fn an_oversized_int_saturates_for_arithmetic_and_says_it_did() {
        let huge = pyint::PyInt::parse("99999999999999999999").expect("parses");
        assert_eq!(huge.fits_i64(), None);
        assert_eq!(huge.fits_u64(), None);
        assert_eq!(huge.saturating_i64(), i64::MAX);
        assert!(huge.is_positive());

        // i64::MAX + 1 still fits a u64, which is what the `--limit` echo uses.
        let past_i64 = pyint::PyInt::parse("9223372036854775808").expect("parses");
        assert_eq!(past_i64.fits_i64(), None);
        assert_eq!(past_i64.fits_u64(), Some(9_223_372_036_854_775_808));

        let tiny = pyint::PyInt::parse("-99999999999999999999").expect("parses");
        assert_eq!(tiny.saturating_i64(), i64::MIN);
        assert!(!tiny.is_positive());

        assert_eq!(pyint::PyInt::from(-7_i64).to_string(), "-7");
        assert_eq!(pyint::PyInt::from(i64::MIN).fits_i64(), Some(i64::MIN));
    }

    /// The overflow travels with the value and fires at the bind, because that
    /// is where `sqlite3` raises it — `memory decisions --limit 10**20` exits 0
    /// (the cap is compared in Python) while `memory sessions` on the same
    /// limit exits 1 (the cap is bound into `LIMIT ?`).
    #[test]
    fn an_oversized_limit_only_fails_where_python_binds_it() {
        let plain: Limit = 20_i64.into();
        assert_eq!(plain.as_i64(), 20);
        assert!(plain.is_positive());
        assert_eq!(plain.bind().expect("binds"), 20);

        let huge: Limit = (&pyint::PyInt::parse("99999999999999999999").expect("parses")).into();
        assert_eq!(huge.as_i64(), i64::MAX, "the cap comparison still works");
        assert!(huge.is_positive());
        assert_eq!(
            huge.bind().expect_err("sqlite3 refuses it").to_string(),
            "Python int too large to convert to SQLite INTEGER"
        );

        // A negative limit never reaches a bind at all (`if limit and limit > 0`).
        let negative: Limit = (&pyint::PyInt::parse("-1").expect("parses")).into();
        assert!(!negative.is_positive());
    }

    #[test]
    fn an_oversized_limit_is_the_stores_error_not_the_since_error() {
        let (_scratch, conn) = seeded();
        let huge = pyint::PyInt::parse("99999999999999999999").expect("parses");
        let error = find_sessions_in_path(&conn, "/home/dev/alpha", None, &huge, None, &budget(0))
            .expect_err("the bind overflows");
        assert!(
            ValueError::of(&error).is_none(),
            "an OverflowError is not catchable by `except ValueError`"
        );
        // The same limit through a query that never binds it is fine.
        let ok = search_past_decisions(&conn, "watermark", None, None, i64::MAX, &budget(0))
            .expect("no bind, no overflow");
        assert_eq!(ok.sessions.len(), 3);
    }

    /// `--project ''` is not `None` (so no cwd fallback) but *is* falsy (so no
    /// `WHERE` clause). Filtering on `p.slug = ''` returns nothing instead, and
    /// both sides exit 0 — a silently different answer.
    #[test]
    fn an_empty_project_slug_scopes_to_every_project() {
        assert_eq!(project_filter(Some("")), None);
        assert_eq!(
            project_filter(Some("-home-dev-alpha")),
            Some("-home-dev-alpha")
        );
        assert_eq!(project_filter(None), None);

        let (_scratch, conn) = seeded();
        let all = search_past_decisions(&conn, "watermark", Some(""), None, 20, &budget(0))
            .expect("the query runs");
        let unscoped = search_past_decisions(&conn, "watermark", None, None, 20, &budget(0))
            .expect("the query runs");
        let scoped = search_past_decisions(
            &conn,
            "watermark",
            Some("-home-dev-alpha"),
            None,
            20,
            &budget(0),
        )
        .expect("the query runs");
        assert_eq!(all.sessions.len(), 3, "'' searches every project");
        assert_eq!(all.sessions.len(), unscoped.sessions.len());
        assert_eq!(scoped.sessions.len(), 2, "a real slug still narrows");

        let worked = find_sessions_where_action_worked(
            &conn,
            "watermark",
            &ActionWorked::new(Some(""), None, 20, 0.5),
        )
        .expect("the query runs");
        assert!(!worked.is_empty(), "`worked` reads '' the same way");
    }

    /// A store-level failure must stay a store-level failure. Before the marker
    /// existed the CLI reported a corrupt store as
    /// `Invalid value for --since: database disk image is malformed` and exited
    /// 2, on an invocation that never passed `--since`.
    #[test]
    fn a_database_failure_is_not_a_value_error() {
        let (_scratch, conn) = seeded();
        let error = search_past_decisions(&conn, "x", None, Some("yesterday"), 20, &budget(0))
            .expect_err("a bad --since");
        assert!(ValueError::of(&error).is_some());

        conn.execute_batch("DROP TABLE messages_202601; DROP TABLE messages_202602;")
            .expect("breaking the store");
        let error = search_past_decisions(&conn, "watermark", None, None, 20, &budget(0))
            .expect_err("the scan cannot run");
        assert_eq!(
            ValueError::of(&error),
            None,
            "sqlite3.DatabaseError propagates past `except ValueError`"
        );
    }

    // ── paths ────────────────────────────────────────────────────────────────

    #[test]
    fn slug_decoding_is_the_lossy_reference_decode() {
        assert_eq!(
            paths::decode_slug_to_path("-Users-foo-dev-proj"),
            "/Users/foo/dev/proj"
        );
        assert_eq!(paths::decode_slug_to_path("workspace-id"), "");
        assert_eq!(paths::decode_slug_to_path(""), "");
    }

    #[test]
    fn ancestry_is_pure_string_arithmetic() {
        assert!(paths::is_ancestor("/a/b", "/a/b"));
        assert!(paths::is_ancestor("/a/b/", "/a/b/c"));
        assert!(paths::is_ancestor("\\a\\b", "/a/b/c"));
        assert!(!paths::is_ancestor("/a/b", "/a/bc"));
        assert!(!paths::is_ancestor("", "/a"));
    }

    #[test]
    fn purepath_normalisation_matches_pathlib() {
        assert_eq!(paths::purepath_str("foo/"), "foo");
        assert_eq!(paths::purepath_str("a//b"), "a/b");
        assert_eq!(paths::purepath_str("a/./b"), "a/b");
        assert_eq!(paths::purepath_str("a/../b"), "a/../b");
        assert_eq!(paths::purepath_str(""), ".");
        assert_eq!(paths::purepath_str("."), ".");
        assert_eq!(paths::purepath_str("/"), "/");
    }

    #[test]
    fn tilde_expansion_stops_at_tilde_user() {
        let home = PathBuf::from("/home/tester");
        assert_eq!(paths::expanduser("~", Some(&home)), "/home/tester");
        assert_eq!(paths::expanduser("~/x", Some(&home)), "/home/tester/x");
        assert_eq!(paths::expanduser("~other/x", Some(&home)), "~other/x");
        assert_eq!(paths::expanduser("/abs", Some(&home)), "/abs");
    }

    #[test]
    fn relative_paths_resolve_against_the_cwd() {
        let scratch = Scratch::new();
        let cwd = scratch.path.clone();
        let resolved = paths::resolve_input_path_with("sub/file.py", None, &cwd);
        let canonical = fs::canonicalize(&cwd).expect("scratch exists");
        assert_eq!(
            resolved,
            format!("{}/sub/file.py", canonical.to_string_lossy())
        );
        assert_eq!(
            paths::resolve_input_path_with("", None, &cwd),
            canonical.to_string_lossy()
        );
        assert_eq!(
            paths::resolve_input_path_with("a/../b", None, &cwd),
            format!("{}/b", canonical.to_string_lossy())
        );
    }

    #[test]
    fn repr_quotes_like_python() {
        assert_eq!(paths::py_repr("cache"), "'cache'");
        assert_eq!(paths::py_repr("it's"), "\"it's\"");
        assert_eq!(paths::py_repr("it's \"q\""), "'it\\'s \"q\"'");
        assert_eq!(paths::py_repr("a\nb"), "'a\\nb'");
    }

    // ── ranking ──────────────────────────────────────────────────────────────

    fn sample(session_id: &str, last_ts: &str, cost: f64) -> SessionMatch {
        SessionMatch {
            session_id: session_id.into(),
            project_slug: "-home-dev-alpha".into(),
            project_path: "/home/dev/alpha".into(),
            provider: "claude".into(),
            first_ts: last_ts.into(),
            last_ts: last_ts.into(),
            message_count: 3,
            cost_usd: cost,
            snippet: None,
            embedding_score: None,
            more_matches_in_session: None,
            outcome: None,
        }
    }

    #[test]
    fn token_estimate_is_the_compact_json_length_over_four() {
        let row = sample("s1", "2026-01-02T10:00:00+00:00", 1.25);
        let serialized = pyjson::dumps_compact(&row.to_dict());
        assert_eq!(
            rank::estimate_tokens(&row.to_dict()),
            i64::try_from(serialized.len() / 4).unwrap() + 1
        );
    }

    #[test]
    fn packing_is_greedy_and_strict() {
        let rows = vec![
            sample("s1", "2026-01-02T10:00:00+00:00", 1.0),
            sample("s2", "2026-01-02T10:00:00+00:00", 1.0),
            sample("s3", "2026-01-02T10:00:00+00:00", 1.0),
        ];
        let per_row = rank::estimate_tokens(&rows[0].to_dict());
        let (kept, dropped, used) = rank::pack_within_budget(rows.clone(), per_row * 2, None);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, 1);
        assert_eq!(used, per_row * 2);

        // A budget under the first row's cost keeps nothing.
        let (kept, dropped, used) = rank::pack_within_budget(rows.clone(), 1, None);
        assert!(kept.is_empty());
        assert_eq!(dropped, 3);
        assert_eq!(used, 0);

        // `<= 0` disables enforcement entirely.
        let (kept, dropped, _) = rank::pack_within_budget(rows, 0, None);
        assert_eq!(kept.len(), 3);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn equal_ranks_keep_their_input_order() {
        let rows = vec![
            sample("first", "2026-01-02T10:00:00+00:00", 1.0),
            sample("second", "2026-01-02T10:00:00+00:00", 1.0),
            sample("third", "2026-01-02T10:00:00+00:00", 1.0),
        ];
        let relevance: rank::Relevance = Box::new(|_| 1.0);
        let (kept, _, _) = rank::pack_within_budget(
            rows,
            0,
            Some((&relevance, rank::DEFAULT_RANK_WEIGHTS, 1_767_312_000.0)),
        );
        let ids: Vec<&str> = kept.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(ids, ["first", "second", "third"]);
    }

    #[test]
    fn rank_weights_fall_back_on_anything_odd() {
        assert_eq!(rank::parse_rank_weights(None), (0.5, 0.2, 0.3));
        assert_eq!(rank::parse_rank_weights(Some("")), (0.5, 0.2, 0.3));
        assert_eq!(rank::parse_rank_weights(Some("1,2")), (0.5, 0.2, 0.3));
        assert_eq!(rank::parse_rank_weights(Some("1,-2,3")), (0.5, 0.2, 0.3));
        assert_eq!(rank::parse_rank_weights(Some("x,y,z")), (0.5, 0.2, 0.3));
        assert_eq!(
            rank::parse_rank_weights(Some("0.4, 0.1, 0.5, 0.9")),
            (0.4, 0.1, 0.5)
        );
    }

    #[test]
    fn cost_saturates_at_five_dollars() {
        assert!((rank::cost_score(&sample("s", "", 10.0)) - 1.0).abs() < 1e-12);
        assert!((rank::cost_score(&sample("s", "", 2.5)) - 0.5).abs() < 1e-12);
        assert!((rank::cost_score(&sample("s", "", -1.0)) - 0.0).abs() < 1e-12);
    }

    // ── outcome heuristics ───────────────────────────────────────────────────

    #[test]
    fn user_text_classification_keeps_the_reference_precedence() {
        assert_eq!(classify("revert that, thanks"), Some("revert"));
        assert_eq!(classify("that broke the build"), Some("negative"));
        assert_eq!(classify("that worked, thanks"), Some("positive"));
        assert_eq!(classify("no"), Some("negative"));
        assert_eq!(classify("no problem, keep going"), None);
        assert_eq!(classify("no worries — but it broke"), Some("negative"));
        assert_eq!(classify("another node in the notes"), None);
        assert_eq!(classify("👍"), Some("positive"));
        assert_eq!(classify(""), None);
    }

    fn classify(text: &str) -> Option<&'static str> {
        outcome::classify_user_text(text)
    }

    #[test]
    fn inline_trimming_matches_the_reference() {
        assert_eq!(outcome::trim_inline("  a \n b  ", 100), "a b");
        assert_eq!(outcome::trim_inline("abcdef", 4), "abc…");
    }

    #[test]
    fn tool_arg_matching_honours_the_mode() {
        let tools = r#"[{"name": "Edit", "input": {"file_path": "/a/b/main.py"}}]"#;
        assert!(outcome::tools_json_mentions_file(
            Some(tools),
            "/a/b/main.py",
            outcome::Mode::Write
        ));
        assert!(outcome::tools_json_mentions_file(
            Some(tools),
            "/a/b/main.py",
            outcome::Mode::Any
        ));
        assert!(!outcome::tools_json_mentions_file(
            Some(tools),
            "/a/b/main.py",
            outcome::Mode::Read
        ));
        assert!(!outcome::tools_json_mentions_file(
            Some("[]"),
            "/a/b/main.py",
            outcome::Mode::Any
        ));
        assert!(!outcome::tools_json_mentions_file(
            Some("{not json"),
            "/a/b/main.py",
            outcome::Mode::Any
        ));
        // The last-ditch check serialises the whole entry with Python's default
        // separators, so a path anywhere in the args still matches.
        let loose = r#"[{"name": "Read", "input": {"pattern": "/a/b/main.py:12"}}]"#;
        assert!(outcome::tools_json_mentions_file(
            Some(loose),
            "/a/b/main.py",
            outcome::Mode::Read
        ));
    }

    #[test]
    fn revert_commands_are_spotted_in_shell_tool_calls() {
        assert_eq!(
            outcome::revert_command_in_tools(Some(
                r#"[{"name": "Bash", "input": {"command": "git   reset --hard HEAD~1"}}]"#
            ))
            .as_deref(),
            Some("git   reset --hard HEAD~1")
        );
        assert_eq!(
            outcome::revert_command_in_tools(Some(
                r#"[{"name": "Bash", "input": {"command": "pytest -q"}}]"#
            )),
            None
        );
        assert_eq!(outcome::revert_command_in_tools(Some("[]")), None);
    }

    fn message(
        id: i64,
        role: &str,
        text: &str,
        tools: &str,
        sidechain: bool,
    ) -> outcome::MessageRow {
        outcome::MessageRow {
            id,
            role: role.into(),
            content_text: text.into(),
            tools_json: Some(tools.into()),
            is_sidechain: sidechain,
        }
    }

    #[test]
    fn outcome_confidence_follows_the_reference_ladder() {
        let anchor_only = vec![message(1, "assistant", "", "[]", false)];
        let fields = outcome::classify_outcome(&anchor_only, 0);
        assert_eq!(fields.outcome, "uncertain");
        assert!((fields.outcome_confidence - 0.0).abs() < 1e-12);

        let explicit = vec![
            message(1, "assistant", "", "[]", false),
            message(2, "user", "that worked, thanks", "[]", false),
        ];
        let fields = outcome::classify_outcome(&explicit, 0);
        assert_eq!(fields.outcome, "worked");
        assert_eq!(fields.outcome_evidence, "user wrote: 'that worked, thanks'");
        assert!((fields.outcome_confidence - 0.8).abs() < 1e-12);

        let tool_revert = vec![
            message(1, "assistant", "", "[]", false),
            message(
                2,
                "assistant",
                "",
                r#"[{"name": "Bash", "input": {"command": "git revert abc"}}]"#,
                false,
            ),
        ];
        let fields = outcome::classify_outcome(&tool_revert, 0);
        assert_eq!(fields.outcome, "reverted");
        assert_eq!(
            fields.outcome_evidence,
            "agent ran `git revert abc` after the action"
        );
        assert!((fields.outcome_confidence - 0.5).abs() < 1e-12);

        let silence = vec![
            message(1, "assistant", "", "[]", false),
            message(2, "assistant", "still working", "[]", false),
        ];
        let fields = outcome::classify_outcome(&silence, 0);
        assert_eq!(fields.outcome, "worked");
        assert!((fields.outcome_confidence - 0.3).abs() < 1e-12);
        assert_eq!(fields.outcome_msg_id, 2);

        // Sidechain rows never speak for the parent session.
        let sidechain_only = vec![
            message(1, "assistant", "", "[]", false),
            message(2, "user", "that broke", "[]", true),
        ];
        let fields = outcome::classify_outcome(&sidechain_only, 0);
        assert_eq!(fields.outcome, "uncertain");
        assert_eq!(fields.outcome_msg_id, 1);
    }

    // ── the queries, against the fixture store ───────────────────────────────

    #[test]
    fn sessions_in_path_matches_ancestor_projects_only() {
        let (_scratch, conn) = seeded();
        let result =
            find_sessions_in_path(&conn, "/home/dev/alpha/src", None, 20, None, &budget(0))
                .expect("query runs");
        let mut ids: Vec<&str> = result
            .sessions
            .iter()
            .map(|m| m.session_id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            [
                "aaaaaaaa-1111-4111-8111-111111111111",
                "bbbbbbbb-2222-4222-8222-222222222222"
            ],
            "the beta project is not an ancestor of this path"
        );
        assert_eq!(
            result
                .sessions
                .iter()
                .find(|m| m.session_id.starts_with("aaaa"))
                .expect("session a")
                .cost_usd,
            1.25
        );
        assert_eq!(
            result
                .sessions
                .iter()
                .find(|m| m.session_id.starts_with("bbbb"))
                .expect("session b")
                .cost_usd,
            0.0,
            "no mart row → 0.0"
        );
        assert_eq!(
            result.sessions[0].project_path,
            paths::resolve_input_path("/home/dev/alpha")
        );
        assert!(!result.truncated);
    }

    #[test]
    fn the_budget_path_returns_rank_order_not_sql_order() {
        // The SQL is `last_ts DESC`, but every `memory` verb passes a budget and
        // `_budgeted` re-sorts by rank — so the newest session is not
        // necessarily first. Here the older session outranks the newer one on
        // the cost term (0.2 × 1.25/5 = 0.05) by more than a day of recency is
        // worth at this distance, exactly as the reference orders it.
        let (_scratch, conn) = seeded();
        let result = find_sessions_in_path(&conn, "/home/dev/alpha", None, 20, None, &budget(2000))
            .expect("query runs");
        let ids: Vec<&str> = result
            .sessions
            .iter()
            .map(|m| m.session_id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "aaaaaaaa-1111-4111-8111-111111111111",
                "bbbbbbbb-2222-4222-8222-222222222222"
            ]
        );
        assert!(result.budget_used_tokens > 0);
        assert_eq!(result.budget_max_tokens, 2000);
    }

    #[test]
    fn sessions_in_path_returns_nothing_for_an_unknown_tree() {
        let (_scratch, conn) = seeded();
        let result = find_sessions_in_path(&conn, "/somewhere/else", None, 20, None, &budget(0))
            .expect("query runs");
        assert!(result.sessions.is_empty());
        assert_eq!(result.more_available, 0);
    }

    #[test]
    fn sessions_in_path_honours_provider_since_and_limit() {
        let (_scratch, conn) = seeded();
        let filtered = find_sessions_in_path(&conn, "/home/dev/alpha", None, 1, None, &budget(0))
            .expect("query runs")
            .sessions;
        assert_eq!(filtered.len(), 1);

        let by_provider = find_sessions_in_path(
            &conn,
            "/home/dev/alpha",
            None,
            20,
            Some("codex"),
            &budget(0),
        )
        .expect("query runs")
        .sessions;
        assert!(by_provider.is_empty(), "alpha is a claude project");

        let since = find_sessions_in_path(
            &conn,
            "/home/dev/alpha",
            Some("2026-01-03T00:00:00+00:00"),
            20,
            None,
            &budget(0),
        )
        .expect("query runs")
        .sessions;
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].session_id, "bbbbbbbb-2222-4222-8222-222222222222");
    }

    #[test]
    fn decisions_snippets_and_clustering_match_the_like_path() {
        let (_scratch, conn) = seeded();
        let result = search_past_decisions(&conn, "watermark", None, None, 20, &budget(0))
            .expect("query runs");
        let mut ids: Vec<&str> = result
            .sessions
            .iter()
            .map(|m| m.session_id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            [
                "aaaaaaaa-1111-4111-8111-111111111111",
                "bbbbbbbb-2222-4222-8222-222222222222",
                "cccccccc-3333-4333-8333-333333333333"
            ]
        );
        assert_eq!(
            result
                .sessions
                .iter()
                .find(|m| m.session_id.starts_with("aaaa"))
                .expect("session a")
                .snippet
                .as_deref(),
            Some("we should cache the watermark lookup")
        );
        assert!(
            result
                .sessions
                .iter()
                .all(|m| m.more_matches_in_session.is_none()),
            "one hit per session → the clustering field stays absent"
        );
    }

    #[test]
    fn decisions_scope_to_a_project_slug() {
        let (_scratch, conn) = seeded();
        let scoped = search_past_decisions(
            &conn,
            "watermark",
            Some("-home-dev-beta"),
            None,
            20,
            &budget(0),
        )
        .expect("query runs");
        assert_eq!(scoped.sessions.len(), 1);
        assert_eq!(scoped.sessions[0].project_slug, "-home-dev-beta");
    }

    #[test]
    fn multi_word_phrases_silently_zero_on_the_like_path() {
        // Findings ledger #3: `LIKE '%a b%'` is a literal substring test, so a
        // phrase whose words are adjacent matches and one whose words are not
        // returns nothing at all. This is the wave-1 contract, bug and all.
        let (_scratch, conn) = seeded();
        let adjacent = search_past_decisions(&conn, "watermark lookup", None, None, 20, &budget(0))
            .expect("runs");
        assert_eq!(adjacent.sessions.len(), 1);
        let scattered =
            search_past_decisions(&conn, "cache lookup", None, None, 20, &budget(0)).expect("runs");
        assert!(
            scattered.sessions.is_empty(),
            "both words are present, in that order, but not adjacent"
        );
    }

    #[test]
    fn tool_call_turns_are_invisible_to_the_decisions_scan() {
        // Findings ledger #1: `content_text` is empty on tool-call turns, so the
        // file an Edit touched cannot be found by searching message content.
        let (_scratch, conn) = seeded();
        let result =
            search_past_decisions(&conn, "/home/dev/alpha/main.py", None, None, 20, &budget(0))
                .expect("query runs");
        assert!(result.sessions.is_empty());
        // …while the tool-arg scan does find it.
        let touching =
            find_sessions_touching_file(&conn, "/home/dev/alpha/main.py", 20).expect("query runs");
        assert_eq!(touching.len(), 1);
    }

    #[test]
    fn empty_queries_short_circuit_before_the_scan() {
        let (_scratch, conn) = seeded();
        let result =
            search_past_decisions(&conn, "   ", None, None, 20, &budget(0)).expect("query runs");
        assert!(result.sessions.is_empty());
        assert_eq!(
            find_sessions_where_action_worked(&conn, "  ", &ActionWorked::new(None, None, 20, 0.5))
                .expect("query runs")
                .len(),
            0
        );
    }

    #[test]
    fn action_worked_requires_an_explicit_confirmation() {
        let (_scratch, conn) = seeded();
        let worked = find_sessions_where_action_worked(
            &conn,
            "watermark",
            &ActionWorked::new(None, None, 20, 0.5),
        )
        .expect("query runs");
        let ids: Vec<&str> = worked.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["bbbbbbbb-2222-4222-8222-222222222222"],
            "session 1 was told it broke; session 3's anchor is the last turn"
        );
        let fields = worked[0].outcome.as_ref().expect("an outcome match");
        assert_eq!(fields.outcome, "worked");
        assert_eq!(fields.outcome_evidence, "user wrote: 'that worked, thanks'");
        assert!((fields.outcome_confidence - 0.8).abs() < 1e-12);
    }

    #[test]
    fn a_lower_confidence_floor_lets_the_silence_rows_back_in() {
        let (_scratch, conn) = seeded();
        let worked = find_sessions_where_action_worked(
            &conn,
            "unrelated",
            &ActionWorked::new(None, None, 20, 0.3),
        )
        .expect("query runs");
        assert_eq!(worked.len(), 1, "session 3 continued without a complaint");
        assert!(
            (worked[0]
                .outcome
                .as_ref()
                .expect("outcome")
                .outcome_confidence
                - 0.3)
                .abs()
                < 1e-12
        );
    }

    /// `find-sessions-where-action-worked --file` — the intersection is loose
    /// (any `tools_json` **or** `content_text` mention), and it runs *after*
    /// the candidate scan, so it can only remove sessions.
    ///
    /// The live store cannot prove this: its `tools_json` is names-only
    /// (`["Read"]`), so no tool-arg matcher ever fires there. This fixture has
    /// real arguments, which is where the branch is actually exercised.
    #[test]
    fn the_file_narrowing_intersects_and_never_adds() {
        let (_scratch, conn) = seeded();
        let unfiltered = find_sessions_where_action_worked(
            &conn,
            "watermark",
            &ActionWorked::new(None, None, 20, 0.5),
        )
        .expect("query runs");
        assert_eq!(unfiltered.len(), 1);

        let matching = find_sessions_where_action_worked(
            &conn,
            "watermark",
            &ActionWorked {
                file_path: Some("/home/dev/alpha/reader.py"),
                ..ActionWorked::new(None, None, 20, 0.5)
            },
        )
        .expect("query runs");
        assert_eq!(matching.len(), 1, "session 2 wrote reader.py");

        let elsewhere = find_sessions_where_action_worked(
            &conn,
            "watermark",
            &ActionWorked {
                file_path: Some("/home/dev/alpha/main.py"),
                ..ActionWorked::new(None, None, 20, 0.5)
            },
        )
        .expect("query runs");
        assert!(
            elsewhere.is_empty(),
            "session 2 never mentions main.py, so the narrowing drops it"
        );

        // An empty `--file` is falsy in Python and filters nothing.
        assert_eq!(
            find_sessions_where_action_worked(
                &conn,
                "watermark",
                &ActionWorked {
                    file_path: Some(""),
                    ..ActionWorked::new(None, None, 20, 0.5)
                },
            )
            .expect("query runs")
            .len(),
            1
        );
    }

    /// `find-sessions-touching-file --mode read|write` narrows to the tool-arg
    /// half only: the SQL drops the `content_text` clause and the refinement
    /// asks the mode-specific matcher, so a prose mention no longer counts.
    #[test]
    fn the_touching_file_modes_split_tool_args_from_prose() {
        let (_scratch, conn) = seeded();
        let ids = |mode: outcome::Mode, path: &str| -> Vec<String> {
            find_sessions_touching_file_budgeted_mode(&conn, path, 20_i64, mode, &budget(0))
                .expect("query runs")
                .sessions
                .into_iter()
                .map(|row| row.session_id)
                .collect()
        };
        // main.py is an `Edit` arg in session 1 — write and any see it, read does not.
        assert_eq!(
            ids(outcome::Mode::Write, "/home/dev/alpha/main.py").len(),
            1
        );
        assert_eq!(ids(outcome::Mode::Any, "/home/dev/alpha/main.py").len(), 1);
        assert!(ids(outcome::Mode::Read, "/home/dev/alpha/main.py").is_empty());
        // reader.py is a `Write` arg in session 2.
        assert_eq!(
            ids(outcome::Mode::Write, "/home/dev/alpha/reader.py").len(),
            1
        );
        assert!(ids(outcome::Mode::Read, "/home/dev/alpha/reader.py").is_empty());
    }

    #[test]
    fn failure_modes_anchor_on_the_last_write_mention() {
        let (_scratch, conn) = seeded();
        let failures = find_failure_modes_for_file(&conn, "/home/dev/alpha/main.py", None, 20, 0.5)
            .expect("query runs");
        assert_eq!(failures.len(), 1);
        let fields = failures[0].outcome.as_ref().expect("an outcome match");
        assert_eq!(fields.outcome, "failed");
        assert_eq!(
            fields.outcome_evidence,
            "user wrote: 'that broke the build'"
        );
    }

    #[test]
    fn risk_counts_the_three_slices_and_the_recent_ids() {
        let (_scratch, conn) = seeded();
        let risk =
            file_risk_summary(&conn, "/home/dev/alpha/main.py", None, 5).expect("query runs");
        assert_eq!(
            risk.path,
            paths::resolve_input_path("/home/dev/alpha/main.py")
        );
        assert_eq!(risk.total_sessions, 1);
        assert_eq!(risk.failed, 1);
        assert_eq!(risk.reverted, 0);
        assert_eq!(risk.worked, 0);
        assert_eq!(
            risk.recent_session_ids,
            ["aaaaaaaa-1111-4111-8111-111111111111"]
        );

        let clean = file_risk_summary(&conn, "/home/dev/alpha/reader.py", None, 5).expect("runs");
        assert_eq!(clean.worked, 1);
        assert_eq!(clean.failed, 0);
        assert!(clean.recent_session_ids.is_empty());
    }

    #[test]
    fn risk_echoes_the_raw_since_string_not_the_parsed_one() {
        let (_scratch, conn) = seeded();
        let risk = file_risk_summary(&conn, "/home/dev/alpha/main.py", Some("30d"), 5)
            .expect("query runs");
        assert_eq!(risk.since.as_deref(), Some("30d"));
    }

    #[test]
    fn cwd_scoping_prefers_the_longest_slug_match() {
        let (_scratch, conn) = seeded();
        conn.execute("UPDATE projects SET path=NULL", []).unwrap();
        assert_eq!(
            detect_cwd_project_slug(&conn, "/home/dev/alpha/src/deep"),
            Some("-home-dev-alpha".to_string())
        );
        assert_eq!(
            detect_cwd_project_slug(&conn, "/home/dev/beta"),
            Some("-home-dev-beta".to_string())
        );
        // The ledgered behavior: a cwd no slug covers scopes to nothing, and the
        // caller then queries every project.
        assert_eq!(detect_cwd_project_slug(&conn, "/opt/elsewhere"), None);
    }

    #[test]
    fn cwd_scoping_prefers_the_stored_path_when_one_exists() {
        let (_scratch, conn) = seeded();
        conn.execute(
            "UPDATE projects SET path = '/mnt/data/alpha' WHERE id = 1",
            [],
        )
        .expect("setting a stored path");
        assert_eq!(
            detect_cwd_project_slug(&conn, "/mnt/data/alpha/src"),
            Some("-home-dev-alpha".to_string())
        );
        assert_eq!(detect_cwd_project_slug(&conn, "/home/dev/alpha"), None);
    }

    #[test]
    fn snippets_are_bounded_and_whitespace_collapsed() {
        let content = format!("{}NEEDLE{}", "a".repeat(300), "b".repeat(300));
        let snippet = build_snippet(&content, "needle").expect("a snippet");
        assert!(snippet.starts_with('…') && snippet.ends_with('…'));
        assert_eq!(snippet.chars().count(), 100 + 6 + 100 + 2);
        assert_eq!(
            build_snippet("  a\n\nb  ", "zzz").as_deref(),
            Some("a b"),
            "a miss falls back to the leading slice"
        );
        assert_eq!(build_snippet("", "x"), None);
    }

    #[test]
    fn session_rows_serialise_in_the_dataclass_field_order() {
        let mut row = sample("s1", "2026-01-02T10:00:00+00:00", 1.25);
        row.snippet = Some("hit".into());
        row.more_matches_in_session = Some(2);
        row.outcome = Some(OutcomeFields {
            outcome: "worked".into(),
            outcome_evidence: "user wrote: 'thanks'".into(),
            outcome_msg_id: 42,
            outcome_confidence: 0.8,
        });
        assert_eq!(
            pyjson::dumps_compact(&row.to_dict()),
            r#"{"session_id":"s1","project_slug":"-home-dev-alpha","project_path":"/home/dev/alpha","provider":"claude","first_ts":"2026-01-02T10:00:00+00:00","last_ts":"2026-01-02T10:00:00+00:00","message_count":3,"cost_usd":1.25,"snippet":"hit","more_matches_in_session":2,"outcome":"worked","outcome_evidence":"user wrote: 'thanks'","outcome_msg_id":42,"outcome_confidence":0.8}"#
        );
    }

    #[test]
    fn absent_optional_fields_keep_the_nine_key_shape() {
        let row = sample("s1", "2026-01-02T10:00:00+00:00", 0.0);
        assert_eq!(
            pyjson::dumps_compact(&row.to_dict()),
            r#"{"session_id":"s1","project_slug":"-home-dev-alpha","project_path":"/home/dev/alpha","provider":"claude","first_ts":"2026-01-02T10:00:00+00:00","last_ts":"2026-01-02T10:00:00+00:00","message_count":3,"cost_usd":0.0,"snippet":null}"#
        );
    }

    #[test]
    fn the_store_is_never_written_by_a_read_path() {
        let (_scratch, conn) = seeded();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("counting");
        let _ = find_sessions_in_path(&conn, "/home/dev/alpha", None, 20, None, &budget(2000));
        let _ = search_past_decisions(&conn, "watermark", None, None, 20, &budget(2000));
        let _ = find_sessions_where_action_worked(
            &conn,
            "watermark",
            &ActionWorked::new(None, None, 20, 0.5),
        );
        let _ = file_risk_summary(&conn, "/home/dev/alpha/main.py", None, 5);
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("counting");
        assert_eq!(before, after);
        // The telemetry table the reference bumps does not even exist here —
        // the port drops that write (documented divergence).
        let telemetry: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'discovery_telemetry'",
                [],
                |row| row.get(0),
            )
            .expect("counting");
        assert_eq!(telemetry, 0);
    }
}
