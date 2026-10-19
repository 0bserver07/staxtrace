//! PostgreSQL claims and settlement with renewable, token-fenced leases.

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use std::sync::OnceLock;
use uuid::Uuid;

use crate::{AGENT_JOBS, EnqueueOptions, enqueue, trim_js};

pub const DEFAULT_LEASE_MS: f64 = 60_000.0;
pub const DEFAULT_RETENTION_MS: f64 = 7.0 * 24.0 * 60.0 * 60_000.0;
const OBSERVE_CHAT_MESSAGE_JOB: &str = "observe-chat-message";

/// The queue of factory runs, which workspaces share fairly (see [`workspace_limit`]).
pub const FACTORY_RUNS: &str = "factory-runs";
/// The most factory runs one workspace has running at once while another workspace also uses the queue. The factory
/// executor runs four at a time, so three leaves a slot that another workspace's run can take at once.
pub const DEFAULT_FACTORY_RUNS_PER_WORKSPACE: i64 = 3;
/// The setting that replaces [`DEFAULT_FACTORY_RUNS_PER_WORKSPACE`]: a whole number of 1 or more.
pub const FACTORY_RUNS_PER_WORKSPACE_VAR: &str = "DESKROOST_FACTORY_RUNS_PER_WORKSPACE";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DequeuedJob {
    pub id: String,
    pub name: String,
    pub data: Value,
    /// Authoritative, once-decoded JSON text for memory-worker payloads stored
    /// as JSONB strings. `data` is only a compatibility view in this case;
    /// values that serde cannot represent retain their physical string there.
    #[sqlx(skip)]
    pub encoded_data: Option<String>,
    pub attempts: i32,
    pub cron_key: Option<String>,
    pub locked_by: String,
    pub lock_token: String,
    pub lock_expires_at: NaiveDateTime,
}

#[derive(Debug, Clone, Default)]
pub struct DequeueOptions {
    pub locked_by: Option<String>,
    pub lease_ms: Option<f64>,
    pub org_id: Option<String>,
    pub job_name: Option<String>,
    /// Restrict a partial application registry without claiming unsupported work.
    pub job_names: Option<Vec<String>>,
    pub data_match: Option<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct FailJobOptions {
    pub terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RearmOutcome {
    Active,
    Rearmed,
}

fn positive_integer(value: Option<f64>, fallback: f64) -> f64 {
    value
        .filter(|value| value.is_finite() && *value > 0.0)
        .map_or(fallback, |value| value.floor().max(1.0))
}

fn nonnegative(value: f64) -> f64 {
    if value.is_nan() {
        value
    } else {
        value.max(0.0)
    }
}

fn default_worker_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| format!("deskroost:{}:{}", std::process::id(), Uuid::new_v4()))
}

pub fn job_priority_rank(queue: &str, name: &str) -> i32 {
    if queue != AGENT_JOBS {
        return 3;
    }
    match name {
        "agent-reply" | "agent-employee-message" | "agent-employee-task" => 0,
        "employee-trigger" | "agent-employee-trigger" | "plan-executor" => 1,
        OBSERVE_CHAT_MESSAGE_JOB => 5,
        _ => 3,
    }
}

fn decode_job_data(data: Value) -> Result<Value, sqlx::Error> {
    match data {
        Value::String(encoded) => {
            serde_json::from_str(&encoded).map_err(|error| sqlx::Error::Decode(Box::new(error)))
        }
        value => Ok(value),
    }
}

fn decode_claimed_data(name: &str, data: Value) -> Result<(Value, Option<String>), sqlx::Error> {
    if matches!(name, "memory-capture" | "memory-extract")
        && let Value::String(encoded) = data
    {
        // RawValue validates JSON syntax without interpreting UTF-16 escapes or
        // finite-number limits. Its iterative scanner also permits deep JSON,
        // as JSON.parse does. Preserve the original bytes for the native worker.
        let _: &serde_json::value::RawValue =
            serde_json::from_str(&encoded).map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        let compatibility =
            serde_json::from_str(&encoded).unwrap_or_else(|_| Value::String(encoded.clone()));
        return Ok((compatibility, Some(encoded)));
    }
    decode_job_data(data).map(|data| (data, None))
}

/// The most jobs of `queue` that one workspace has running at once while another workspace also uses the queue;
/// `None` for the queues that are claimed by priority and age alone.
///
/// Deskroost fair concurrency (2026-10-10): the factory executor has a fixed number of slots for every workspace of
/// the server. Without a limit, a workspace with a long backlog holds all of them and another workspace's run waits
/// behind that backlog. Factory runs are therefore claimed by [`dequeue_job_shared`].
pub fn workspace_limit(queue: &str) -> Option<i64> {
    (queue == FACTORY_RUNS).then(|| {
        workspace_limit_from(
            std::env::var(FACTORY_RUNS_PER_WORKSPACE_VAR)
                .ok()
                .as_deref(),
        )
    })
}

/// [`DEFAULT_FACTORY_RUNS_PER_WORKSPACE`] unless the setting is a whole number of 1 or more.
fn workspace_limit_from(setting: Option<&str>) -> i64 {
    setting
        .map(trim_js)
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|limit| *limit >= 1)
        .unwrap_or(DEFAULT_FACTORY_RUNS_PER_WORKSPACE)
}

/// Claims the next due job of `queue`. A queue that workspaces share fairly ([`workspace_limit`]) is claimed by
/// [`dequeue_job_shared`]; every other queue by priority and age.
pub async fn dequeue_job(
    pool: &PgPool,
    queue: &str,
    options: &DequeueOptions,
) -> Result<Option<DequeuedJob>, sqlx::Error> {
    match workspace_limit(queue) {
        Some(limit) => dequeue_job_shared(pool, queue, options, limit).await,
        None => dequeue_job_by_age(pool, queue, options).await,
    }
}

/// The claim of a queue that workspaces share fairly: no workspace can hold every slot, and a backlog does not make
/// another workspace wait.
///
/// - **Least loaded first.** Among due jobs of the same priority, the job of the workspace with the fewest running
///   jobs in this queue is claimed first, and the oldest job within a workspace. A workspace that queued fifty jobs
///   therefore takes turns with one that queued a single job a moment later.
/// - **A limit per workspace.** While another workspace also has jobs in the queue (in any state, so for as long as
///   its finished jobs are retained), a workspace has at most `per_workspace` jobs running; its further jobs wait
///   although a slot is free. With a limit below the consumer's number of slots, a slot is always left for the
///   others. A workspace that has the queue to itself is not limited, and jobs without a workspace never are.
///
/// Claims of one shared queue take turns (a transaction-level advisory lock on the queue's name), so two workers
/// that claim at the same moment both count the running jobs as they are and cannot exceed the limit together.
/// The filters of [`DequeueOptions`] apply as in [`dequeue_job`].
pub async fn dequeue_job_shared(
    pool: &PgPool,
    queue: &str,
    options: &DequeueOptions,
    per_workspace: i64,
) -> Result<Option<DequeuedJob>, sqlx::Error> {
    let locked_by = options
        .locked_by
        .as_deref()
        .map(trim_js)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_worker_id());
    let lock_token = Uuid::new_v4().to_string();
    let lease_ms = positive_integer(options.lease_ms, DEFAULT_LEASE_MS);
    let (data_key, data_value) = options
        .data_match
        .as_ref()
        .map_or((None, None), |(key, value)| (Some(key), Some(value)));
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("deskroost-job-share:{queue}"))
        .execute(&mut *tx)
        .await?;
    let row: Option<DequeuedJob> = sqlx::query_as(
        "WITH load AS (
             SELECT org_id, count(*) AS running FROM job_queue
             WHERE queue=$4 AND status='running' AND org_id IS NOT NULL GROUP BY org_id
         )
         UPDATE job_queue
         SET status='running', started_at=now(), completed_at=NULL, error=NULL,
             attempts=attempts+1, locked_by=$1, lock_token=$2,
             lock_expires_at=now()+($3*interval '1 millisecond')
         WHERE id=(
             SELECT j.id FROM job_queue j LEFT JOIN load l ON l.org_id=j.org_id
             WHERE j.status='pending' AND j.queue=$4 AND j.run_at<=now()
               AND ($5::text IS NULL OR j.org_id=$5)
               AND ($6::text IS NULL OR j.name=$6)
               AND ($7::text IS NULL OR j.data->>$7=$8)
               AND ($9::text[] IS NULL OR j.name=ANY($9))
               AND (j.org_id IS NULL OR COALESCE(l.running,0)<$10 OR NOT EXISTS (
                   SELECT 1 FROM job_queue other
                   WHERE other.queue=$4 AND other.org_id IS NOT NULL AND other.org_id<>j.org_id
               ))
             ORDER BY CASE
                 WHEN j.queue='agent-jobs' AND j.name IN ('agent-reply','agent-employee-message','agent-employee-task') THEN 0
                 WHEN j.queue='agent-jobs' AND j.name IN ('employee-trigger','agent-employee-trigger','plan-executor') THEN 1
                 WHEN j.queue='agent-jobs' AND j.name='observe-chat-message' THEN 5
                 ELSE 3 END, COALESCE(l.running,0) ASC, j.created_at ASC
             LIMIT 1 FOR UPDATE OF j SKIP LOCKED
         )
         RETURNING id,name,data,attempts,cron_key,locked_by,lock_token,lock_expires_at",
    )
    .bind(locked_by)
    .bind(lock_token)
    .bind(lease_ms)
    .bind(queue)
    .bind(&options.org_id)
    .bind(&options.job_name)
    .bind(data_key)
    .bind(data_value)
    .bind(&options.job_names)
    .bind(per_workspace.max(1))
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    row.map(|mut job| {
        (job.data, job.encoded_data) = decode_claimed_data(&job.name, job.data)?;
        Ok(job)
    })
    .transpose()
}

async fn dequeue_job_by_age(
    pool: &PgPool,
    queue: &str,
    options: &DequeueOptions,
) -> Result<Option<DequeuedJob>, sqlx::Error> {
    let locked_by = options
        .locked_by
        .as_deref()
        .map(trim_js)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_worker_id());
    let lock_token = Uuid::new_v4().to_string();
    let lease_ms = positive_integer(options.lease_ms, DEFAULT_LEASE_MS);
    let (data_key, data_value) = options
        .data_match
        .as_ref()
        .map_or((None, None), |(key, value)| (Some(key), Some(value)));
    let row: Option<DequeuedJob> = sqlx::query_as(
        "UPDATE job_queue
         SET status='running', started_at=now(), completed_at=NULL, error=NULL,
             attempts=attempts+1, locked_by=$1, lock_token=$2,
             lock_expires_at=now()+($3*interval '1 millisecond')
         WHERE id=(
             SELECT id FROM job_queue
             WHERE status='pending' AND queue=$4 AND run_at<=now()
               AND ($5::text IS NULL OR org_id=$5)
               AND ($6::text IS NULL OR name=$6)
               AND ($7::text IS NULL OR data->>$7=$8)
               AND ($9::text[] IS NULL OR name=ANY($9))
             ORDER BY CASE
                 WHEN queue='agent-jobs' AND name IN ('agent-reply','agent-employee-message','agent-employee-task') THEN 0
                 WHEN queue='agent-jobs' AND name IN ('employee-trigger','agent-employee-trigger','plan-executor') THEN 1
                 WHEN queue='agent-jobs' AND name='observe-chat-message' THEN 5
                 ELSE 3 END, created_at ASC
             LIMIT 1 FOR UPDATE SKIP LOCKED
         )
         RETURNING id,name,data,attempts,cron_key,locked_by,lock_token,lock_expires_at",
    )
    .bind(locked_by)
    .bind(lock_token)
    .bind(lease_ms)
    .bind(queue)
    .bind(&options.org_id)
    .bind(&options.job_name)
    .bind(data_key)
    .bind(data_value)
    .bind(&options.job_names)
    .fetch_optional(pool)
    .await?;
    // The source reparses one string layer after the claim has committed.
    // Invalid encoded JSON leaves the claimed row running with its new lease.
    row.map(|mut job| {
        (job.data, job.encoded_data) = decode_claimed_data(&job.name, job.data)?;
        Ok(job)
    })
    .transpose()
}

pub async fn renew_job_lease(
    pool: &PgPool,
    id: &str,
    token: &str,
    lease_ms: Option<f64>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE job_queue SET lock_expires_at=now()+($3*interval '1 millisecond')
         WHERE id=$1 AND status='running' AND lock_token=$2 AND lock_expires_at>now()",
    )
    .bind(id)
    .bind(token)
    .bind(positive_integer(lease_ms, DEFAULT_LEASE_MS))
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn complete_job(pool: &PgPool, id: &str, token: &str) -> Result<bool, sqlx::Error> {
    complete_job_on(pool, id, token).await
}

/// [`complete_job`] on a caller's connection, usually a transaction (`&mut *tx`): a consumer can then commit its own
/// domain write together with the settlement, and only while its fencing token still holds the live lease. `false`
/// means the lease was lost (expired, or the job was settled or claimed by another worker); the caller should roll
/// back its write.
pub async fn complete_job_on<'e, E>(executor: E, id: &str, token: &str) -> Result<bool, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let result = sqlx::query(
        "UPDATE job_queue SET status='completed', completed_at=now(), error=NULL,
             locked_by=NULL, lock_token=NULL, lock_expires_at=NULL
         WHERE id=$1 AND status='running' AND lock_token=$2 AND lock_expires_at>now()",
    )
    .bind(id)
    .bind(token)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn fail_job(
    pool: &PgPool,
    id: &str,
    token: &str,
    error: &str,
    options: &FailJobOptions,
) -> Result<bool, sqlx::Error> {
    let row: Option<(String, Value, String)> = sqlx::query_as(
        "UPDATE job_queue SET
             status=CASE WHEN $3 OR attempts>=max_attempts THEN 'failed' ELSE 'pending' END,
             run_at=CASE WHEN $3 OR attempts>=max_attempts THEN run_at
                 ELSE now()+(LEAST(1000*power(2,GREATEST(attempts-1,0)),60000)*interval '1 millisecond') END,
             started_at=CASE WHEN $3 OR attempts>=max_attempts THEN started_at ELSE NULL END,
             completed_at=CASE WHEN $3 OR attempts>=max_attempts THEN now() ELSE NULL END,
             error=$4, locked_by=NULL, lock_token=NULL, lock_expires_at=NULL
         WHERE id=$1 AND status='running' AND lock_token=$2 AND lock_expires_at>now()
         RETURNING name,data,status",
    )
    .bind(id)
    .bind(token)
    .bind(options.terminal)
    .bind(error)
    .fetch_optional(pool)
    .await?;
    let Some((name, data, status)) = row else {
        return Ok(false);
    };
    if name == OBSERVE_CHAT_MESSAGE_JOB {
        let data = decode_job_data(data)?;
        mark_observation_failed(pool, &data, status == "pending", error).await?;
    }
    Ok(true)
}

pub async fn defer_job(
    pool: &PgPool,
    id: &str,
    token: &str,
    error: &str,
    delay_ms: f64,
) -> Result<bool, sqlx::Error> {
    let delay = if delay_ms.is_nan() {
        delay_ms
    } else {
        delay_ms.floor().max(1_000.0)
    };
    let result = sqlx::query(
        "UPDATE job_queue SET status='pending', attempts=GREATEST(attempts-1,0),
             run_at=now()+($4*interval '1 millisecond'), started_at=NULL,
             completed_at=NULL, error=$3, locked_by=NULL, lock_token=NULL, lock_expires_at=NULL
         WHERE id=$1 AND status='running' AND lock_token=$2 AND lock_expires_at>now()",
    )
    .bind(id)
    .bind(token)
    .bind(error)
    .bind(delay)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// `enqueueOrRearmFailed(queue, name, data, { dedupeKey, executor })`: enqueue one deduplicated delivery, or re-arm
/// that exact delivery after its queue-level retries terminal-failed. The executor is one connection, usually a
/// transaction (`&mut *tx`), so a caller can commit the re-arm together with its own domain write (the App automation
/// scanner charges the failed delivery to the fire in the same transaction).
pub async fn enqueue_or_rearm_failed(
    connection: &mut PgConnection,
    queue: &str,
    name: &str,
    data: &Value,
    options: &EnqueueOptions,
) -> Result<RearmOutcome, sqlx::Error> {
    let dedupe = options
        .dedupe_key
        .as_deref()
        .map(trim_js)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            sqlx::Error::Protocol("Recoverable delivery requires a dedupe key".into())
        })?;
    let org = options.org_id.clone().unwrap_or_else(|| {
        ["orgId", "org_id"].into_iter().find_map(|key| {
            data.get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
    });
    enqueue(&mut *connection, queue, name, data, options).await?;
    let result = sqlx::query(
        "UPDATE job_queue SET status='pending', data=$1, attempts=0, max_attempts=$2,
             run_at=now()+($3*interval '1 millisecond'), started_at=NULL, completed_at=NULL,
             error=NULL, locked_by=NULL, lock_token=NULL, lock_expires_at=NULL
         WHERE org_id IS NOT DISTINCT FROM $4 AND queue=$5 AND name=$6 AND dedupe_key=$7 AND status='failed'",
    )
    .bind(data)
    .bind(positive_integer(options.max_attempts, 3.0) as i64)
    .bind(nonnegative(options.delay_ms))
    .bind(org)
    .bind(queue)
    .bind(name)
    .bind(dedupe)
    .execute(&mut *connection)
    .await?;
    Ok(if result.rows_affected() == 1 {
        RearmOutcome::Rearmed
    } else {
        RearmOutcome::Active
    })
}

pub async fn ensure_cron_job(
    pool: &PgPool,
    queue: &str,
    name: &str,
    key: &str,
    data: Option<&Value>,
    delay_ms: Option<f64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO job_queue(id,queue,name,data,status,max_attempts,run_at,cron_key)
         VALUES($1,$2,$3,$4,'pending',2,now()+($5*interval '1 millisecond'),$6)
         ON CONFLICT DO NOTHING",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(queue)
    .bind(name)
    .bind(data.unwrap_or(&json!({})))
    .bind(nonnegative(delay_ms.unwrap_or(0.0)))
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn cleanup_stale_jobs(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let rows: Vec<(String, Value, String, String)> = sqlx::query_as(
        "UPDATE job_queue SET
             status=CASE WHEN attempts<max_attempts THEN 'pending' ELSE 'failed' END,
             error='stale: worker lease expired before settlement',
             run_at=CASE WHEN attempts<max_attempts THEN now()+interval '5 seconds' ELSE run_at END,
             started_at=CASE WHEN attempts<max_attempts THEN NULL ELSE started_at END,
             completed_at=CASE WHEN attempts<max_attempts THEN NULL ELSE now() END,
             locked_by=NULL, lock_token=NULL, lock_expires_at=NULL
         WHERE status='running' AND (lock_expires_at IS NULL OR lock_expires_at<=now())
         RETURNING name,data,status,error",
    )
    .fetch_all(pool)
    .await?;
    for (name, data, status, error) in &rows {
        if name == OBSERVE_CHAT_MESSAGE_JOB {
            let data = decode_job_data(data.clone())?;
            mark_observation_failed(pool, &data, status == "pending", error).await?;
        }
    }
    Ok(rows.len() as u64)
}

pub async fn prune_finished_jobs(
    pool: &PgPool,
    retention_ms: Option<f64>,
) -> Result<u64, sqlx::Error> {
    let retention = positive_integer(retention_ms, DEFAULT_RETENTION_MS);
    // The source uses the application clock (millisecond precision), not the
    // database clock, for the cutoff and compares it strictly.
    let cutoff_ms = Utc::now().timestamp_millis() as f64 - retention;
    let cutoff = if cutoff_ms.abs() <= 8.64e15 {
        DateTime::from_timestamp_millis(cutoff_ms as i64)
    } else {
        None
    }
    .ok_or_else(|| sqlx::Error::Protocol("Invalid retention cutoff date".into()))?
    .naive_utc();
    let result = sqlx::query(
        "DELETE FROM job_queue WHERE status IN ('completed','failed')
         AND completed_at IS NOT NULL AND completed_at<$1",
    )
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

async fn mark_observation_failed(
    pool: &PgPool,
    data: &Value,
    retrying: bool,
    error: &str,
) -> Result<(), sqlx::Error> {
    let (Some(message), Some(org)) = (
        data.get("messageId").and_then(Value::as_str),
        data.get("orgId").and_then(Value::as_str),
    ) else {
        return Ok(());
    };
    let version = data
        .get("observationVersion")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let error = observation_error(error);
    let now = DateTime::from_timestamp_millis(Utc::now().timestamp_millis())
        .expect("current timestamp")
        .naive_utc();
    // SQLx binds TEXT explicitly, unlike node-postgres' untyped parameters;
    // PostgreSQL does not assign TEXT to the enum column implicitly.
    sqlx::query(
        "UPDATE message_observations SET status=$4::text::message_observation_status,last_error=$5,completed_at=$6,updated_at=$7
         WHERE org_id=$1 AND message_id=$2 AND observation_version=$3::text::integer",
    )
    .bind(org)
    .bind(message)
    .bind(version.to_string())
    .bind(if retrying { "retrying" } else { "failed" })
    .bind(error)
    .bind(if retrying { None } else { Some(now) })
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

fn observation_error(error: &str) -> String {
    // slice(0, 2000) counts UTF-16 units. If it splits a surrogate pair,
    // node-postgres writes the resulting lone surrogate as U+FFFD in UTF-8.
    String::from_utf16_lossy(&error.encode_utf16().take(2000).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_replies_and_employee_actions_outrank_bulk_observations() {
        assert_eq!(job_priority_rank(AGENT_JOBS, "agent-reply"), 0);
        assert_eq!(job_priority_rank(AGENT_JOBS, "agent-employee-message"), 0);
        assert_eq!(job_priority_rank(AGENT_JOBS, "employee-trigger"), 1);
        assert_eq!(job_priority_rank(AGENT_JOBS, "plan-executor"), 1);
        assert_eq!(job_priority_rank(AGENT_JOBS, "some-background-job"), 3);
        assert_eq!(job_priority_rank(AGENT_JOBS, OBSERVE_CHAT_MESSAGE_JOB), 5);
    }

    #[test]
    fn only_factory_runs_are_shared_and_the_limit_is_a_whole_number_of_one_or_more() {
        assert_eq!(FACTORY_RUNS, "factory-runs");
        for queue in [
            AGENT_JOBS,
            crate::SCHEDULED_JOBS,
            "custom-queue",
            "factory-runs ",
        ] {
            assert_eq!(workspace_limit(queue), None, "{queue}");
        }
        assert_eq!(
            workspace_limit_from(None),
            DEFAULT_FACTORY_RUNS_PER_WORKSPACE
        );
        assert_eq!(DEFAULT_FACTORY_RUNS_PER_WORKSPACE, 3);
        for (setting, limit) in [
            ("1", 1),
            (" 2 ", 2),
            ("12", 12),
            ("", 3),
            ("0", 3),
            ("-4", 3),
            ("2.5", 3),
            ("many", 3),
        ] {
            assert_eq!(workspace_limit_from(Some(setting)), limit, "{setting:?}");
        }
    }

    #[test]
    fn non_agent_queues_keep_neutral_priority() {
        assert_eq!(job_priority_rank(crate::SCHEDULED_JOBS, "agent-reply"), 3);
        assert_eq!(
            job_priority_rank("custom-queue", OBSERVE_CHAT_MESSAGE_JOB),
            3
        );
    }

    #[test]
    fn job_data_reparses_only_one_encoded_json_string_layer() {
        let object = json!({"messageId":"message","orgId":"org"});
        assert_eq!(decode_job_data(object.clone()).unwrap(), object);
        assert_eq!(decode_job_data(json!(object.to_string())).unwrap(), object);
        assert_eq!(decode_job_data(json!("null")).unwrap(), Value::Null);
        assert_eq!(decode_job_data(json!("42")).unwrap(), json!(42));
        assert_eq!(
            decode_job_data(json!(r#""plain string""#)).unwrap(),
            json!("plain string")
        );
        assert!(matches!(
            decode_job_data(json!("not JSON")),
            Err(sqlx::Error::Decode(_))
        ));
    }

    #[test]
    fn memory_capture_claim_keeps_the_source_json_parse_domain_and_original_encoding() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../deskroost-chat/tests/fixtures/memory-capture-source.json"
        ))
        .unwrap();
        let cases = fixture["decode_cases"].as_array().unwrap();
        assert_eq!(cases.len(), 10);
        for name in ["memory-capture", "memory-extract"] {
            let mut accepted = 0;
            for case in cases {
                let stored: Value =
                    serde_json::from_str(case["driver_data_json"].as_str().unwrap()).unwrap();
                let decoded = decode_claimed_data(name, stored.clone());
                if case.get("error").is_some() {
                    assert!(
                        matches!(decoded, Err(sqlx::Error::Decode(_))),
                        "{}",
                        case["name"]
                    );
                    continue;
                }
                let (compatibility, encoded) = decoded.unwrap();
                if let Value::String(original) = &stored {
                    assert_eq!(
                        encoded.as_deref(),
                        Some(original.as_str()),
                        "{}",
                        case["name"]
                    );
                    match serde_json::from_str::<Value>(original) {
                        Ok(expected) => assert_eq!(compatibility, expected, "{}", case["name"]),
                        Err(_) => {
                            assert_eq!(compatibility, stored);
                            // Other consumers retain their existing validation and
                            // never receive this memory-specific encoded channel.
                            assert!(decode_claimed_data("other-job", stored).is_err());
                        }
                    }
                } else {
                    assert!(encoded.is_none());
                    assert_eq!(compatibility, stored);
                }
                accepted += 1;
            }
            assert_eq!(accepted, 8);
            for invalid in ["[0,]", "{\"a\":}", "1 2", "NaN", "\"\\uGGGG\""] {
                assert!(decode_claimed_data(name, json!(invalid)).is_err());
            }
        }
    }

    #[test]
    fn observation_errors_use_utf16_slice_and_node_postgres_utf8_encoding() {
        assert_eq!(
            observation_error(&format!("{}😀tail", "a".repeat(1999))),
            format!("{}\u{fffd}", "a".repeat(1999))
        );
        assert_eq!(
            observation_error(&format!("{}😀tail", "a".repeat(1998))),
            format!("{}😀", "a".repeat(1998))
        );
        assert_eq!(observation_error("short failure"), "short failure");
    }
}
