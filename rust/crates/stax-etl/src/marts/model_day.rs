//! Port of `python-legacy: etl/marts/model_day.py` — the `(day, model, speed)`
//! rollup for the compare-across-agents view.
//!
//! Additive over the SUM/COUNT(*) columns; `session_count` is recomputed for
//! affected keys after the upsert, for the reason `daily` documents at length.

use anyhow::Result;
use rusqlite::Connection;

use super::{MartBuilder, max_event_id};

/// Per-`(day, model, speed)` rollup across all providers + projects.
pub struct ModelDayMartBuilder;

const RECOMPUTE_SESSION_COUNTS_SQL: &str = r"
    WITH affected AS (
        SELECT DISTINCT day, model, speed
        FROM usage_events
        WHERE id > ? AND id <= ?
    ),
    recomputed AS MATERIALIZED (
        SELECT
            e.day, e.model, e.speed,
            COUNT(DISTINCT e.session_id) AS sc
        FROM usage_events e
        JOIN affected a USING (day, model, speed)
        GROUP BY e.day, e.model, e.speed
    )
    UPDATE model_day_mart
       SET session_count = (
           SELECT sc FROM recomputed r
           WHERE r.day = model_day_mart.day
             AND r.model = model_day_mart.model
             AND r.speed = model_day_mart.speed
       )
     WHERE (day, model, speed) IN (
           SELECT day, model, speed FROM affected
       )
";

impl MartBuilder for ModelDayMartBuilder {
    fn name(&self) -> &'static str {
        "model_day"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        conn.execute(
            r"
            INSERT INTO model_day_mart (
                day, model, speed,
                cost_usd, input_tokens, output_tokens,
                cache_read, cache_create,
                message_count, session_count
            )
            SELECT
                day, model, speed,
                SUM(cost_usd),
                SUM(input_tokens),
                SUM(output_tokens),
                SUM(cache_read_tokens),
                SUM(cache_create_tokens),
                COUNT(*),
                COUNT(DISTINCT session_id)
            FROM usage_events
            WHERE id > ? AND id <= ?
            GROUP BY day, model, speed
            ON CONFLICT (day, model, speed) DO UPDATE SET
                cost_usd      = cost_usd      + excluded.cost_usd,
                input_tokens  = input_tokens  + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_read    = cache_read    + excluded.cache_read,
                cache_create  = cache_create  + excluded.cache_create,
                message_count = message_count + excluded.message_count,
                session_count = session_count + excluded.session_count
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        conn.execute(
            RECOMPUTE_SESSION_COUNTS_SQL,
            rusqlite::params![since_event_id, max_id],
        )?;

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute("DELETE FROM model_day_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{testdb, watermark};
    use super::*;

    fn seed_dimensions(c: &Connection) {
        testdb::project(c, 1, "one", "provider-a");
        testdb::project(c, 2, "two", "provider-b");
    }

    fn seed_model_event(c: &Connection, id: i64) {
        let (project_id, session_id, provider, model, day, tokens, cost) = match id {
            1 => (
                1,
                "a",
                "provider-a",
                "model-a",
                "2026-01-01",
                (1, 2, 0, 0),
                0.25,
            ),
            2 => (
                2,
                "b",
                "provider-b",
                "model-a",
                "2026-01-01",
                (2, 4, 1, 0),
                0.5,
            ),
            3 => (
                1,
                "a",
                "provider-a",
                "model-b",
                "2026-01-01",
                (4, 8, 0, 2),
                1.0,
            ),
            4 => (
                2,
                "c",
                "provider-b",
                "model-b",
                "2026-01-02",
                (1, 1, 0, 0),
                0.25,
            ),
            5 => (
                1,
                "a",
                "provider-a",
                "model-a",
                "2026-01-01",
                (1, 2, 0, 0),
                0.25,
            ),
            6 => (
                2,
                "d",
                "provider-b",
                "model-a",
                "2026-01-01",
                (2, 4, 1, 0),
                0.5,
            ),
            7 => (
                1,
                "e",
                "provider-a",
                "model-b",
                "2026-01-01",
                (2, 2, 0, 0),
                0.25,
            ),
            8 => (
                2,
                "c",
                "provider-b",
                "model-b",
                "2026-01-02",
                (1, 1, 0, 0),
                0.25,
            ),
            _ => panic!("unknown model fixture event {id}"),
        };
        testdb::event(
            c, id, None, project_id, session_id, provider, model, day, tokens, cost,
        );
    }

    type ModelSnapshotRow = (String, String, String, u64, i64, i64, i64, i64, i64, i64);

    fn snapshot(c: &Connection) -> Vec<ModelSnapshotRow> {
        c.prepare(
            "SELECT day, model, speed, cost_usd, input_tokens, output_tokens, \
                    cache_read, cache_create, message_count, session_count \
             FROM model_day_mart ORDER BY day, model, speed",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get::<_, f64>(3)?.to_bits(),
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn refresh_with_watermark(c: &Connection, now: &str) -> i64 {
        let since = watermark::get_watermark(c, "model_day").unwrap();
        let next = ModelDayMartBuilder.refresh(c, since).unwrap();
        watermark::set_watermark(c, "model_day", next, now).unwrap();
        next
    }

    #[test]
    fn the_key_spans_projects_and_providers() {
        let c = testdb::conn();
        testdb::project(&c, 1, "a", "claude");
        testdb::project(&c, 2, "b", "codex");
        testdb::event(
            &c,
            1,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-01",
            (1, 1, 0, 0),
            1.0,
        );
        testdb::event(
            &c,
            2,
            None,
            2,
            "s2",
            "codex",
            "opus",
            "2026-01-01",
            (2, 2, 0, 0),
            2.0,
        );
        ModelDayMartBuilder.refresh(&c, 0).unwrap();

        let (rows, msgs, sc, cost): (i64, i64, i64, f64) = c
            .query_row(
                "SELECT COUNT(*), SUM(message_count), SUM(session_count), SUM(cost_usd) \
                 FROM model_day_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((rows, msgs, sc), (1, 2, 2));
        assert!((cost - 3.0).abs() < 1e-12);
    }

    #[test]
    fn session_count_is_recomputed_across_windows() {
        let c = testdb::conn();
        testdb::project(&c, 1, "a", "claude");
        testdb::event(
            &c,
            1,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        ModelDayMartBuilder.refresh(&c, 0).unwrap();
        testdb::event(
            &c,
            2,
            None,
            1,
            "s1",
            "claude",
            "opus",
            "2026-01-01",
            (1, 1, 0, 0),
            0.0,
        );
        ModelDayMartBuilder.refresh(&c, 1).unwrap();
        let sc: i64 = c
            .query_row("SELECT session_count FROM model_day_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sc, 1);
    }

    #[test]
    fn production_recompute_is_materialized_exactly_once() {
        let c = testdb::conn();
        let sql = format!("EXPLAIN QUERY PLAN {RECOMPUTE_SESSION_COUNTS_SQL}");
        let plan: Vec<String> = c
            .prepare(&sql)
            .unwrap()
            .query_map(rusqlite::params![0, 10], |row| row.get(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        eprintln!("model-day recompute plan: {plan:#?}");
        assert_eq!(
            plan.iter()
                .filter(|detail| detail.as_str() == "MATERIALIZE recomputed")
                .count(),
            1,
            "{plan:#?}"
        );
        assert_eq!(
            plan.iter()
                .filter(|detail| detail.as_str() == "CO-ROUTINE recomputed")
                .count(),
            0,
            "{plan:#?}"
        );
    }

    #[test]
    fn all_columns_and_watermark_match_incremental_from_zero_and_rebuild() {
        let incremental = testdb::conn();
        seed_dimensions(&incremental);
        for id in 1..=4 {
            seed_model_event(&incremental, id);
        }
        assert_eq!(refresh_with_watermark(&incremental, "window-one"), 4);
        for id in 5..=8 {
            seed_model_event(&incremental, id);
        }
        assert_eq!(refresh_with_watermark(&incremental, "window-two"), 8);

        let from_zero = testdb::conn();
        seed_dimensions(&from_zero);
        for id in 1..=8 {
            seed_model_event(&from_zero, id);
        }
        assert_eq!(refresh_with_watermark(&from_zero, "window-two"), 8);
        let expected = snapshot(&incremental);
        assert_eq!(expected, snapshot(&from_zero));

        let incremental_watermark: (i64, String) = incremental
            .query_row(
                "SELECT last_event_id, last_refresh_ts FROM mart_watermark \
                 WHERE mart_name = 'model_day'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let from_zero_watermark: (i64, String) = from_zero
            .query_row(
                "SELECT last_event_id, last_refresh_ts FROM mart_watermark \
                 WHERE mart_name = 'model_day'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(incremental_watermark, (8, "window-two".to_string()));
        assert_eq!(from_zero_watermark, incremental_watermark);

        ModelDayMartBuilder
            .rebuild_from_scratch(&from_zero)
            .unwrap();
        assert_eq!(expected, snapshot(&from_zero));
        let watermark_after_rebuild: (i64, String) = from_zero
            .query_row(
                "SELECT last_event_id, last_refresh_ts FROM mart_watermark \
                 WHERE mart_name = 'model_day'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(watermark_after_rebuild, incremental_watermark);
    }
}
