//! Port of `python-legacy: etl/marts/daily.py` — the
//! `(day, project_id, provider, model, speed)` rollup.
//!
//! Additive: tokens, `message_count` and cost are summed into the existing row
//! by `ON CONFLICT DO UPDATE`, which is safe because the watermark guarantees no
//! `event_id` is processed twice.
//!
//! `session_count` is the exception, and the reason the second statement
//! exists. `COUNT(DISTINCT session_id)` is not additive across refresh windows:
//! a session producing events on day D in two windows would be counted twice
//! (1 + 1 = 2 instead of 1). Python follows option (a) from the spec — after the
//! additive upsert, recompute `session_count` from the *full* events table for
//! the keys this window touched. Both statements are ported verbatim, including
//! the row-value `IN` predicate in the UPDATE's WHERE.

use anyhow::Result;
use rusqlite::Connection;

use super::{MartBuilder, max_event_id};

/// Production statement shared with the query-plan regression below so the
/// plan proof cannot silently drift onto a favorable duplicate query.
const RECOMPUTE_SESSION_COUNT_SQL: &str = r"
    WITH affected AS (
        SELECT DISTINCT day, project_id, provider, model, speed
        FROM usage_events
        WHERE id > ? AND id <= ?
    ),
    recomputed AS MATERIALIZED (
        SELECT
            e.day, e.project_id, e.provider, e.model, e.speed,
            COUNT(DISTINCT e.session_id) AS sc
        FROM usage_events e
        JOIN affected a USING (day, project_id, provider, model, speed)
        GROUP BY e.day, e.project_id, e.provider, e.model, e.speed
    )
    UPDATE daily_mart
       SET session_count = (
           SELECT sc FROM recomputed r
           WHERE r.day = daily_mart.day
             AND r.project_id = daily_mart.project_id
             AND r.provider = daily_mart.provider
             AND r.model = daily_mart.model
             AND r.speed = daily_mart.speed
       )
     WHERE (day, project_id, provider, model, speed) IN (
           SELECT day, project_id, provider, model, speed FROM affected
       )
    ";

/// Per-`(day, project, provider, model, speed)` cost + token rollup.
pub struct DailyMartBuilder;

impl MartBuilder for DailyMartBuilder {
    fn name(&self) -> &'static str {
        "daily"
    }

    fn refresh(&self, conn: &Connection, since_event_id: i64) -> Result<i64> {
        let max_id = max_event_id(conn)?;
        if max_id <= since_event_id {
            return Ok(since_event_id);
        }

        // ── additive upsert for SUM/COUNT(*) columns ──────────────────────
        conn.execute(
            r"
            INSERT INTO daily_mart (
                day, project_id, provider, model, speed,
                input_tokens, output_tokens, cache_read, cache_create,
                message_count, session_count, cost_usd
            )
            SELECT
                day, project_id, provider, model, speed,
                SUM(input_tokens),
                SUM(output_tokens),
                SUM(cache_read_tokens),
                SUM(cache_create_tokens),
                COUNT(*),
                COUNT(DISTINCT session_id),
                SUM(cost_usd)
            FROM usage_events
            WHERE id > ? AND id <= ?
            GROUP BY day, project_id, provider, model, speed
            ON CONFLICT (day, project_id, provider, model, speed) DO UPDATE SET
                input_tokens  = input_tokens  + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_read    = cache_read    + excluded.cache_read,
                cache_create  = cache_create  + excluded.cache_create,
                message_count = message_count + excluded.message_count,
                session_count = session_count + excluded.session_count,
                cost_usd      = cost_usd      + excluded.cost_usd
            ",
            rusqlite::params![since_event_id, max_id],
        )?;

        // ── recompute session_count for affected keys ─────────────────────
        conn.execute(
            RECOMPUTE_SESSION_COUNT_SQL,
            rusqlite::params![since_event_id, max_id],
        )?;

        Ok(max_id)
    }

    fn rebuild_from_scratch(&self, conn: &Connection) -> Result<()> {
        conn.execute("DELETE FROM daily_mart", [])?;
        self.refresh(conn, 0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{testdb, watermark};
    use super::*;

    #[derive(Clone, Copy)]
    struct FixtureEvent {
        id: i64,
        project_id: i64,
        session_id: &'static str,
        provider: &'static str,
        model: &'static str,
        speed: &'static str,
        day: &'static str,
        tokens: (i64, i64, i64, i64),
        cost: f64,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct DailyRow {
        day: String,
        project_id: i64,
        provider: String,
        model: String,
        speed: String,
        input_tokens: i64,
        output_tokens: i64,
        cache_read: i64,
        cache_create: i64,
        message_count: i64,
        session_count: i64,
        cost_bits: u64,
    }

    fn insert_fixture_event(c: &Connection, event: FixtureEvent) {
        testdb::event(
            c,
            event.id,
            None,
            event.project_id,
            event.session_id,
            event.provider,
            event.model,
            event.day,
            event.tokens,
            event.cost,
        );
        c.execute(
            "UPDATE usage_events SET speed = ? WHERE id = ?",
            rusqlite::params![event.speed, event.id],
        )
        .unwrap();
    }

    fn daily_snapshot(c: &Connection) -> Vec<DailyRow> {
        let mut stmt = c
            .prepare(
                "SELECT day, project_id, provider, model, speed, input_tokens, \
                 output_tokens, cache_read, cache_create, message_count, session_count, cost_usd \
                 FROM daily_mart ORDER BY day, project_id, provider, model, speed",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(DailyRow {
                day: row.get(0)?,
                project_id: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                speed: row.get(4)?,
                input_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                cache_read: row.get(7)?,
                cache_create: row.get(8)?,
                message_count: row.get(9)?,
                session_count: row.get(10)?,
                cost_bits: f64::to_bits(row.get(11)?),
            })
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    fn refresh_daily_with_watermark(c: &Connection, now: &str) -> i64 {
        let since = watermark::get_watermark(c, "daily").unwrap();
        let next = DailyMartBuilder.refresh(c, since).unwrap();
        watermark::set_watermark(c, "daily", next, now).unwrap();
        next
    }

    fn daily_watermark(c: &Connection) -> (i64, String) {
        c.query_row(
            "SELECT last_event_id, last_refresh_ts FROM mart_watermark WHERE mart_name='daily'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn production_recomputed_cte_is_materialized_once() {
        let c = testdb::conn();
        let sql = format!("EXPLAIN QUERY PLAN {RECOMPUTE_SESSION_COUNT_SQL}");
        let mut stmt = c.prepare(&sql).unwrap();
        let details: Vec<String> = stmt
            .query_map(rusqlite::params![0, 1], |row| row.get(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();

        assert_eq!(
            details
                .iter()
                .filter(|detail| detail.contains("MATERIALIZE recomputed"))
                .count(),
            1,
            "production plan must materialize recomputed exactly once: {details:#?}"
        );
        assert!(
            details
                .iter()
                .all(|detail| !detail.contains("CO-ROUTINE recomputed")),
            "production plan must not execute recomputed as a co-routine: {details:#?}"
        );
    }

    #[test]
    fn from_zero_matches_two_incremental_windows_all_columns_and_watermark() {
        const PREFIX_LEN: usize = 5;
        const EVENTS: [FixtureEvent; 10] = [
            FixtureEvent {
                id: 1,
                project_id: 1,
                session_id: "shared",
                provider: "provider-a",
                model: "model-a",
                speed: "standard",
                day: "2026-01-01",
                tokens: (1, 2, 3, 4),
                cost: 0.25,
            },
            FixtureEvent {
                id: 2,
                project_id: 1,
                session_id: "prefix-second",
                provider: "provider-a",
                model: "model-a",
                speed: "standard",
                day: "2026-01-01",
                tokens: (5, 6, 7, 8),
                cost: 0.5,
            },
            FixtureEvent {
                id: 3,
                project_id: 2,
                session_id: "unaffected",
                provider: "provider-b",
                model: "model-b",
                speed: "fast",
                day: "2026-01-02",
                tokens: (9, 10, 11, 12),
                cost: 1.0,
            },
            FixtureEvent {
                id: 4,
                project_id: 2,
                session_id: "other-key-first",
                provider: "provider-b",
                model: "model-c",
                speed: "standard",
                day: "2026-01-01",
                tokens: (13, 14, 15, 16),
                cost: 2.0,
            },
            FixtureEvent {
                id: 5,
                project_id: 1,
                session_id: "shared",
                provider: "provider-a",
                model: "model-a",
                speed: "standard",
                day: "2026-01-01",
                tokens: (17, 18, 19, 20),
                cost: 4.0,
            },
            FixtureEvent {
                id: 6,
                project_id: 1,
                session_id: "shared",
                provider: "provider-a",
                model: "model-a",
                speed: "standard",
                day: "2026-01-01",
                tokens: (21, 22, 23, 24),
                cost: 8.0,
            },
            FixtureEvent {
                id: 7,
                project_id: 1,
                session_id: "tail-third",
                provider: "provider-a",
                model: "model-a",
                speed: "standard",
                day: "2026-01-01",
                tokens: (25, 26, 27, 28),
                cost: 16.0,
            },
            FixtureEvent {
                id: 8,
                project_id: 1,
                session_id: "new-day",
                provider: "provider-a",
                model: "model-b",
                speed: "fast",
                day: "2026-01-02",
                tokens: (29, 30, 31, 32),
                cost: 32.0,
            },
            FixtureEvent {
                id: 9,
                project_id: 2,
                session_id: "other-key-second",
                provider: "provider-b",
                model: "model-c",
                speed: "standard",
                day: "2026-01-01",
                tokens: (33, 34, 35, 36),
                cost: 64.0,
            },
            FixtureEvent {
                id: 10,
                project_id: 2,
                session_id: "third-day",
                provider: "provider-a",
                model: "model-a",
                speed: "fast",
                day: "2026-01-03",
                tokens: (37, 38, 39, 40),
                cost: 128.0,
            },
        ];

        let incremental = testdb::conn();
        let from_zero = testdb::conn();
        for c in [&incremental, &from_zero] {
            testdb::project(c, 1, "project-a", "provider-a");
            testdb::project(c, 2, "project-b", "provider-b");
        }

        for event in &EVENTS[..PREFIX_LEN] {
            insert_fixture_event(&incremental, *event);
        }
        assert_eq!(refresh_daily_with_watermark(&incremental, "window-one"), 5);
        let unaffected_before = daily_snapshot(&incremental)
            .into_iter()
            .find(|row| row.session_count == 1 && row.model == "model-b")
            .expect("unaffected prefix key");

        for event in &EVENTS[PREFIX_LEN..] {
            insert_fixture_event(&incremental, *event);
        }
        assert_eq!(refresh_daily_with_watermark(&incremental, "window-two"), 10);

        for event in EVENTS {
            insert_fixture_event(&from_zero, event);
        }
        assert_eq!(refresh_daily_with_watermark(&from_zero, "window-two"), 10);

        let incremental_rows = daily_snapshot(&incremental);
        let from_zero_rows = daily_snapshot(&from_zero);
        assert_eq!(incremental_rows, from_zero_rows);
        assert_eq!(
            daily_watermark(&incremental),
            daily_watermark(&from_zero),
            "from-zero and incremental daily watermarks must agree"
        );

        let unaffected_after = incremental_rows
            .iter()
            .find(|row| {
                row.day == "2026-01-02"
                    && row.project_id == 2
                    && row.provider == "provider-b"
                    && row.model == "model-b"
                    && row.speed == "fast"
            })
            .expect("unaffected key after the second window");
        assert_eq!(unaffected_after, &unaffected_before);

        DailyMartBuilder.rebuild_from_scratch(&from_zero).unwrap();
        assert_eq!(
            daily_snapshot(&from_zero),
            incremental_rows,
            "full rebuild must retain the same exact daily rows"
        );
    }

    #[test]
    fn nothing_new_leaves_the_watermark_where_it_was() {
        let c = testdb::conn();
        assert_eq!(DailyMartBuilder.refresh(&c, 7).unwrap(), 7);
    }

    #[test]
    fn distinct_session_count_is_recomputed_not_summed_across_windows() {
        // The trap the second statement exists for: one session, two refresh
        // windows, same day. Naive addition would report 2.
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
        testdb::event(
            &c,
            1,
            None,
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (1, 2, 0, 0),
            0.5,
        );
        assert_eq!(DailyMartBuilder.refresh(&c, 0).unwrap(), 1);

        testdb::event(
            &c,
            2,
            None,
            1,
            "s1",
            "claude",
            "m",
            "2026-01-01",
            (3, 4, 0, 0),
            0.25,
        );
        assert_eq!(DailyMartBuilder.refresh(&c, 1).unwrap(), 2);

        let (msgs, sessions, cost, tin): (i64, i64, f64, i64) = c
            .query_row(
                "SELECT message_count, session_count, cost_usd, input_tokens FROM daily_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(msgs, 2);
        assert_eq!(sessions, 1, "COUNT(DISTINCT) must be recomputed, not added");
        assert!((cost - 0.75).abs() < 1e-12);
        assert_eq!(tin, 4);
    }

    #[test]
    fn a_rebuild_is_bit_reproducible_where_an_incremental_path_need_not_be() {
        // The wave-3 gate compares rebuild against rebuild, and this is why.
        // The additive path sums each window separately and then adds the
        // partial sums; a rebuild sums every row in one `SUM`. Float addition
        // is not associative, so the two can legitimately differ in the last
        // bits — but two rebuilds of the same events cannot.
        let c = testdb::conn();
        testdb::project(&c, 1, "p", "claude");
        let cost = |i: i64| 0.1 * f64::from(u32::try_from(i).unwrap());
        for i in 1..=3 {
            testdb::event(
                &c,
                i,
                None,
                1,
                "s1",
                "claude",
                "m",
                "2026-01-01",
                (i, i, 0, 0),
                cost(i),
            );
        }
        assert_eq!(DailyMartBuilder.refresh(&c, 0).unwrap(), 3);
        for i in 4..=6 {
            testdb::event(
                &c,
                i,
                None,
                1,
                "s2",
                "claude",
                "m",
                "2026-01-01",
                (i, i, 0, 0),
                cost(i),
            );
        }
        assert_eq!(DailyMartBuilder.refresh(&c, 3).unwrap(), 6);

        let (msgs, sessions): (i64, i64) = c
            .query_row(
                "SELECT message_count, session_count FROM daily_mart",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((msgs, sessions), (6, 2));

        DailyMartBuilder.rebuild_from_scratch(&c).unwrap();
        let first: f64 = c
            .query_row("SELECT cost_usd FROM daily_mart", [], |r| r.get(0))
            .unwrap();
        DailyMartBuilder.rebuild_from_scratch(&c).unwrap();
        let second: f64 = c
            .query_row("SELECT cost_usd FROM daily_mart", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first.to_bits(), second.to_bits(), "{first} vs {second}");
    }
}
