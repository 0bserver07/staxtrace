use super::*;
use chrono::{Duration, TimeZone};
use sqlx::postgres::PgPoolOptions;

fn date(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    date("2026-08-17T12:00:00.000Z")
}

fn worker() -> WorkerProjection {
    WorkerProjection {
        running: true,
        started_at: Some(now()),
        last_poll_at: Some(now()),
        in_flight: 0.0,
    }
}

// These four tests adapt the four pure callbacks in the pinned queue-health
// suite. The database callback is adapted separately below.
#[test]
fn normalizes_aggregate_values_and_reports_a_live_worker() {
    let snapshot = summarize(
        Some(&json!({
            "pending":"3", "running":2, "completed":"20", "failed":1,
            "oldest_ready_lag_seconds":"4.5", "expired_leases":"0",
            "recent_terminal_failures":"1",
        })),
        WorkerProjection {
            started_at: Some(date("2026-08-17T11:00:00.000Z")),
            last_poll_at: Some(now() - Duration::seconds(1)),
            in_flight: 2.0,
            ..worker()
        },
        now(),
    );
    assert_eq!(
        snapshot,
        json!({
            "status":"ok", "pending":3, "running":2, "completed":20, "failed":1,
            "oldest_ready_lag_seconds":4.5, "expired_leases":0, "recent_terminal_failures":1,
            "worker": {
                "running":true, "started_at":"2026-08-17T11:00:00.000Z",
                "last_poll_at":"2026-08-17T11:59:59.000Z", "heartbeat_age_seconds":1,
                "heartbeat_stale":false, "in_flight":2,
            }
        })
    );
}

#[test]
fn an_expired_lease_degrades_an_otherwise_live_worker() {
    let snapshot = summarize(Some(&json!({"expired_leases":1})), worker(), now());
    assert_eq!(snapshot["status"], "degraded");
    assert_eq!(snapshot["expired_leases"], 1);
    assert_eq!(snapshot["worker"]["heartbeat_stale"], false);
}

#[test]
fn stopped_missing_and_stale_worker_heartbeats_degrade_health() {
    for worker in [
        WorkerProjection::from(&WorkerStatus::default()),
        WorkerProjection {
            last_poll_at: None,
            ..worker()
        },
        WorkerProjection {
            last_poll_at: Some(now() - Duration::milliseconds(QUEUE_WORKER_STALE_AFTER_MS + 1)),
            ..worker()
        },
    ] {
        let snapshot = summarize(Some(&json!({})), worker, now());
        assert_eq!(snapshot["status"], "degraded");
        assert_eq!(snapshot["worker"]["heartbeat_stale"], true);
    }
}

#[test]
fn missing_negative_and_invalid_aggregates_and_in_flight_are_zero() {
    let snapshot = summarize(
        Some(&json!({
            "pending":null, "running":"-2", "completed":"not-a-number",
            "oldest_ready_lag_seconds":-10,
        })),
        WorkerProjection {
            in_flight: -1.0,
            ..worker()
        },
        now(),
    );
    assert_eq!(snapshot["pending"], 0);
    assert_eq!(snapshot["running"], 0);
    assert_eq!(snapshot["completed"], 0);
    assert_eq!(snapshot["oldest_ready_lag_seconds"], 0);
    assert_eq!(snapshot["worker"]["in_flight"], 0);
}

#[test]
fn heartbeat_uses_source_milliseconds_and_inclusive_freshness_boundary() {
    let now = now() + Duration::microseconds(999);
    for (last, age, stale) in [
        (now - Duration::seconds(15), json!(15), false),
        (now - Duration::milliseconds(15_001), json!(15.001), true),
        (now + Duration::seconds(1), json!(0), false),
        // Truncate both timestamps before subtracting, not the duration.
        (now - Duration::microseconds(1_500), json!(0.001), false),
    ] {
        let snapshot = summarize(
            None,
            WorkerProjection {
                last_poll_at: Some(last),
                ..worker()
            },
            now,
        );
        assert_eq!(snapshot["worker"]["heartbeat_age_seconds"], age);
        assert_eq!(snapshot["worker"]["heartbeat_stale"], stale);
    }
    let pre_epoch = Utc.timestamp_opt(-1, 999_900_000).unwrap();
    let snapshot = summarize(
        None,
        WorkerProjection {
            started_at: Some(Utc.with_ymd_and_hms(10000, 1, 1, 0, 0, 0).unwrap()),
            last_poll_at: Some(pre_epoch),
            ..worker()
        },
        Utc.timestamp_opt(0, 100_000).unwrap(),
    );
    assert_eq!(snapshot["worker"]["heartbeat_age_seconds"], json!(0.001));
    assert_eq!(
        snapshot["worker"]["last_poll_at"],
        "1969-12-31T23:59:59.999Z"
    );
    assert_eq!(
        snapshot["worker"]["started_at"],
        "+010000-01-01T00:00:00.000Z"
    );
}

#[test]
fn executed_source_summaries_and_global_query_match() {
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/queue-health-source.json")).unwrap();
    let summaries = fixture["summary_cases"].as_array().unwrap();
    assert_eq!(summaries.len(), 29);
    for case in summaries {
        let input: Value = serde_json::from_str(case["input_json"].as_str().unwrap()).unwrap();
        let now: String = serde_json::from_str(case["now_json"].as_str().unwrap()).unwrap();
        let expected: Value =
            serde_json::from_str(case["expected_json"].as_str().unwrap()).unwrap();
        let actual = summarize(
            input.get("row"),
            fixture_worker(&input["worker"]),
            date(&now),
        );
        assert_eq!(actual, expected, "{}", case["name"]);
        // The bounded fixture values also prove integer JSON formatting and
        // the original order of the public response fields.
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            case["expected_json"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
    let queries = fixture["query_cases"].as_array().unwrap();
    assert_eq!(queries.len(), 3);
    for case in queries {
        assert_eq!(case["queries"].as_array().unwrap().len(), 1);
        assert_eq!(case["queries"][0]["sql"], HEALTH_SQL);
        assert_eq!(case["queries"][0]["params_json"], "[]");
        if case["driver_error"] == true {
            // Query failure is checked through the real HTTP host's closed
            // pool tests. This fixture comparison makes no DB execution claim.
            continue;
        }
        let rows: Vec<Value> =
            serde_json::from_str(case["driver_rows_json"].as_str().unwrap()).unwrap();
        let worker: Value = serde_json::from_str(case["worker_json"].as_str().unwrap()).unwrap();
        let now: String = serde_json::from_str(case["now_json"].as_str().unwrap()).unwrap();
        let expected: Value =
            serde_json::from_str(case["expected_json"].as_str().unwrap()).unwrap();
        assert_eq!(
            summarize(rows.first(), fixture_worker(&worker), date(&now)),
            expected,
            "{}",
            case["name"]
        );
    }
}

fn fixture_worker(value: &Value) -> WorkerProjection {
    // The production tracker supplies valid DateTimes. These recorded pure
    // source inputs additionally cover the original null/invalid-date branch;
    // this adapter is intentionally not a general JavaScript Date parser.
    let date = |key| match value.get(key).and_then(Value::as_str) {
        None | Some("" | "not-a-date") => None,
        Some(value) => Some(date(value)),
    };
    WorkerProjection {
        running: value["running"].as_bool().unwrap(),
        started_at: date("startedAt"),
        last_poll_at: date("lastPollAt"),
        in_flight: value["inFlight"].as_f64().unwrap(),
    }
}

#[tokio::test]
#[ignore = "requires a leased disposable PostgreSQL database"]
async fn global_query_counts_statuses_ready_lag_and_exact_lease_and_retention_boundaries() {
    assert_eq!(
        std::env::var("DESKROOST_TEST_DISPOSABLE").as_deref(),
        Ok("1"),
        "set DESKROOST_TEST_DISPOSABLE=1 only for a leased disposable database"
    );
    let url = std::env::var("DESKROOST_TEST_DATABASE_URL")
        .expect("set DESKROOST_TEST_DATABASE_URL to the leased disposable database");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    // This crate cannot install the schema (the schema crate builds on it), so the test checks that it is there
    // and says what to do, instead of failing on the first query.
    let installed: bool = sqlx::query_scalar("SELECT to_regclass('public.job_queue') IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        installed,
        "this database has no schema yet. Install it first, for example with \
         `cargo test -p deskroost-db --test schema -- --ignored` or `deskroost-server --migrate-only`, \
         then run this test again"
    );
    let mut transaction = pool.begin().await.unwrap();
    // Prevent another worker from changing the baseline during the global
    // aggregate assertions. All fixture writes roll back, including on panic.
    sqlx::query("LOCK TABLE job_queue IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *transaction)
        .await
        .unwrap();
    let clock: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    let worker = WorkerStatus {
        running: true,
        started_at: Some(clock),
        last_poll_at: Some(clock),
        in_flight: 0,
    };
    let before = query_snapshot(&mut *transaction, &worker, clock)
        .await
        .unwrap();
    let prefix = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO job_queue(id,org_id,queue,name,data,status,run_at,lock_expires_at,completed_at)
        SELECT $1 || ':' || fixture.name, fixture.org_id, fixture.queue, fixture.name, '{}', fixture.status,
               now() + fixture.run_offset * interval '1 second',
               now() + fixture.lease_offset * interval '1 second',
               now() + fixture.completed_offset * interval '1 second'
        FROM (VALUES
          ('ready',       'health-org-one', 'health-one', 'pending',   -60, NULL, NULL),
          ('ready-now',   'health-org-two', 'health-two', 'pending',     0, NULL, NULL),
          ('future',     NULL,             'health-two', 'pending',  3600, NULL, NULL),
          ('expired',    'health-org-one', 'health-one', 'running',  -300,   -1, NULL),
          ('missing',    'health-org-two', 'health-two', 'running',  -300, NULL, NULL),
          ('lease-now',  NULL,             'health-two', 'running',  -300,    0, NULL),
          ('live',       NULL,             'health-two', 'running',  -300, 3600, NULL),
          ('done',       NULL,             'health-two', 'completed',-300, NULL,    0),
          ('recent',     NULL,             'health-one', 'failed',   -300, NULL,    0),
          ('boundary',   NULL,             'health-two', 'failed',   -300, NULL,-86400),
          ('old',        NULL,             'health-two', 'failed',   -300, NULL,-86401),
          ('undated',    NULL,             'health-two', 'failed',   -300, NULL, NULL),
          ('other',      NULL,             'health-two', 'canceled', -300, NULL,    0)
        ) AS fixture(name,org_id,queue,status,run_offset,lease_offset,completed_offset)")
        .bind(&prefix).execute(&mut *transaction).await.unwrap();
    let snapshot = query_snapshot(&mut *transaction, &worker, clock)
        .await
        .unwrap();
    for (key, increment) in [
        ("pending", 3),
        ("running", 4),
        ("completed", 1),
        ("failed", 4),
        ("expired_leases", 3),
        ("recent_terminal_failures", 2),
    ] {
        assert_eq!(
            snapshot[key].as_u64().unwrap(),
            before[key].as_u64().unwrap() + increment,
            "{key}"
        );
    }
    assert_eq!(
        snapshot["oldest_ready_lag_seconds"].as_f64().unwrap(),
        before["oldest_ready_lag_seconds"]
            .as_f64()
            .unwrap()
            .max(60.0),
    );
    assert_eq!(snapshot["status"], "degraded");
    assert_eq!(snapshot["worker"]["heartbeat_stale"], false);
    transaction.rollback().await.unwrap();
    pool.close().await;
}
