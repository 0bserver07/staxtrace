//! Port of `project.py`'s second pass — `_refresh_message_dims` and the two
//! counters it drives. This is the hard half of the mart layer.
//!
//! `usage_events` is assistant-only (the normalizers skip non-billable rows),
//! so the token/cost totals in `project_mart` cannot see user turns, tool-result
//! turns, or commands. The Overview's User / Assistant / Tool-Use / Tool-Results
//! cards, the Commands KPI, and the v023 cache / interruption / error rates all
//! need those, so twelve extra columns are materialised straight off the
//! project's `messages.raw_json` — running the **same** classifier + enricher +
//! `aggregator._command_analysis` the full pipeline runs, which is why the
//! counts equal `get_project_stats` (the Python suite's equivalence tests pin
//! it).
//!
//! # This is where DIV-002 lands in a column
//!
//! [`count_message_dims`] classifies with
//! [`crate::stats::classifier::determine_kind`], whose fall-through sends every
//! unplaceable entry to `"assistant"`. On the maintainer's store that is 5,656
//! legacy-history user turns counted as assistant messages, and it is *also*
//! why 57 of 243 events-backed rows report `total_commands = 0`. Cent-exact
//! mart parity means reproducing both numbers exactly (§6b divergence 2), so
//! nothing here rounds the corner off.
//!
//! # One parse, two counters
//!
//! Python parses each `raw_json` twice — once inside `_count_message_dims`,
//! once inside `_count_interaction_dims`. This port parses once and feeds both,
//! which is observationally identical: the only mutation Python makes
//! (`payload["timestamp"] = r["timestamp"]`, in the second counter) happens
//! after the first has finished, and neither `_determine_kind` nor `_text_from`
//! reads `timestamp`. The order is preserved here for the same reason.

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

use crate::stats::classifier::{INTERRUPT_API, INTERRUPT_PREFIX, RawEntry, determine_kind, tag};
use crate::stats::command_analysis::command_analysis;
use crate::stats::enricher::{build, has_result_block_of, text_from, tools_from_raw};
use crate::stats::pytext::py_json_dumps_counter;

/// The v022 message-type + command dims.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MessageDims {
    /// `overview.message_types["user"]`.
    pub user: i64,
    /// `overview.message_types["assistant"]`.
    pub assistant: i64,
    /// Assistant records carrying `tool_use` blocks.
    pub tool_use: i64,
    /// Records carrying a `tool_result` block.
    pub tool_result: i64,
    /// `user_interactions.user_commands_analyzed`.
    pub commands: i64,
}

/// The v023 Overview rate numerators.
#[derive(Debug, Default, Clone)]
pub struct InteractionDims {
    /// `len(EnrichedDataset.records)` — the all-kinds record count `errors.rate`
    /// divides by. Distinct from `total_messages`, which is billable events.
    pub records: i64,
    /// `_CacheCollector.w_read` — the `cache.hit_rate` numerator.
    pub cache_read_messages: i64,
    /// `_ErrorsCollector._total`.
    pub errors_total: i64,
    /// `json.dumps(dict(_ErrorsCollector.by_category))`, CPython separators.
    pub errors_by_category_json: String,
    /// `_command_analysis["commands_followed_by_interruption"]`.
    pub commands_followed_by_interruption: i64,
    /// `_command_analysis["total_tools_used"]`.
    pub command_tools: i64,
    /// `_command_analysis["total_assistant_steps"]`.
    pub command_steps: i64,
}

/// One `messages` row as the dims pass sees it.
pub struct DimRow {
    /// `m.raw_json`.
    pub raw_json: Option<String>,
    /// `s.session_id`.
    pub session_id: Option<String>,
    /// `m.timestamp`.
    pub timestamp: Option<String>,
    /// `p.provider`.
    pub provider: Option<String>,
}

/// `project._refresh_message_dims` — recompute and store the dims for each id.
///
/// Callers pass only ids they just wrote a row for (the events pass or the
/// coverage seed), so the UPDATE always matches; an id that somehow has no row
/// is a silent no-op rather than an error.
pub fn refresh_message_dims(conn: &Connection, project_ids: &[i64]) -> Result<()> {
    // The query shape is §6b territory and is ported literally, `ORDER BY
    // m.timestamp` included — the interaction grouping downstream is a stable
    // sort over exactly this sequence.
    let mut scan = conn.prepare(
        "SELECT m.raw_json AS raw_json, s.session_id AS session_id, \
         \x20      m.timestamp AS timestamp, p.provider AS provider \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         JOIN projects p ON p.id = s.project_id \
         WHERE s.project_id = ? \
         ORDER BY m.timestamp",
    )?;
    let mut update = conn.prepare(
        "UPDATE project_mart SET \
         total_user_messages = ?, \
         total_assistant_messages = ?, \
         total_tool_use_messages = ?, \
         total_tool_result_messages = ?, \
         total_commands = ?, \
         total_records = ?, \
         total_errors = ?, \
         errors_by_category = ?, \
         total_cache_read_messages = ?, \
         total_commands_followed_by_interruption = ?, \
         total_command_tools = ?, \
         total_command_steps = ? \
         WHERE project_id = ?",
    )?;

    for pid in project_ids {
        let rows: Vec<DimRow> = scan
            .query_map([pid], |r| {
                Ok(DimRow {
                    raw_json: r.get::<_, Option<String>>("raw_json")?,
                    session_id: r.get::<_, Option<String>>("session_id")?,
                    timestamp: r.get::<_, Option<String>>("timestamp")?,
                    provider: r.get::<_, Option<String>>("provider")?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let (dims, rate) = count_dims(&rows);
        update.execute(rusqlite::params![
            dims.user,
            dims.assistant,
            dims.tool_use,
            dims.tool_result,
            dims.commands,
            rate.records,
            rate.errors_total,
            rate.errors_by_category_json,
            rate.cache_read_messages,
            rate.commands_followed_by_interruption,
            rate.command_tools,
            rate.command_steps,
            pid,
        ])?;
    }
    Ok(())
}

/// Both counters over one shared parse of the project's rows.
///
/// See the module docs for why sharing the parse is safe.
#[must_use]
pub fn count_dims(rows: &[DimRow]) -> (MessageDims, InteractionDims) {
    // Parse once. Both Python counters open with the identical guard, so one
    // filter serves both:
    //
    //     try:
    //         payload = json.loads(rj) if rj else {}
    //     except (json.JSONDecodeError, TypeError, ValueError):
    //         continue
    //     if not isinstance(payload, dict):
    //         continue
    //
    // The `if rj else {}` is not a convenience — a NULL or empty `raw_json` is
    // **processed as an empty dict**, not skipped, and an empty dict has no
    // `type` and no `message.role`, so DIV-002's fall-through files it as an
    // *assistant message*. It then also counts as an assistant step of whatever
    // command precedes it. A port that treats "nothing to parse" as "nothing to
    // count" reads the same and moves four columns.
    let parsed: Vec<(&DimRow, Value)> = rows
        .iter()
        .filter_map(|r| {
            let v = match r.raw_json.as_deref() {
                None | Some("") => Value::Object(serde_json::Map::new()),
                Some(text) => super::json::loads(Some(text))?,
            };
            v.is_object().then_some((r, v))
        })
        .collect();

    let dims = count_message_dims(parsed.iter().map(|(_, v)| v));
    let rate = count_interaction_dims(parsed);
    (dims, rate)
}

/// `project._count_message_dims`.
///
/// * `user` / `assistant` are the classifier's kind counts.
/// * `tool_use` counts assistant records carrying `tool_use` blocks.
/// * `tool_result` counts records carrying a `tool_result` block — *any* kind,
///   which is why it is tallied before the kind branch.
/// * `commands` is a real user turn: kind `user`, not a tool_result, not an
///   interruption. The same rule `command_day_mart` uses, which is what makes
///   the two tables agree.
#[must_use]
pub fn count_message_dims<'a>(payloads: impl Iterator<Item = &'a Value>) -> MessageDims {
    let mut d = MessageDims::default();
    for payload in payloads {
        let kind = determine_kind(payload);
        let has_tool_result = has_result_block_of(payload);
        if has_tool_result {
            d.tool_result += 1;
        }
        if kind == "user" {
            d.user += 1;
            if !has_tool_result {
                let text = text_from(payload);
                if !(text.starts_with(INTERRUPT_PREFIX) || text.starts_with(INTERRUPT_API)) {
                    d.commands += 1;
                }
            }
        } else if kind == "assistant" {
            d.assistant += 1;
            if !tools_from_raw(payload).is_empty() {
                d.tool_use += 1;
            }
        }
    }
    d
}

/// `project._count_interaction_dims`.
///
/// Rebuilds the project's `EnrichedDataset` the same way
/// `queries.build_enriched_dataset` does — the clean column timestamp wins over
/// any timestamp in the raw payload — then reads the numerators straight off
/// the records and `aggregator._command_analysis`.
#[must_use]
pub fn count_interaction_dims(parsed: Vec<(&DimRow, Value)>) -> InteractionDims {
    let mut raw_entries: Vec<RawEntry> = Vec::with_capacity(parsed.len());
    for (r, mut payload) in parsed {
        // `if r["timestamp"]: payload["timestamp"] = r["timestamp"]` — a truthy
        // column value overwrites; NULL and '' leave the payload's own.
        if let Some(ts) = r.timestamp.as_deref().filter(|t| !t.is_empty())
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("timestamp".to_string(), Value::String(ts.to_string()));
        }
        let session_id = r.session_id.clone().unwrap_or_default();
        raw_entries.push(RawEntry {
            payload,
            session_id,
            // `provider=r["provider"] or "anthropic"`
            provider: match r.provider.as_deref() {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => "anthropic".to_string(),
            },
        });
    }

    let dataset = build(tag(raw_entries));
    let records = &dataset.records;

    let cache_read_messages = records
        .iter()
        .filter(|rec| rec.kind == "assistant" && rec.cache_read_truthy)
        .count();

    // `Counter` insertion order is what `json.dumps` emits, so the order of
    // first occurrence is preserved here too.
    let mut errors_total = 0_i64;
    let mut cat_order: Vec<String> = Vec::new();
    let mut by_category: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for rec in records {
        if rec.is_error {
            errors_total += 1;
            // `rec.error_category or "Other"` — falsy includes the empty string.
            let cat = match rec.error_category.as_deref() {
                Some(c) if !c.is_empty() => c.to_string(),
                _ => "Other".to_string(),
            };
            if let Some(n) = by_category.get_mut(&cat) {
                *n += 1;
            } else {
                cat_order.push(cat.clone());
                by_category.insert(cat, 1);
            }
        }
    }
    let pairs: Vec<(String, i64)> = cat_order
        .into_iter()
        .map(|k| {
            let v = by_category[&k];
            (k, v)
        })
        .collect();

    let ca = command_analysis(&dataset);

    #[allow(clippy::cast_possible_wrap)]
    InteractionDims {
        records: records.len() as i64,
        cache_read_messages: cache_read_messages as i64,
        errors_total,
        errors_by_category_json: py_json_dumps_counter(&pairs),
        commands_followed_by_interruption: ca.commands_followed_by_interruption,
        command_tools: ca.total_tools_used,
        command_steps: ca.total_assistant_steps,
    }
}

#[cfg(test)]
mod tests {

    /// Not a test: a measurement harness for the per-tick peak. Run the
    /// release test binary under `/usr/bin/time -l` (macOS) or
    /// `/usr/bin/time -v` (Linux) with `STAX_DIMS_BENCH_STORE` pointing at a
    /// COPY of a real store and `STAX_DIMS_BENCH_PROJECT` naming a slug; the
    /// max RSS it prints is the number this module's design is judged on.
    #[test]
    #[ignore]
    fn bench_refresh_message_dims_on_a_real_store() {
        let Ok(store) = std::env::var("STAX_DIMS_BENCH_STORE") else {
            return;
        };
        let slug = std::env::var("STAX_DIMS_BENCH_PROJECT").expect("STAX_DIMS_BENCH_PROJECT");
        let conn = Connection::open(&store).expect("open the copied store");
        // `ALL` refreshes every project — the parity oracle: dump
        // `project_mart` before and after a change and diff the rows.
        let ids: Vec<i64> = if slug == "ALL" {
            let mut stmt = conn.prepare("SELECT id FROM projects ORDER BY id").unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        } else {
            vec![
                conn.query_row("SELECT id FROM projects WHERE slug = ?", [&slug], |r| r.get(0))
                    .expect("project slug exists"),
            ]
        };
        let messages: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages m JOIN sessions s ON s.id = m.session_fk WHERE s.project_id IN (SELECT id FROM projects WHERE ? = 'ALL' OR slug = ?)",
                [&slug, &slug],
                |r| r.get(0),
            )
            .unwrap();
        let started = std::time::Instant::now();
        refresh_message_dims(&conn, &ids).expect("refresh");
        eprintln!(
            "dims bench: {slug} projects={} messages={messages} elapsed={:.1?}",
            ids.len(),
            started.elapsed()
        );
    }

    use super::*;

    fn row(raw: &str, ts: &str) -> DimRow {
        DimRow {
            raw_json: Some(raw.to_string()),
            session_id: Some("s1".to_string()),
            timestamp: Some(ts.to_string()),
            provider: Some("claude".to_string()),
        }
    }

    #[test]
    fn div_002_a_legacy_user_turn_is_counted_as_an_assistant_message() {
        // No `type`, no `message.role` — exactly the shape behind the 5,656.
        let rows = vec![row(r#"{"message":{"content":"legacy user text"}}"#, "t1")];
        let (dims, _) = count_dims(&rows);
        assert_eq!(dims.user, 0, "the bug: this IS a user turn");
        assert_eq!(dims.assistant, 1, "…and Python counts it as an assistant");
        assert_eq!(dims.commands, 0, "…so it is not a command either");
    }

    #[test]
    fn message_type_dims_follow_the_pipelines_rules() {
        let rows = vec![
            row(
                r#"{"type":"human","message":{"role":"user","content":"/init"}}"#,
                "t1",
            ),
            row(
                r#"{"type":"assistant","message":{"role":"assistant","content":[
                    {"type":"tool_use","id":"a","name":"Read","input":{}}]}}"#,
                "t2",
            ),
            row(
                r#"{"type":"human","message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"a","content":"out"}]}}"#,
                "t3",
            ),
            row(
                r#"{"type":"assistant","message":{"role":"assistant","content":"done"}}"#,
                "t4",
            ),
        ];
        let (dims, _) = count_dims(&rows);
        assert_eq!(dims.user, 2, "the tool_result turn is still kind=user");
        assert_eq!(dims.assistant, 2);
        assert_eq!(dims.tool_use, 1);
        assert_eq!(dims.tool_result, 1);
        assert_eq!(dims.commands, 1, "a tool_result turn is not a command");
    }

    #[test]
    fn interruption_turns_are_user_messages_but_not_commands() {
        let rows = vec![
            row(r#"{"type":"human","message":{"content":"real"}}"#, "t1"),
            row(
                r#"{"type":"human","message":{"content":"[Request interrupted by user for tool use]"}}"#,
                "t2",
            ),
            row(
                r#"{"type":"human","message":{"content":"API Error: Request was aborted."}}"#,
                "t3",
            ),
        ];
        let (dims, _) = count_dims(&rows);
        assert_eq!(dims.user, 3);
        assert_eq!(dims.commands, 1);
    }

    #[test]
    fn undecodable_rows_are_skipped_by_both_counters() {
        let rows = vec![
            row("{not json", "t1"),
            row("[1,2]", "t2"),
            row("\"a string\"", "t3"),
            row("5", "t4"),
            row(r#"{"type":"human","message":{"content":"ok"}}"#, "t5"),
        ];
        let (dims, rate) = count_dims(&rows);
        assert_eq!(dims.user, 1);
        assert_eq!(dims.assistant, 0);
        assert_eq!(rate.records, 1, "the record count must skip them too");
    }

    #[test]
    fn an_empty_raw_json_is_counted_as_an_assistant_not_skipped() {
        // `json.loads(rj) if rj else {}` — NULL and '' become an empty dict,
        // which has no `type` and no `message.role`, so DIV-002's fall-through
        // files it as an assistant message AND as a step of the command before
        // it. "Nothing to parse" is not "nothing to count".
        let rows = vec![
            row(r#"{"type":"human","message":{"content":"go"}}"#, "t1"),
            DimRow {
                raw_json: None,
                session_id: Some("s1".into()),
                timestamp: Some("t2".into()),
                provider: Some("claude".into()),
            },
            row("", "t3"),
        ];
        let (dims, rate) = count_dims(&rows);
        assert_eq!(dims.user, 1);
        assert_eq!(dims.assistant, 2, "both empty rows are assistant messages");
        assert_eq!(rate.records, 3);
        assert_eq!(rate.command_steps, 2, "…and both are steps of the command");
    }

    #[test]
    fn errors_bucket_by_category_with_cpython_json_separators() {
        let err = |body: &str| {
            format!(
                r#"{{"type":"human","message":{{"content":[
                    {{"type":"tool_result","is_error":true,"content":"{body}"}}]}}}}"#
            )
        };
        let rows = vec![
            row(&err("Traceback (most recent call last)"), "t1"),
            row(&err("permission denied"), "t2"),
            row(&err("Traceback again"), "t3"),
            row(&err("nothing matches this"), "t4"),
        ];
        let (_, rate) = count_dims(&rows);
        assert_eq!(rate.errors_total, 4);
        assert_eq!(
            rate.errors_by_category_json,
            r#"{"Code Runtime Error": 2, "Permission Error": 1, "Other": 1}"#
        );
    }

    #[test]
    fn no_errors_renders_an_empty_object() {
        let rows = vec![row(
            r#"{"type":"human","message":{"content":"fine"}}"#,
            "t1",
        )];
        let (_, rate) = count_dims(&rows);
        assert_eq!(rate.errors_by_category_json, "{}");
        assert_eq!(rate.errors_total, 0);
    }

    #[test]
    fn cache_read_messages_count_assistant_records_with_cache_tokens() {
        let rows = vec![
            row(
                r#"{"type":"assistant","message":{"usage":{"cache_read_input_tokens":10}}}"#,
                "t1",
            ),
            row(
                r#"{"type":"assistant","message":{"usage":{"cache_read_input_tokens":0}}}"#,
                "t2",
            ),
            // A user turn with cache tokens is not counted — assistant only.
            row(
                r#"{"type":"human","message":{"usage":{"cache_read_input_tokens":99}}}"#,
                "t3",
            ),
        ];
        let (_, rate) = count_dims(&rows);
        assert_eq!(rate.cache_read_messages, 1);
    }

    #[test]
    fn the_column_timestamp_wins_over_the_payloads_own() {
        // `build_enriched_dataset`'s rule, and it changes the interaction order.
        let rows = vec![
            row(
                r#"{"type":"human","timestamp":"z-late","message":{"content":"first"}}"#,
                "a",
            ),
            row(
                r#"{"type":"assistant","timestamp":"a-early","message":{"content":"reply"}}"#,
                "b",
            ),
        ];
        let (_, rate) = count_dims(&rows);
        // With the column timestamps (a < b) the assistant follows the user and
        // counts as its step. With the payload ones (z-late > a-early) it would
        // precede it and count as nothing.
        assert_eq!(rate.command_steps, 1);
    }

    #[test]
    fn command_numerators_come_from_the_real_command_analysis() {
        let rows = vec![
            row(r#"{"type":"human","message":{"content":"go"}}"#, "t1"),
            row(
                r#"{"type":"assistant","message":{"content":[
                    {"type":"tool_use","id":"a","name":"Read","input":{}},
                    {"type":"tool_use","id":"b","name":"Edit","input":{}}]}}"#,
                "t2",
            ),
            row(
                r#"{"type":"human","message":{"content":"[Request interrupted by user for tool use]"}}"#,
                "t3",
            ),
        ];
        let (_, rate) = count_dims(&rows);
        assert_eq!(rate.command_tools, 2);
        assert_eq!(rate.command_steps, 1);
        assert_eq!(rate.commands_followed_by_interruption, 1);
        assert_eq!(rate.records, 3);
    }
}
