//! Port of `python-legacy: etl/marts/tool.py` — the
//! `(day, project_id, provider, tool_name)` rollup with 1/N attribution.
//!
//! One billable event is one assistant `messages` row; that row's `tools_json`
//! carries every tool the turn invoked, and the mart fans the event out across
//! the message's **distinct** tool names. Cost and all four token columns are
//! split `1/N` over those distinct names (`N` = distinct count), mirroring the
//! legacy `stats.aggregator._ToolCostCollector`, so a turn that called `Read`
//! three times contributes one `Read` bucket's worth of cost and three to
//! `calls_total`.
//!
//! # Float accumulation order is part of the answer
//!
//! `bucket["cost_usd"] += cost_share` runs once per event in `ORDER BY e.id`,
//! and float addition is not associative — accumulating the same shares in a
//! different order gives a different `f64`. The window query keeps its
//! `ORDER BY e.id` and the bucket map here is insertion-ordered over the same
//! row sequence, so the additions land in Python's order. A `HashMap` iterated
//! for the insert would be fine (each bucket sums independently), but the
//! *within-bucket* order is the one that cannot move.
//!
//! # `session_count`'s real cost
//!
//! `COUNT(DISTINCT session_id)` is not additive across windows, and it cannot
//! be reconstructed from the window alone — a session that used the tool in an
//! earlier window would be invisible. The recompute therefore still scans the
//! complete event history of every touched `(day, project_id, provider)`
//! triple and re-parses `tools_json`, but it loads bounded groups together so
//! the partitioned message view is materialized once per chunk rather than
//! once per group. This preserves Python's documented option (d) without its
//! repeated-query multiplier.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::Value;

use super::{MartBuilder, max_event_id};

/// Per-`(day, project, provider, tool_name)` cost + token rollup.
pub struct ToolMartBuilder;

/// Three scalar binds per affected `(day, project, provider)` key, capped at
/// the task's conservative 400-variable ceiling.
const MAX_BATCH_BINDS: usize = 400;
const TOOL_GROUP_CHUNK: usize = MAX_BATCH_BINDS / 3;
const _: () = assert!(TOOL_GROUP_CHUNK * 3 <= MAX_BATCH_BINDS);

#[derive(Default)]
struct SourceScanAccounting {
    #[cfg(test)]
    queries: usize,
}

impl SourceScanAccounting {
    fn record_query(&mut self) {
        #[cfg(test)]
        {
            self.queries += 1;
        }
    }
}

/// The additive measures accumulated per bucket.
#[derive(Default, Clone, Copy)]
struct Bucket {
    event_count: i64,
    calls_total: i64,
    cost_usd: f64,
    tokens_in: f64,
    tokens_out: f64,
    cache_read: f64,
    cache_create: f64,
}

type Key = (String, i64, String, String);

struct WindowRow {
    day: String,
    project_id: i64,
    provider: String,
    cost_usd: f64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_create_tokens: i64,
    tools_json: Option<String>,
}

impl MartBuilder for ToolMartBuilder {
    fn name(&self) -> &'static str {
        "tool"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        let rows = fetch_window(conn, since_event_id, max_id)?;

        // Insertion-ordered bucket map: a Vec holds the buckets in first-seen
        // order and the HashMap indexes into it. Python's dict gives both for
        // free; this is the same structure spelled out.
        let mut order: Vec<Key> = Vec::new();
        let mut buckets: HashMap<Key, Bucket> = HashMap::new();

        for r in &rows {
            let tool_counts = parse_tool_names(r.tools_json.as_deref());
            if tool_counts.is_empty() {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let n = tool_counts.len() as f64;
            let cost_share = r.cost_usd / n;
            #[allow(clippy::cast_precision_loss)]
            let in_share = r.input_tokens as f64 / n;
            #[allow(clippy::cast_precision_loss)]
            let out_share = r.output_tokens as f64 / n;
            #[allow(clippy::cast_precision_loss)]
            let cache_read_share = r.cache_read_tokens as f64 / n;
            #[allow(clippy::cast_precision_loss)]
            let cache_create_share = r.cache_create_tokens as f64 / n;

            for (tool_name, occurrences) in &tool_counts {
                let key = (
                    r.day.clone(),
                    r.project_id,
                    r.provider.clone(),
                    tool_name.clone(),
                );
                if !buckets.contains_key(&key) {
                    order.push(key.clone());
                    buckets.insert(key.clone(), Bucket::default());
                }
                let bucket = buckets.get_mut(&key).expect("just inserted");
                bucket.event_count += 1;
                bucket.calls_total += occurrences;
                bucket.cost_usd += cost_share;
                bucket.tokens_in += in_share;
                bucket.tokens_out += out_share;
                bucket.cache_read += cache_read_share;
                bucket.cache_create += cache_create_share;
            }
        }

        if !order.is_empty() {
            let mut stmt = conn.prepare(
                r"
                INSERT INTO tool_mart (
                    day, project_id, provider, tool_name,
                    event_count, calls_total, cost_usd, tokens_in, tokens_out,
                    cache_read, cache_create,
                    session_count
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)
                ON CONFLICT (day, project_id, provider, tool_name) DO UPDATE SET
                    event_count  = event_count  + excluded.event_count,
                    calls_total  = calls_total  + excluded.calls_total,
                    cost_usd     = cost_usd     + excluded.cost_usd,
                    tokens_in    = tokens_in    + excluded.tokens_in,
                    tokens_out   = tokens_out   + excluded.tokens_out,
                    cache_read   = cache_read   + excluded.cache_read,
                    cache_create = cache_create + excluded.cache_create
                ",
            )?;
            for key in &order {
                let v = &buckets[key];
                stmt.execute(rusqlite::params![
                    key.0,
                    key.1,
                    key.2,
                    key.3,
                    v.event_count,
                    v.calls_total,
                    v.cost_usd,
                    // Python: `int(v["tokens_in"])` — truncation toward zero,
                    // which is what `as i64` does for a finite non-negative f64.
                    trunc_i64(v.tokens_in),
                    trunc_i64(v.tokens_out),
                    trunc_i64(v.cache_read),
                    trunc_i64(v.cache_create),
                ])?;
            }
            drop(stmt);

            recompute_session_counts(conn, &order)?;
        }

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute("DELETE FROM tool_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

/// Python's `int(f)` for the non-negative accumulators here: truncate toward zero.
#[allow(clippy::cast_possible_truncation)]
fn trunc_i64(v: f64) -> i64 {
    v as i64
}

/// `tool._fetch_window` — joined event + message rows in `(since, max]`.
///
/// `LEFT JOIN` defends against an event whose source message was deleted; such
/// a row has no `tools_json` and contributes nothing.
fn fetch_window(conn: &Connection, since_event_id: i64, max_id: i64) -> Result<Vec<WindowRow>> {
    let mut stmt = conn.prepare(
        r"
        SELECT e.id                  AS event_id,
               e.day                 AS day,
               e.project_id          AS project_id,
               e.provider            AS provider,
               e.session_id          AS session_id,
               e.cost_usd            AS cost_usd,
               e.input_tokens        AS input_tokens,
               e.output_tokens       AS output_tokens,
               e.cache_read_tokens   AS cache_read_tokens,
               e.cache_create_tokens AS cache_create_tokens,
               m.tools_json          AS tools_json
          FROM usage_events e
          LEFT JOIN messages m ON m.id = e.source_message_fk
         WHERE e.id > ? AND e.id <= ?
         ORDER BY e.id
        ",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![since_event_id, max_id], |r| {
            Ok(WindowRow {
                day: r.get::<_, Option<String>>("day")?.unwrap_or_default(),
                project_id: r.get::<_, Option<i64>>("project_id")?.unwrap_or_default(),
                provider: r.get::<_, Option<String>>("provider")?.unwrap_or_default(),
                // `float(r["cost_usd"] or 0.0)` / `int(r["…"] or 0)`
                cost_usd: r.get::<_, Option<f64>>("cost_usd")?.unwrap_or(0.0),
                input_tokens: r.get::<_, Option<i64>>("input_tokens")?.unwrap_or(0),
                output_tokens: r.get::<_, Option<i64>>("output_tokens")?.unwrap_or(0),
                cache_read_tokens: r.get::<_, Option<i64>>("cache_read_tokens")?.unwrap_or(0),
                cache_create_tokens: r.get::<_, Option<i64>>("cache_create_tokens")?.unwrap_or(0),
                tools_json: r.get::<_, Option<String>>("tools_json")?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `tool._parse_tool_names` — `{tool_name: occurrence_count}`, first-seen order.
///
/// The writer stores `json.dumps(list(rec.tools))`, so the value is a JSON array
/// of strings. Distinct names are the 1/N attribution unit; total occurrences
/// are `calls_total`. Malformed JSON, non-arrays, empty arrays and non-string
/// entries all contribute nothing — the same defensive parse the aggregator has.
#[must_use]
pub fn parse_tool_names(tools_json: Option<&str>) -> Vec<(String, i64)> {
    let Some(parsed) = super::json::loads(tools_json) else {
        return Vec::new();
    };
    let Value::Array(items) = parsed else {
        return Vec::new();
    };
    // `Counter` is insertion-ordered on first occurrence.
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, i64> = HashMap::new();
    for entry in items {
        if let Value::String(s) = entry
            && !s.is_empty()
        {
            if let Some(n) = counts.get_mut(&s) {
                *n += 1;
            } else {
                order.push(s.clone());
                counts.insert(s, 1);
            }
        }
    }
    order.into_iter().map(|k| (k.clone(), counts[&k])).collect()
}

/// `tool._recompute_session_counts` — the true DISTINCT for the touched keys.
///
/// Keys are deduped to their `(day, project_id, provider)` groups first, so the
/// per-group `tools_json` reparse happens once rather than once per tool name.
fn recompute_session_counts(conn: &Connection, keys: &[Key]) -> Result<()> {
    let mut accounting = SourceScanAccounting::default();
    recompute_session_counts_with_accounting(conn, keys, &mut accounting)
}

fn tool_group_batch_sql(group_count: usize) -> String {
    debug_assert!(group_count > 0 && group_count <= TOOL_GROUP_CHUNK);
    let values = (0..group_count)
        .map(|ordinal| format!("({ordinal}, ?, ?, ?)"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "WITH affected(ordinal, day, project_id, provider) AS MATERIALIZED (VALUES {values}) \
         SELECT affected.ordinal, e.session_id, m.tools_json \
         FROM affected \
         JOIN usage_events e \
           ON e.day = affected.day \
          AND e.project_id = affected.project_id \
          AND e.provider = affected.provider \
         LEFT JOIN messages m ON m.id = e.source_message_fk \
         ORDER BY affected.ordinal, e.id"
    )
}

fn recompute_session_counts_with_accounting(
    conn: &Connection,
    keys: &[Key],
    accounting: &mut SourceScanAccounting,
) -> Result<()> {
    let mut group_order: Vec<(String, i64, String)> = Vec::new();
    let mut group_tools: HashMap<(String, i64, String), HashSet<String>> = HashMap::new();
    for k in keys {
        let g = (k.0.clone(), k.1, k.2.clone());
        if let Some(set) = group_tools.get_mut(&g) {
            set.insert(k.3.clone());
        } else {
            group_order.push(g.clone());
            group_tools.insert(g, HashSet::from([k.3.clone()]));
        }
    }

    let mut update = conn.prepare(
        r"
        UPDATE tool_mart
           SET session_count = ?
         WHERE day = ? AND project_id = ?
           AND provider = ? AND tool_name = ?
        ",
    )?;

    for chunk in group_order.chunks(TOOL_GROUP_CHUNK) {
        let sql = tool_group_batch_sql(chunk.len());
        let mut params = Vec::with_capacity(chunk.len() * 3);
        for (day, project_id, provider) in chunk {
            params.push(SqlValue::Text(day.clone()));
            params.push(SqlValue::Integer(*project_id));
            params.push(SqlValue::Text(provider.clone()));
        }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    // `str(row["session_id"] or "")`
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        accounting.record_query();

        let mut rows_by_group: Vec<Vec<(String, Option<String>)>> =
            (0..chunk.len()).map(|_| Vec::new()).collect();
        for (ordinal, session_id, tools_json) in rows {
            rows_by_group[usize::try_from(ordinal)?].push((session_id, tools_json));
        }

        for (g, rows) in chunk.iter().zip(rows_by_group) {
            let wanted = &group_tools[g];
            let mut per_tool_order: Vec<String> = Vec::new();
            let mut per_tool_sessions: HashMap<String, HashSet<String>> = HashMap::new();
            for (sid, tools_json) in rows {
                let tools = parse_tool_names(tools_json.as_deref());
                if tools.is_empty() {
                    continue;
                }
                for (t, _) in tools {
                    if wanted.contains(&t) {
                        if let Some(set) = per_tool_sessions.get_mut(&t) {
                            set.insert(sid.clone());
                        } else {
                            per_tool_order.push(t.clone());
                            per_tool_sessions.insert(t, HashSet::from([sid.clone()]));
                        }
                    }
                }
            }

            for t in &per_tool_order {
                #[allow(clippy::cast_possible_wrap)]
                let n = per_tool_sessions[t].len() as i64;
                update.execute(rusqlite::params![n, g.0, g.1, g.2, t])?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testdb;
    use super::*;

    fn seed(c: &Connection) {
        testdb::project(c, 1, "p", "claude");
        testdb::session(c, 1, 1, "s1");
    }

    fn seed_batch_dimensions(c: &Connection) {
        testdb::project(c, 1, "p1", "provider-a");
        testdb::project(c, 2, "p2", "provider-b");
        testdb::session(c, 1, 1, "one");
        testdb::session(c, 2, 1, "two");
        testdb::session(c, 3, 2, "three");
    }

    #[allow(clippy::too_many_arguments)]
    fn tool_event(
        c: &Connection,
        id: i64,
        source_message_fk: Option<i64>,
        session_fk: i64,
        project_id: i64,
        session_id: &str,
        provider: &str,
        day: &str,
        tools_json: Option<&str>,
        tokens: (i64, i64, i64, i64),
        cost: f64,
    ) {
        if let (Some(message_id), Some(tools_json)) = (source_message_fk, tools_json) {
            testdb::message(
                c,
                message_id,
                session_fk,
                id,
                "2026-01-01T00:00:00Z",
                "assistant",
                "",
                tools_json,
                "{}",
            );
        }
        testdb::event(
            c,
            id,
            source_message_fk,
            project_id,
            session_id,
            provider,
            "m",
            day,
            tokens,
            cost,
        );
    }

    fn seed_batch_prefix(c: &Connection) {
        tool_event(
            c,
            1,
            Some(1),
            1,
            1,
            "shared",
            "provider-a",
            "2026-01-01",
            Some(r#"["Read","Read","Edit"]"#),
            (8, 16, 4, 12),
            1.0,
        );
        tool_event(
            c,
            2,
            Some(2),
            2,
            1,
            "shared",
            "provider-a",
            "2026-01-01",
            Some(r#"["Read"]"#),
            (4, 8, 0, 0),
            0.5,
        );
        tool_event(
            c,
            3,
            Some(3),
            1,
            1,
            "alias",
            "provider-a",
            "2026-01-02",
            Some("malformed"),
            (2, 2, 0, 0),
            2.0,
        );
    }

    fn seed_batch_tail(c: &Connection) {
        tool_event(
            c,
            4,
            Some(4),
            1,
            1,
            "alias",
            "provider-a",
            "2026-01-01",
            Some(r#"["Edit","Write"]"#),
            (4, 8, 2, 6),
            0.5,
        );
        tool_event(
            c,
            5,
            Some(999),
            1,
            1,
            "missing",
            "provider-a",
            "2026-01-01",
            None,
            (4, 4, 0, 0),
            4.0,
        );
        tool_event(
            c,
            6,
            Some(6),
            3,
            2,
            "other",
            "provider-b",
            "2026-01-01",
            Some(r#"["Read",4,"","Read"]"#),
            (4, 8, 0, 0),
            0.25,
        );
        tool_event(
            c,
            7,
            Some(7),
            3,
            2,
            "other",
            "provider-b",
            "2026-01-02",
            Some(r#"{"bad":1}"#),
            (2, 2, 0, 0),
            1.0,
        );
        tool_event(
            c,
            8,
            Some(8),
            3,
            2,
            "",
            "provider-b",
            "2026-01-01",
            Some(r#"["Write"]"#),
            (2, 4, 0, 0),
            0.25,
        );
        tool_event(
            c,
            9,
            Some(9),
            1,
            1,
            "alias",
            "provider-a",
            "2026-01-01",
            Some(r#"["Edit"]"#),
            (2, 4, 0, 0),
            0.25,
        );
    }

    type ToolSnapshotRow = (
        String,
        i64,
        String,
        String,
        i64,
        i64,
        u64,
        i64,
        i64,
        i64,
        i64,
        i64,
    );

    fn tool_snapshot(c: &Connection, include_unaffected: bool) -> Vec<ToolSnapshotRow> {
        let filter = if include_unaffected {
            ""
        } else {
            "WHERE day != 'unaffected'"
        };
        c.prepare(&format!(
            "SELECT day, project_id, provider, tool_name, event_count, calls_total, \
                    cost_usd, tokens_in, tokens_out, cache_read, cache_create, session_count \
             FROM tool_mart {filter} ORDER BY day, project_id, provider, tool_name"
        ))
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get::<_, f64>(6)?.to_bits(),
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn mart_keys(c: &Connection) -> Vec<Key> {
        c.prepare(
            "SELECT day, project_id, provider, tool_name FROM tool_mart \
             WHERE day != 'unaffected' ORDER BY day, project_id, provider, tool_name",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn recompute_session_counts_oracle(conn: &Connection, keys: &[Key]) {
        let mut group_order: Vec<(String, i64, String)> = Vec::new();
        let mut group_tools: HashMap<(String, i64, String), HashSet<String>> = HashMap::new();
        for key in keys {
            let group = (key.0.clone(), key.1, key.2.clone());
            if let Some(tools) = group_tools.get_mut(&group) {
                tools.insert(key.3.clone());
            } else {
                group_order.push(group.clone());
                group_tools.insert(group, HashSet::from([key.3.clone()]));
            }
        }

        let mut scan = conn
            .prepare(
                "SELECT e.session_id, m.tools_json FROM usage_events e \
                 LEFT JOIN messages m ON m.id = e.source_message_fk \
                 WHERE e.day = ? AND e.project_id = ? AND e.provider = ?",
            )
            .unwrap();
        let mut update = conn
            .prepare(
                "UPDATE tool_mart SET session_count = ? \
                 WHERE day = ? AND project_id = ? AND provider = ? AND tool_name = ?",
            )
            .unwrap();
        for group in &group_order {
            let wanted = &group_tools[group];
            let rows: Vec<(String, Option<String>)> = scan
                .query_map(rusqlite::params![group.0, group.1, group.2], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        row.get(1)?,
                    ))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            let mut sessions: HashMap<String, HashSet<String>> = HashMap::new();
            for (session_id, tools_json) in rows {
                for (tool, _) in parse_tool_names(tools_json.as_deref()) {
                    if wanted.contains(&tool) {
                        sessions.entry(tool).or_default().insert(session_id.clone());
                    }
                }
            }
            for (tool, session_ids) in sessions {
                update
                    .execute(rusqlite::params![
                        i64::try_from(session_ids.len()).unwrap(),
                        group.0,
                        group.1,
                        group.2,
                        tool
                    ])
                    .unwrap();
            }
        }
    }

    #[test]
    fn tool_names_count_occurrences_and_keep_first_seen_order() {
        assert_eq!(
            parse_tool_names(Some(r#"["Read","Edit","Read"]"#)),
            vec![("Read".to_string(), 2), ("Edit".to_string(), 1)]
        );
        assert!(parse_tool_names(Some("[]")).is_empty());
        assert!(parse_tool_names(Some("not json")).is_empty());
        assert!(parse_tool_names(Some(r#"{"a":1}"#)).is_empty());
        assert!(parse_tool_names(None).is_empty());
        // non-string and empty-string entries drop out
        assert_eq!(
            parse_tool_names(Some(r#"["Read", 5, "", null, "Read"]"#)),
            vec![("Read".to_string(), 2)]
        );
    }

    #[test]
    fn cost_splits_one_over_distinct_names_not_over_calls() {
        let c = testdb::conn();
        seed(&c);
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "assistant",
            "",
            r#"["Read","Read","Edit"]"#,
            "{}",
        );
        testdb::event(
            &c,
            1,
            Some(1),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (10, 20, 4, 8),
            1.0,
        );
        ToolMartBuilder.refresh(&c, 0).unwrap();

        let mut stmt = c
            .prepare(
                "SELECT tool_name, event_count, calls_total, cost_usd, tokens_in, tokens_out, \
                 cache_read, cache_create, session_count FROM tool_mart ORDER BY tool_name",
            )
            .unwrap();
        type Row = (String, i64, i64, f64, i64, i64, i64, i64, i64);
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();

        assert_eq!(rows.len(), 2);
        // Edit: 1 event, 1 call, half the cost and half of every token column.
        assert_eq!(rows[0].0, "Edit");
        assert_eq!((rows[0].1, rows[0].2), (1, 1));
        assert!((rows[0].3 - 0.5).abs() < 1e-12);
        assert_eq!((rows[0].4, rows[0].5, rows[0].6, rows[0].7), (5, 10, 2, 4));
        // Read: 1 event (distinct), 2 calls, the SAME half — repeats never
        // double the cost.
        assert_eq!(rows[1].0, "Read");
        assert_eq!((rows[1].1, rows[1].2), (1, 2));
        assert!((rows[1].3 - 0.5).abs() < 1e-12);
        assert_eq!(rows[1].8, 1);
    }

    #[test]
    fn session_count_is_recomputed_from_the_full_day_not_the_window() {
        // The undercount option (b) was rejected for exactly this shape: the
        // session that used `Read` in window 1 must still be counted in
        // window 2's recompute.
        let c = testdb::conn();
        seed(&c);
        testdb::session(&c, 2, 1, "s2");
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "assistant",
            "",
            r#"["Read"]"#,
            "{}",
        );
        testdb::event(
            &c,
            1,
            Some(1),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        ToolMartBuilder.refresh(&c, 0).unwrap();

        testdb::message(
            &c,
            2,
            2,
            0,
            "2026-01-01T00:00:00Z",
            "assistant",
            "",
            r#"["Read"]"#,
            "{}",
        );
        testdb::event(
            &c,
            2,
            Some(2),
            1,
            "s2",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        ToolMartBuilder.refresh(&c, 1).unwrap();

        let (ec, sc): (i64, i64) = c
            .query_row(
                "SELECT event_count, session_count FROM tool_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((ec, sc), (2, 2));
    }

    #[test]
    fn events_whose_message_has_no_tools_contribute_nothing() {
        let c = testdb::conn();
        seed(&c);
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "assistant",
            "",
            "[]",
            "{}",
        );
        testdb::event(
            &c,
            1,
            Some(1),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            9.0,
        );
        // …and an event whose source message vanished entirely.
        testdb::event(
            &c,
            2,
            Some(999),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 1, 0, 0),
            9.0,
        );
        assert_eq!(ToolMartBuilder.refresh(&c, 0).unwrap(), 2);
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM tool_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn token_columns_truncate_toward_zero_like_pythons_int() {
        let c = testdb::conn();
        seed(&c);
        // 3 distinct tools, 10 input tokens → 3.333… each, stored as 3.
        testdb::message(
            &c,
            1,
            1,
            0,
            "2026-01-01T00:00:00Z",
            "assistant",
            "",
            r#"["A","B","C"]"#,
            "{}",
        );
        testdb::event(
            &c,
            1,
            Some(1),
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (10, 10, 0, 0),
            0.0,
        );
        ToolMartBuilder.refresh(&c, 0).unwrap();
        let tin: i64 = c
            .query_row(
                "SELECT tokens_in FROM tool_mart WHERE tool_name='A'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tin, 3);
    }

    #[test]
    fn batched_recompute_matches_the_incumbent_and_exact_window_schedules() {
        let build_incremental = || {
            let c = testdb::conn();
            seed_batch_dimensions(&c);
            seed_batch_prefix(&c);
            assert_eq!(ToolMartBuilder.refresh(&c, 0).unwrap(), 3);
            seed_batch_tail(&c);
            assert_eq!(ToolMartBuilder.refresh(&c, 3).unwrap(), 9);
            c.execute(
                "INSERT INTO tool_mart \
                 (day, project_id, provider, tool_name, session_count) \
                 VALUES ('unaffected', 1, 'provider-a', 'Keep', 77)",
                [],
            )
            .unwrap();
            c
        };

        let optimized = build_incremental();
        let incumbent = build_incremental();
        let keys = mart_keys(&optimized);
        optimized
            .execute(
                "UPDATE tool_mart SET session_count = 0 WHERE day != 'unaffected'",
                [],
            )
            .unwrap();
        incumbent
            .execute(
                "UPDATE tool_mart SET session_count = 0 WHERE day != 'unaffected'",
                [],
            )
            .unwrap();
        let mut accounting = SourceScanAccounting::default();
        recompute_session_counts_with_accounting(&optimized, &keys, &mut accounting).unwrap();
        recompute_session_counts_oracle(&incumbent, &keys);
        assert_eq!(accounting.queries, 1);
        assert_eq!(
            tool_snapshot(&optimized, true),
            tool_snapshot(&incumbent, true)
        );
        let unaffected: i64 = optimized
            .query_row(
                "SELECT session_count FROM tool_mart WHERE day = 'unaffected'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unaffected, 77);

        let from_zero = testdb::conn();
        seed_batch_dimensions(&from_zero);
        seed_batch_prefix(&from_zero);
        seed_batch_tail(&from_zero);
        assert_eq!(ToolMartBuilder.refresh(&from_zero, 0).unwrap(), 9);
        let incremental_snapshot = tool_snapshot(&optimized, false);
        assert_eq!(
            incremental_snapshot,
            tool_snapshot(&from_zero, false),
            "exact-representable fixture differs between incremental and from-zero"
        );
        ToolMartBuilder.rebuild_from_scratch(&from_zero).unwrap();
        assert_eq!(incremental_snapshot, tool_snapshot(&from_zero, false));
        let edit_sessions: i64 = from_zero
            .query_row(
                "SELECT session_count FROM tool_mart \
                 WHERE day = '2026-01-01' AND project_id = 1 \
                   AND provider = 'provider-a' AND tool_name = 'Edit'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edit_sessions, 2);
    }

    #[test]
    fn tool_source_queries_scale_by_chunks_and_materialize_once_per_chunk() {
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "provider-a");
        let mut keys = Vec::with_capacity(TOOL_GROUP_CHUNK + 1);
        for index in 0..=TOOL_GROUP_CHUNK {
            let day = format!("day-{index:03}");
            let session_fk = i64::try_from(index).unwrap() + 1;
            let session_id = format!("session-{index:03}");
            let tool_name = if index == TOOL_GROUP_CHUNK {
                "Tail"
            } else {
                "Read"
            };
            testdb::session(&c, session_fk, 1, &session_id);
            testdb::message(
                &c,
                session_fk,
                session_fk,
                0,
                &format!("{day}T00:00:00Z"),
                "assistant",
                "",
                &format!(r#"["{tool_name}"]"#),
                "{}",
            );
            testdb::event(
                &c,
                session_fk,
                Some(session_fk),
                1,
                &session_id,
                "provider-a",
                "m",
                &day,
                (1, 1, 0, 0),
                0.0,
            );
            c.execute(
                "INSERT INTO tool_mart \
                 (day, project_id, provider, tool_name, session_count) \
                 VALUES (?, 1, 'provider-a', ?, 77)",
                rusqlite::params![day, tool_name],
            )
            .unwrap();
            keys.push((day, 1, "provider-a".to_string(), tool_name.to_string()));
        }
        c.execute(
            "INSERT INTO tool_mart \
             (day, project_id, provider, tool_name, session_count) \
             VALUES ('unaffected', 1, 'provider-a', 'Read', 88)",
            [],
        )
        .unwrap();
        let mut accounting = SourceScanAccounting::default();
        recompute_session_counts_with_accounting(&c, &keys, &mut accounting).unwrap();
        assert_eq!(accounting.queries, 2);
        let populated: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM tool_mart \
                 WHERE day != 'unaffected' AND session_count = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(populated, i64::try_from(TOOL_GROUP_CHUNK + 1).unwrap());
        let first_chunk_count: i64 = c
            .query_row(
                "SELECT session_count FROM tool_mart \
                 WHERE day = 'day-000' AND tool_name = 'Read'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_chunk_count, 1);
        let final_chunk_count: i64 = c
            .query_row(
                "SELECT session_count FROM tool_mart \
                 WHERE day = ? AND tool_name = 'Tail'",
                [format!("day-{TOOL_GROUP_CHUNK:03}")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(final_chunk_count, 1, "final-chunk ordinal routed its row");
        let unaffected: i64 = c
            .query_row(
                "SELECT session_count FROM tool_mart WHERE day = 'unaffected'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unaffected, 88);

        let production = crate::ingest::testdb::store();
        for partition in ["messages_202601", "messages_202602"] {
            crate::ingest::writer::ensure_partition(&production, partition).unwrap();
        }
        let sql = format!("EXPLAIN QUERY PLAN {}", tool_group_batch_sql(2));
        let params = [
            SqlValue::Text("2026-01-01".to_string()),
            SqlValue::Integer(1),
            SqlValue::Text("provider-a".to_string()),
            SqlValue::Text("2026-01-02".to_string()),
            SqlValue::Integer(2),
            SqlValue::Text("provider-b".to_string()),
        ];
        let plan: Vec<String> = production
            .prepare(&sql)
            .unwrap()
            .query_map(params_from_iter(params.iter()), |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        eprintln!("tool affected-group batch plan: {plan:#?}");
        assert_eq!(
            plan.iter()
                .filter(|detail| detail.as_str() == "MATERIALIZE messages")
                .count(),
            1,
            "message-view work must occur once per source chunk: {plan:#?}"
        );
        assert!(
            plan.iter()
                .any(|detail| detail == "USE TEMP B-TREE FOR ORDER BY"),
            "batched rows must be ordered by affected ordinal and event id: {plan:#?}"
        );
    }
}
