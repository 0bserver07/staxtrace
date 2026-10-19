//! Dead letters: jobs that failed for good, and the operator's replay of one (Deskroost, 2026-10-10).
//!
//! A job that keeps failing is isolated by the queue itself: after its last attempt it is `failed`, and no claim
//! selects it again, so it cannot hold a worker or the jobs behind it ([`crate::runtime::fail_job`] and the
//! stale-lease sweep). This module is what an operator does next: look at what failed and why, and put a job back.
//!
//! **Looking.** [`list`] returns the failed jobs of every workspace, newest failure first, with the stored payload
//! and error as they are. They can hold credentials, so the caller that shows them to a person redacts both first
//! (the server's route does). Each dead letter carries its replay history.
//!
//! **Replaying.** [`replay`] puts one failed job back in its queue. It is the same job: the same id, queue, name,
//! workspace and payload. The caller cannot change any of them, so a replay cannot be used to run something that
//! was never queued or to move work to another workspace, and the job's consumer checks what it checks on any
//! attempt. Three more rules keep a replay inside what was granted originally:
//!
//! - The count of attempts is kept and one more attempt is allowed (`max_attempts = attempts + 1`). A consumer that
//!   refuses to do a thing twice by looking at the attempt count, as the factory executor does, still refuses.
//! - A job whose workspace no longer exists is not replayed (`WorkspaceGone`).
//! - A scheduled job (one with a `cron_key`) is not replayed: its schedule queues the next occurrence itself, and a
//!   second active occurrence is not allowed.
//!
//! **Provenance.** In the transaction that re-arms the job, an `audit_log` entry `job.replayed` records the job, who
//! replayed it, an optional note, and the failure it was replayed from: the attempt count, the time of the failure
//! and the error (redacted by the caller's function). The entry outlives the job row, which the retention sweep
//! removes a week after the job ends. A job without a workspace is recorded under the empty workspace id.
use chrono::NaiveDateTime;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Dead letters returned when the caller names no limit, and the most a caller can ask for.
pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;
/// The audit action of a replay and the entity type of its entries.
pub const REPLAY_ACTION: &str = "job.replayed";
pub const JOB_ENTITY: &str = "job";

/// A job that failed for good, as stored.
#[derive(Clone, Debug)]
pub struct DeadLetter {
    pub id: String,
    pub queue: String,
    pub name: String,
    pub org_id: Option<String>,
    pub attempts: i32,
    pub max_attempts: i32,
    pub created_at: NaiveDateTime,
    pub started_at: Option<NaiveDateTime>,
    pub failed_at: Option<NaiveDateTime>,
    /// The last error, unredacted.
    pub error: Option<String>,
    /// The payload, unredacted.
    pub data: Value,
    pub scheduled: bool,
    /// Earlier replays of this job, oldest first.
    pub replays: Vec<Replay>,
}

/// One replay of a job, from the audit log.
#[derive(Clone, Debug, PartialEq)]
pub struct Replay {
    pub at: NaiveDateTime,
    pub by: String,
    pub note: Option<String>,
    /// The attempts the job had made when it was replayed.
    pub attempts: i64,
    pub failed_at: Option<String>,
    /// The error it was replayed from, as redacted when the replay was recorded.
    pub error: Option<String>,
}

impl Replay {
    pub fn to_json(&self) -> Value {
        json!({
            "at": self.at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "by": self.by,
            "note": self.note,
            "attempts": self.attempts,
            "failed_at": self.failed_at,
            "error": self.error,
        })
    }
}

fn dead_letter(row: &sqlx::postgres::PgRow) -> Result<DeadLetter, sqlx::Error> {
    Ok(DeadLetter {
        id: row.try_get("id")?,
        queue: row.try_get("queue")?,
        name: row.try_get("name")?,
        org_id: row.try_get("org_id")?,
        attempts: row.try_get("attempts")?,
        max_attempts: row.try_get("max_attempts")?,
        created_at: row.try_get("created_at")?,
        started_at: row.try_get("started_at")?,
        failed_at: row.try_get("completed_at")?,
        error: row.try_get("error")?,
        data: row.try_get("data")?,
        scheduled: row.try_get::<Option<String>, _>("cron_key")?.is_some(),
        replays: Vec::new(),
    })
}

const COLUMNS: &str = "id,queue,name,org_id,attempts,max_attempts,created_at,started_at,completed_at,error,data,cron_key";

/// The replays of the given jobs, oldest first, by job.
async fn replays_of<'e, E>(
    executor: E,
    jobs: &[String],
) -> Result<std::collections::HashMap<String, Vec<Replay>>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT entity_id, actor_id, before_state, metadata, created_at FROM audit_log \
         WHERE entity_type = $1 AND action = $2 AND entity_id = ANY($3) ORDER BY created_at, id",
    )
    .bind(JOB_ENTITY)
    .bind(REPLAY_ACTION)
    .bind(jobs)
    .fetch_all(executor)
    .await?;
    let mut by_job: std::collections::HashMap<String, Vec<Replay>> = Default::default();
    for row in rows {
        let before: Option<Value> = row.try_get("before_state")?;
        let metadata: Option<Value> = row.try_get("metadata")?;
        let before = before.unwrap_or(Value::Null);
        let text =
            |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        by_job
            .entry(row.try_get("entity_id")?)
            .or_default()
            .push(Replay {
                at: row.try_get("created_at")?,
                by: row.try_get("actor_id")?,
                note: metadata
                    .as_ref()
                    .and_then(|metadata| text(metadata, "note")),
                attempts: before.get("attempts").and_then(Value::as_i64).unwrap_or(0),
                failed_at: text(&before, "failed_at"),
                error: text(&before, "error"),
            });
    }
    Ok(by_job)
}

/// The failed jobs, newest failure first: of one queue when `queue` is given, else of every queue. `limit` is
/// clamped to 1..=[`MAX_LIMIT`].
pub async fn list(
    pool: &PgPool,
    queue: Option<&str>,
    limit: i64,
) -> Result<Vec<DeadLetter>, sqlx::Error> {
    list_scoped_on(&mut *pool.acquire().await?, queue, None, limit).await
}

/// Apply operator queue/workspace scopes in SQL before the page is limited.
pub async fn list_scoped_on(
    connection: &mut sqlx::PgConnection,
    queue: Option<&str>,
    org_id: Option<&str>,
    limit: i64,
) -> Result<Vec<DeadLetter>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM job_queue WHERE status = 'failed' AND ($1::text IS NULL OR queue = $1) \
         AND ($3::text IS NULL OR org_id = $3) ORDER BY completed_at DESC NULLS LAST, id LIMIT $2"
    ))
    .bind(queue)
    .bind(limit.clamp(1, MAX_LIMIT))
    .bind(org_id)
    .fetch_all(&mut *connection)
    .await?;
    let mut letters = rows
        .iter()
        .map(dead_letter)
        .collect::<Result<Vec<_>, _>>()?;
    let ids: Vec<String> = letters.iter().map(|letter| letter.id.clone()).collect();
    let mut replays = replays_of(connection, &ids).await?;
    for letter in &mut letters {
        letter.replays = replays.remove(&letter.id).unwrap_or_default();
    }
    Ok(letters)
}

/// How many jobs are failed, in one queue or in all.
pub async fn count(pool: &PgPool, queue: Option<&str>) -> Result<i64, sqlx::Error> {
    count_scoped_on(&mut *pool.acquire().await?, queue, None).await
}

pub async fn count_scoped_on(
    connection: &mut sqlx::PgConnection,
    queue: Option<&str>,
    org_id: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM job_queue WHERE status = 'failed' AND ($1::text IS NULL OR queue = $1) \
         AND ($2::text IS NULL OR org_id = $2)",
    )
    .bind(queue)
    .bind(org_id)
    .fetch_one(connection)
    .await
}

/// What a replay did.
#[derive(Clone, Debug)]
pub enum ReplayOutcome {
    /// The job is pending again, with one more attempt allowed. The dead letter is as it was before the replay,
    /// with the new replay last in its history.
    Replayed(Box<DeadLetter>),
    /// No such job.
    NotFound,
    /// The job is not failed (its status is given): pending, running or completed jobs are not replayed.
    NotFailed(String),
    /// The job's workspace no longer exists.
    WorkspaceGone,
    /// The job belongs to a schedule, which queues its next occurrence itself.
    Scheduled,
}

/// Replays one failed job (see the module's rules). `actor` is who replays, `note` their reason, and `redact` is
/// applied to the job's error before it is written to the audit log.
pub async fn replay(
    pool: &PgPool,
    id: &str,
    actor: &str,
    note: Option<&str>,
    redact: &(dyn Fn(&str) -> String + Sync),
) -> Result<ReplayOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let outcome = replay_on(&mut tx, id, actor, note, redact).await?;
    tx.commit().await?;
    Ok(outcome)
}

/// Replay inside the caller's transaction, so credential checks and operational
/// audit commit atomically with the job and its existing replay provenance.
pub async fn replay_on(
    connection: &mut sqlx::PgConnection,
    id: &str,
    actor: &str,
    note: Option<&str>,
    redact: &(dyn Fn(&str) -> String + Sync),
) -> Result<ReplayOutcome, sqlx::Error> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS}, status FROM job_queue WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(ReplayOutcome::NotFound);
    };
    let status: String = row.try_get("status")?;
    if status != "failed" {
        return Ok(ReplayOutcome::NotFailed(status));
    }
    let mut letter = dead_letter(&row)?;
    if letter.scheduled {
        return Ok(ReplayOutcome::Scheduled);
    }
    if let Some(org) = &letter.org_id {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM orgs WHERE id = $1)")
            .bind(org)
            .fetch_one(&mut *connection)
            .await?;
        if !exists {
            return Ok(ReplayOutcome::WorkspaceGone);
        }
    }
    let rearmed = sqlx::query(
        "UPDATE job_queue SET status = 'pending', max_attempts = attempts + 1, run_at = now(), \
             started_at = NULL, completed_at = NULL, locked_by = NULL, lock_token = NULL, lock_expires_at = NULL \
         WHERE id = $1 AND status = 'failed'",
    )
    .bind(id)
    .execute(&mut *connection)
    .await?
    .rows_affected();
    if rearmed != 1 {
        return Ok(ReplayOutcome::NotFailed(status));
    }
    let failed_at = letter.failed_at.map(|at| {
        at.and_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    });
    let error = letter.error.as_deref().map(redact);
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    sqlx::query(
        "INSERT INTO audit_log (id, org_id, actor_type, actor_id, action, entity_type, entity_id, before_state, \
         after_state, metadata) VALUES ($1, $2, 'operator', $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(letter.org_id.clone().unwrap_or_default())
    .bind(actor)
    .bind(REPLAY_ACTION)
    .bind(JOB_ENTITY)
    .bind(id)
    .bind(json!({
        "status": "failed",
        "attempts": letter.attempts,
        "max_attempts": letter.max_attempts,
        "failed_at": failed_at,
        "error": error,
    }))
    .bind(json!({"status": "pending", "attempts": letter.attempts, "max_attempts": letter.attempts + 1}))
    .bind(json!({"queue": letter.queue, "name": letter.name, "note": note}))
    .execute(&mut *connection)
    .await?;
    letter.replays = replays_of(connection, std::slice::from_ref(&letter.id))
        .await?
        .remove(&letter.id)
        .unwrap_or_default();
    Ok(ReplayOutcome::Replayed(Box::new(letter)))
}
