//! Artifact readers for D1 — the four formats agent configs actually come in.
//!
//! * JSON, with the comment lines and trailing commas real files carry
//!   (Copilot CLI writes a `// User …` line above its JSON): strict first,
//!   then a comment-stripping pass, so a file the vendor's own parser reads is
//!   a file the audit reads.
//! * TOML, through the `toml` crate the workspace graph already links. The
//!   first build hand-rolled a `[section]` + `key = value` reader that could
//!   not see dotted keys (`analytics.enabled = false`), inline tables, quoted
//!   keys (`[projects."/Users/x"]`) or arrays of tables
//!   (`[[marketplace.sources]]`) — all present in the real Codex and Grok
//!   configs on the maintainer's machine — so a veto that WAS set audited as
//!   at-risk and a fully vetoed grok config audited as unreadable. A
//!   wrong-direction posture is the one thing a security tool may not emit.
//! * dotenv (`KEY=VALUE`, `export` allowed) — values stay strings.
//! * flat YAML (`key: value` at column 0, nothing nested) — the gh-copilot
//!   `config.yml` shape. Anything nested is an error, never a guess.
//!
//! Anything a reader cannot understand is an `Err`; the scanner turns that
//! into posture `unknown` — degraded never means silent (§8.3).

use anyhow::{Result, bail};
use serde_json::{Map, Value};

/// Read an artifact into a JSON value tree according to its declared format.
pub fn read_artifact(text: &str, format: crate::Format) -> Result<Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match format {
        crate::Format::Json => json(text),
        crate::Format::Toml => toml(text),
        crate::Format::Env => dotenv(text),
        crate::Format::YamlFlat => yaml_flat(text),
    }
}

/// Walk a dotted key path (`telemetry.trace_upload`) through objects.
pub fn lookup<'v>(root: &'v Value, dotted: &str) -> Option<&'v Value> {
    let mut node = root;
    for part in dotted.split('.') {
        node = node.as_object()?.get(part)?;
    }
    Some(node)
}

/// Strict JSON, then JSON-with-comments. The strict error is the one reported
/// when both fail — it names the real problem, not the stripped copy's.
fn json(text: &str) -> Result<Value> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(strict) => {
            serde_json::from_str(&strip_jsonc(text)).map_err(|_| anyhow::anyhow!("JSON: {strict}"))
        }
    }
}

/// Drop `//` and `/* … */` comments outside strings, then trailing commas
/// before a closing bracket. String contents (URLs with `//`) are untouched.
fn strip_jsonc(text: &str) -> String {
    let mut without_comments = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(ch) = chars.next() {
        if in_string {
            without_comments.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    without_comments.push(escaped);
                }
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                without_comments.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        without_comments.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            _ => without_comments.push(ch),
        }
    }

    let chars: Vec<char> = without_comments.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if in_string {
            out.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
        } else if ch == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Full TOML. The tree is re-expressed as JSON values so every check compares
/// against one value model; a TOML datetime becomes a string-shaped leaf no
/// check will ever look up.
fn toml(text: &str) -> Result<Value> {
    let parsed: toml::Value = text.parse().map_err(|e| anyhow::anyhow!("TOML: {e}"))?;
    Ok(serde_json::to_value(parsed)?)
}

/// `KEY=VALUE` per line; `export ` prefixes and `#` comments allowed; values
/// stay strings (signatures list the string forms they mean).
fn dotenv(text: &str) -> Result<Value> {
    let mut map = Map::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = strip_hash_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(&line).trim();
        let Some((key, value)) = line.split_once('=') else {
            bail!("line {}: expected KEY=VALUE", idx + 1);
        };
        map.insert(key.trim().to_string(), Value::String(unquote(value.trim())));
    }
    Ok(Value::Object(map))
}

/// `key: value` at column 0 and nothing else. Indentation, lists and empty
/// values all mean structure this reader does not model — so they are
/// errors, and the check degrades to `unknown` rather than guessing.
fn yaml_flat(text: &str) -> Result<Value> {
    let mut map = Map::new();
    for (idx, raw) in text.lines().enumerate() {
        let n = idx + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed == "..." {
            continue;
        }
        if raw.starts_with([' ', '\t']) {
            bail!("line {n}: nested YAML is beyond the flat reader");
        }
        if trimmed == "-" || trimmed.starts_with("- ") {
            bail!("line {n}: YAML lists are beyond the flat reader");
        }
        let line = strip_yaml_comment(raw).trim_end();
        let Some((key, value)) = line.split_once(':') else {
            bail!("line {n}: expected `key: value`");
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            bail!("line {n}: empty key");
        }
        if value.is_empty() {
            bail!("line {n}: `{key}:` opens a block — beyond the flat reader");
        }
        map.insert(key.to_string(), yaml_scalar(value));
    }
    Ok(Value::Object(map))
}

/// An unquoted `#` starts a comment.
fn strip_hash_comment(line: &str) -> &str {
    let mut in_str: Option<char> = None;
    for (i, ch) in line.char_indices() {
        match (ch, in_str) {
            ('"' | '\'', None) => in_str = Some(ch),
            (q, Some(open)) if q == open => in_str = None,
            ('#', None) => return &line[..i],
            _ => {}
        }
    }
    line
}

/// YAML's rule: `#` starts a comment only at line start or after whitespace.
fn strip_yaml_comment(line: &str) -> &str {
    let mut in_str: Option<char> = None;
    let mut prev = ' ';
    for (i, ch) in line.char_indices() {
        match (ch, in_str) {
            ('"' | '\'', None) => in_str = Some(ch),
            (q, Some(open)) if q == open => in_str = None,
            ('#', None) if prev.is_whitespace() => return &line[..i],
            _ => {}
        }
        prev = ch;
    }
    line
}

/// YAML 1.1 scalars, the dialect Go's yaml.v2 (gh's) speaks: `yes`/`no`/
/// `on`/`off` are booleans, `~` is null.
fn yaml_scalar(s: &str) -> Value {
    match s {
        "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => {
            return Value::Bool(true);
        }
        "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => {
            return Value::Bool(false);
        }
        "null" | "Null" | "NULL" | "~" => return Value::Null,
        _ => {}
    }
    scalar(s)
}

fn scalar(s: &str) -> Value {
    match s {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(n) = s.parse::<i64>() {
        return Value::from(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::from(f);
    }
    Value::String(unquote(s))
}

fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_sections_scalars_comments() {
        let v = toml("# c\n[features]\ntelemetry = true # on\n[a.b]\nn = 3\ns = \"x\"\n").unwrap();
        assert_eq!(lookup(&v, "features.telemetry"), Some(&Value::Bool(true)));
        assert_eq!(lookup(&v, "a.b.n"), Some(&Value::from(3)));
        assert_eq!(lookup(&v, "a.b.s"), Some(&Value::String("x".into())));
    }

    #[test]
    fn toml_reads_dotted_keys_inline_tables_and_quoted_sections() {
        // Every one of these is valid, common TOML the first reader could not
        // see — and each carried a veto that was then reported as absent.
        let v = toml(
            "analytics.enabled = false\notel = { log_user_prompt = false }\n[projects.\"/Users/x/repo\"]\ntrust_level = \"trusted\"\n",
        )
        .unwrap();
        assert_eq!(lookup(&v, "analytics.enabled"), Some(&Value::Bool(false)));
        assert_eq!(
            lookup(&v, "otel.log_user_prompt"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            v["projects"]["/Users/x/repo"]["trust_level"],
            Value::String("trusted".into())
        );
    }

    #[test]
    fn toml_reads_the_real_grok_shape_with_arrays_of_tables() {
        // The maintainer's actual ~/.grok/config.toml carries a
        // [[marketplace.sources]] block; the first reader refused the whole
        // file and every grok veto on that machine audited as "unreadable".
        let v = toml(
            "[cli]\nversion = 1\n[[marketplace.sources]]\nname = \"official\"\n[features]\ntelemetry = false\n[telemetry]\ntrace_upload = false\ndisable_codebase_upload = true\n",
        )
        .unwrap();
        assert_eq!(
            lookup(&v, "telemetry.trace_upload"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            lookup(&v, "telemetry.disable_codebase_upload"),
            Some(&Value::Bool(true))
        );
        assert_eq!(lookup(&v, "features.telemetry"), Some(&Value::Bool(false)));
    }

    #[test]
    fn toml_rejects_what_is_not_toml() {
        assert!(toml("key value\n").is_err());
        assert!(toml("[unclosed\n").is_err());
    }

    #[test]
    fn json_accepts_comment_lines_and_trailing_commas() {
        // Copilot CLI's ~/.copilot/config.json opens with a `// User …` line.
        let v = json("// User configuration for Copilot CLI\n{\n  \"firstLaunchAt\": \"2026-07-17\", /* block */\n  \"url\": \"https://x/y//z\",\n}\n").unwrap();
        assert_eq!(v["firstLaunchAt"], Value::String("2026-07-17".into()));
        assert_eq!(v["url"], Value::String("https://x/y//z".into()));
    }

    #[test]
    fn json_reports_the_strict_error_when_both_passes_fail() {
        let err = json("{ this is not json").unwrap_err().to_string();
        assert!(err.starts_with("JSON:"), "{err}");
    }

    #[test]
    fn a_byte_order_mark_is_not_content() {
        let v = read_artifact("\u{feff}{\"a\": 1}", crate::Format::Json).unwrap();
        assert_eq!(v["a"], Value::from(1));
    }

    #[test]
    fn yaml_flat_reads_gh_copilot_and_refuses_structure() {
        let v = yaml_flat("# gh-copilot\noptional_analytics: true\ntheme: \"dark\" # trailing\n")
            .unwrap();
        assert_eq!(lookup(&v, "optional_analytics"), Some(&Value::Bool(true)));
        assert_eq!(lookup(&v, "theme"), Some(&Value::String("dark".into())));
        assert_eq!(
            yaml_flat("optional_analytics: no\n").unwrap()["optional_analytics"],
            Value::Bool(false)
        );
        assert!(
            yaml_flat("a:\n  b: 1\n").is_err(),
            "nesting is beyond the flat reader"
        );
        assert!(
            yaml_flat("- item\n").is_err(),
            "lists are beyond the flat reader"
        );
        assert!(yaml_flat("just text\n").is_err());
    }

    #[test]
    fn dotenv_reads_exports_and_quotes() {
        let v = dotenv("# c\nexport A=1\nB=\"two\"\n").unwrap();
        assert_eq!(lookup(&v, "A"), Some(&Value::String("1".into())));
        assert_eq!(lookup(&v, "B"), Some(&Value::String("two".into())));
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        let v = toml("[s]\nurl = \"http://x/#y\"\n").unwrap();
        assert_eq!(
            lookup(&v, "s.url"),
            Some(&Value::String("http://x/#y".into()))
        );
        let v = dotenv("URL=\"http://x/#y\"\n").unwrap();
        assert_eq!(
            lookup(&v, "URL"),
            Some(&Value::String("http://x/#y".into()))
        );
    }
}
