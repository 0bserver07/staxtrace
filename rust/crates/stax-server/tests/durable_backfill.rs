#![cfg(feature = "rigwright-durable-work")]

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deskroost_jobs::runtime;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use stax_server::services::durable_backfill::{BackfillQueue, Outcome, SCHEMA};
use stax_server::{AppState, Config};

struct Fixture {
    admin: PgPool,
    pool: PgPool,
    queue: BackfillQueue,
    schema: String,
    dir: PathBuf,
    state: AppState,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let Ok(url) = std::env::var("STAX_DURABLE_WORK_TEST_DATABASE_URL") else {
            eprintln!(
                "SKIP durable backfill PostgreSQL test: set STAX_DURABLE_WORK_TEST_DATABASE_URL and STAX_DURABLE_WORK_TEST_DISPOSABLE=1"
            );
            return None;
        };
        assert_eq!(
            std::env::var("STAX_DURABLE_WORK_TEST_DISPOSABLE").as_deref(),
            Ok("1"),
            "the PostgreSQL database must be explicitly disposable"
        );
        let options = PgConnectOptions::from_str(&url).expect("invalid disposable PostgreSQL URL");
        assert!(
            matches!(options.get_host(), "127.0.0.1" | "localhost" | "::1"),
            "these tests require a private loopback PostgreSQL instance"
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone())
            .await
            .expect("connect to disposable PostgreSQL");
        let schema = format!(
            "stax_rp139_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", schema.as_str()), ("timezone", "UTC")]))
            .await
            .unwrap();
        sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT table_name FROM information_schema.tables WHERE table_schema=current_schema() ORDER BY table_name"
        ).fetch_all(&pool).await.unwrap();
        assert_eq!(
            tables,
            ["job_queue"],
            "no Rigwright application schema is installed"
        );

        let dir = std::env::temp_dir().join(&schema);
        std::fs::create_dir(&dir).unwrap();
        let store = dir.join("store.db");
        let conn = rusqlite::Connection::open(&store).unwrap();
        conn.execute_batch(stax_etl::backfill::testdb::SCHEMA)
            .unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id,provider,slug,display_name) VALUES (1,'claude','p','p');
             INSERT INTO sessions (id,project_id,session_id) VALUES (1,1,'s1');
             INSERT INTO messages (id,session_fk,seq,timestamp,role,model,input_tokens,output_tokens)
             VALUES (1,1,0,'2026-01-01T00:00:00+00:00','assistant','claude-sonnet-4-5-20250929',1000,100);"
        ).unwrap();
        drop(conn);
        let state = AppState::new(store, dir.clone(), Config::default());
        let queue = BackfillQueue::new(pool.clone(), "fixture-store".into()).unwrap();
        Some(Self {
            admin,
            pool,
            queue,
            schema,
            dir,
            state,
        })
    }

    fn sqlite_count(&self, table: &str) -> i64 {
        self.state
            .connect()
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    async fn status(&self) -> (String, i32) {
        sqlx::query_as("SELECT status,attempts FROM job_queue")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn close(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[tokio::test]
async fn enqueue_claim_complete_runs_the_real_incremental_backfill() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.queue.submit("refresh-1").await.unwrap();
    f.queue.submit("refresh-1").await.unwrap();
    let outcome = f.queue.run_once(f.state.clone(), "worker").await.unwrap();
    assert!(matches!(
        outcome,
        Outcome::Completed {
            events_inserted: 1,
            ..
        }
    ));
    assert_eq!(f.status().await, ("completed".into(), 1));
    assert_eq!(f.sqlite_count("usage_events"), 1);
    assert_eq!(f.sqlite_count("daily_mart"), 1);
    assert_eq!(f.sqlite_count("mart_watermark"), 8);
    assert_eq!(
        f.queue.run_once(f.state.clone(), "worker").await.unwrap(),
        Outcome::Idle
    );
    f.close().await;
}

#[tokio::test]
async fn a_failed_real_backfill_retries_after_backoff() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.state
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE messages RENAME TO unavailable_messages")
        .unwrap();
    f.queue.submit("retry").await.unwrap();
    assert!(matches!(
        f.queue.run_once(f.state.clone(), "first").await.unwrap(),
        Outcome::AttemptFailed { .. }
    ));
    assert_eq!(f.status().await, ("pending".into(), 1));
    let backed_off: bool = sqlx::query_scalar("SELECT run_at>now() FROM job_queue")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(backed_off);
    assert!(f.queue.claim("too-early").await.unwrap().is_none());
    f.state
        .connect()
        .unwrap()
        .execute_batch("ALTER TABLE unavailable_messages RENAME TO messages")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(matches!(
        f.queue
            .run_once(f.state.clone(), "retry-worker")
            .await
            .unwrap(),
        Outcome::Completed {
            events_inserted: 1,
            ..
        }
    ));
    assert_eq!(f.status().await, ("completed".into(), 2));
    assert_eq!(f.sqlite_count("usage_events"), 1);
    f.close().await;
}

#[tokio::test]
async fn concurrent_workers_cannot_double_claim_one_request() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.queue.submit("race").await.unwrap();
    let (a, b) = tokio::join!(f.queue.claim("a"), f.queue.claim("b"));
    let claims: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
    assert_eq!(claims.len(), 1);
    assert_eq!(f.status().await, ("running".into(), 1));
    let job = &claims[0];
    assert!(
        runtime::complete_job(&f.pool, &job.id, &job.lock_token)
            .await
            .unwrap()
    );
    assert!(
        !runtime::complete_job(&f.pool, &job.id, &job.lock_token)
            .await
            .unwrap()
    );
    f.close().await;
}

#[tokio::test]
async fn crash_after_sqlite_effects_recovers_without_double_counting() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.queue.submit("crash-gap").await.unwrap();
    let stale = f.queue.claim("crashed-worker").await.unwrap().unwrap();
    // Simulate effects committed immediately before the process dies, with no
    // PostgreSQL settlement. Recovery must rerun actual ETL safely.
    {
        let conn = f.state.connect().unwrap();
        let ctx = stax_etl::normalize::NormalizeContext::new(
            stax_server::pricing::engine(&conn, f.state.package_dir()).unwrap(),
        );
        assert_eq!(
            stax_etl::backfill::backfill(&conn, &ctx, false, "2026-01-01T00:00:00+00:00")
                .unwrap()
                .events_inserted,
            1
        );
    }
    sqlx::query("UPDATE job_queue SET lock_expires_at=now()-interval '1 second'")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        !runtime::complete_job(&f.pool, &stale.id, &stale.lock_token)
            .await
            .unwrap()
    );
    assert_eq!(
        f.queue
            .run_once(f.state.clone(), "recovery-sweep")
            .await
            .unwrap(),
        Outcome::Idle,
        "stale recovery schedules a five-second cooldown"
    );
    assert_eq!(f.status().await, ("pending".into(), 1));
    tokio::time::sleep(Duration::from_millis(5100)).await;
    assert!(matches!(
        f.queue
            .run_once(f.state.clone(), "recovered-worker")
            .await
            .unwrap(),
        Outcome::Completed {
            events_inserted: 0,
            ..
        }
    ));
    assert_eq!(f.status().await, ("completed".into(), 2));
    assert_eq!(f.sqlite_count("usage_events"), 1);
    assert_eq!(f.sqlite_count("daily_mart"), 1);
    assert!(
        !runtime::complete_job(&f.pool, &stale.id, &stale.lock_token)
            .await
            .unwrap()
    );
    f.close().await;
}

#[tokio::test]
async fn separate_requests_for_one_store_are_serialized_and_idempotent() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.queue.submit("one").await.unwrap();
    f.queue.submit("two").await.unwrap();
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.state.store_path().with_extension("durable-work.lock"))
        .unwrap();
    guard.try_lock().unwrap();
    assert_eq!(
        f.queue.run_once(f.state.clone(), "blocked").await.unwrap(),
        Outcome::StoreBusy
    );
    let attempts: i64 = sqlx::query_scalar("SELECT sum(attempts)::bigint FROM job_queue")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "the local lock is acquired before claiming");
    drop(guard);
    assert!(matches!(
        f.queue.run_once(f.state.clone(), "one").await.unwrap(),
        Outcome::Completed {
            events_inserted: 1,
            ..
        }
    ));
    assert!(matches!(
        f.queue.run_once(f.state.clone(), "two").await.unwrap(),
        Outcome::Completed {
            events_inserted: 0,
            ..
        }
    ));
    let completed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM job_queue WHERE status='completed'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(completed, 2);
    assert_eq!(f.sqlite_count("usage_events"), 1);
    f.close().await;
}
