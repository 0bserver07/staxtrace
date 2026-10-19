//! Optional durable scheduling for the existing incremental ETL backfill.
//!
//! PostgreSQL owns scheduling; SQLite owns effects. A crash between them can
//! repeat a backfill. The existing unique-event index and mart watermarks make
//! incremental work resumable. Forced rebuilds are deliberately unsupported.

use std::fs::{File, OpenOptions, TryLockError};
use std::time::Duration;

use anyhow::{Result, ensure};
use deskroost_jobs::runtime::{self, DequeueOptions, DequeuedJob, FailJobOptions};
use deskroost_jobs::{EnqueueOptions, enqueue};
use serde_json::json;
use sqlx::PgPool;
use stax_core::queries::pytime;

use crate::state::AppState;

pub const QUEUE: &str = "staxtrace-incremental-backfill";
pub const JOB: &str = "staxtrace-incremental-backfill-v1";
pub const SCHEMA: &str =
    include_str!("../../../../../vendor/rigwright-durable-work/deskroost-jobs-0.1.0/schema.sql");

/// Use a dedicated PostgreSQL schema and UTC sessions for every connection.
/// This primitive's stale-job sweeper operates on the whole visible queue table.
#[derive(Clone)]
pub struct BackfillQueue {
    pool: PgPool,
    store_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Idle,
    StoreBusy,
    Completed {
        job_id: String,
        events_inserted: u64,
    },
    AttemptFailed {
        job_id: String,
    },
    LeaseLost {
        job_id: String,
    },
}

impl BackfillQueue {
    pub fn new(pool: PgPool, store_id: String) -> Result<Self> {
        ensure!(!store_id.trim().is_empty(), "store-id must not be empty");
        Ok(Self { pool, store_id })
    }

    /// An explicit request identity deduplicates resubmission, including after
    /// completion. Use a new identity for a new refresh of the same store.
    pub async fn submit(&self, request_id: &str) -> Result<()> {
        ensure!(
            !request_id.trim().is_empty(),
            "request-id must not be empty"
        );
        enqueue(
            &self.pool,
            QUEUE,
            JOB,
            &json!({"store_id": self.store_id}),
            &EnqueueOptions {
                org_id: Some(None),
                max_attempts: Some(3.0),
                // JSON tuple encoding avoids collisions between ids containing ':' etc.
                dedupe_key: Some(serde_json::to_string(&(
                    "staxtrace:incremental-backfill:v1",
                    &self.store_id,
                    request_id,
                ))?),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    /// Low-level scheduling claim. Execute through `run_once`, which also
    /// serializes cooperating workers against the local SQLite store.
    pub async fn claim(&self, worker_id: &str) -> Result<Option<DequeuedJob>> {
        Ok(runtime::dequeue_job(
            &self.pool,
            QUEUE,
            &DequeueOptions {
                locked_by: Some(worker_id.to_owned()),
                job_name: Some(JOB.to_owned()),
                data_match: Some(("store_id".to_owned(), self.store_id.clone())),
                ..Default::default()
            },
        )
        .await?)
    }

    /// Run real ETL work, renew its lease, then settle or retry its outcome.
    ///
    /// The local file lock stays inside the blocking task, even if this future
    /// is dropped or a lease is lost. A replacement worker cannot overlap it
    /// on the same local store. Callers should still await in-flight work at
    /// shutdown. Other writers (the original HTTP/CLI paths) do not use this
    /// lock: do not run those concurrently with this opt-in worker.
    pub async fn run_once(&self, state: AppState, worker_id: &str) -> Result<Outcome> {
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(state.store_path().with_extension("durable-work.lock"))?;
        match guard.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(Outcome::StoreBusy),
            Err(error) => return Err(error.into()),
        }
        runtime::cleanup_stale_jobs(&self.pool).await?;
        let Some(job) = self.claim(worker_id).await? else {
            return Ok(Outcome::Idle);
        };

        let mut work = tokio::task::spawn_blocking(move || execute(state, guard));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut lease_lost = false;
        let result = loop {
            tokio::select! {
                result = &mut work => break result,
                _ = heartbeat.tick(), if !lease_lost => {
                    // A failed renewal cannot cancel a blocking SQLite operation.
                    // Wait for it with the file lock still held, and never settle
                    // with a token whose ownership could not be verified.
                    lease_lost = !matches!(
                        runtime::renew_job_lease(&self.pool, &job.id, &job.lock_token, None).await,
                        Ok(true)
                    );
                }
            }
        };
        if lease_lost {
            return Ok(Outcome::LeaseLost { job_id: job.id });
        }
        match result {
            Ok(Ok(events_inserted)) => {
                if runtime::complete_job(&self.pool, &job.id, &job.lock_token).await? {
                    Ok(Outcome::Completed {
                        job_id: job.id,
                        events_inserted,
                    })
                } else {
                    Ok(Outcome::LeaseLost { job_id: job.id })
                }
            }
            // Keep paths and connection details out of queue error strings.
            _ => {
                if runtime::fail_job(
                    &self.pool,
                    &job.id,
                    &job.lock_token,
                    "staxtrace incremental backfill failed",
                    &FailJobOptions::default(),
                )
                .await?
                {
                    // Retry/terminal status remains authoritative in PostgreSQL.
                    Ok(Outcome::AttemptFailed { job_id: job.id })
                } else {
                    Ok(Outcome::LeaseLost { job_id: job.id })
                }
            }
        }
    }
}

fn execute(state: AppState, _guard: File) -> Result<u64> {
    let conn = state.connect()?;
    let engine = crate::pricing::engine(&conn, state.package_dir())?;
    let ctx = stax_etl::normalize::NormalizeContext::new(engine);
    let report = stax_etl::backfill::backfill(
        &conn,
        &ctx,
        false,
        &pytime::isoformat_utc(pytime::now_micros()),
    )?;
    Ok(report.events_inserted)
}
