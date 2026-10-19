//! Durable PostgreSQL job producers. Consumers are added with their features.

pub mod runtime;

pub mod consumer;
pub mod dead_letters;
pub mod health;
pub mod outbound;

use serde_json::Value;
use sqlx::{Executor, Postgres};
use uuid::Uuid;

pub const AGENT_JOBS: &str = "agent-jobs";
pub const SCHEDULED_JOBS: &str = "scheduled-jobs";

/// The PostgreSQL `NOTIFY` channel that wakes the job workers (payload: the queue).
///
/// Deskroost latency work: a job someone is waiting for ([`wakes_worker`]) is claimed as soon as its enqueue commits,
/// instead of at the next poll (up to `DESKROOST_WORKER_POLL_INTERVAL_MS`, 1 s by default). The notification is sent
/// by the inserting statement itself, so PostgreSQL delivers it only when that transaction commits.
pub const WAKE_CHANNEL: &str = "deskroost_job_wake";

/// The jobs a person is waiting for: an assistant mention reply, an agent employee's answer or assigned task, an
/// employee trigger and a plan run (the two highest claim priorities of the agent-jobs queue).
pub fn wakes_worker(queue: &str, name: &str) -> bool {
    queue == AGENT_JOBS && runtime::job_priority_rank(queue, name) <= 1
}

#[derive(Clone, Debug, Default)]
pub struct EnqueueOptions {
    pub delay_ms: f64,
    pub max_attempts: Option<f64>,
    /// None infers the tenant from the payload; Some(None) explicitly omits it.
    pub org_id: Option<Option<String>>,
    pub dedupe_key: Option<String>,
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(|c| matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'))
}

const INSERT_SQL: &str = "INSERT INTO job_queue(id,org_id,queue,name,data,status,max_attempts,run_at,dedupe_key) VALUES($1,$2,$3,$4,$5,'pending',$6,now()+($7*interval '1 millisecond'),$8) ON CONFLICT DO NOTHING";
/// [`INSERT_SQL`] that also notifies [`WAKE_CHANNEL`] when (and only when) it inserted the row.
const WAKING_INSERT_SQL: &str = "WITH queued AS (INSERT INTO job_queue(id,org_id,queue,name,data,status,max_attempts,run_at,dedupe_key) VALUES($1,$2,$3,$4,$5,'pending',$6,now()+($7*interval '1 millisecond'),$8) ON CONFLICT DO NOTHING RETURNING queue) SELECT pg_notify('deskroost_job_wake', queue) FROM queued";

pub async fn enqueue<'e, E>(
    executor: E,
    queue: &str,
    name: &str,
    data: &Value,
    options: &EnqueueOptions,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    let org = options.org_id.clone().unwrap_or_else(|| {
        ["orgId", "org_id"].into_iter().find_map(|key| {
            data.get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
    });
    let data = serde_json::value::to_raw_value(data)
        .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    enqueue_raw(executor, queue, name, &data, org.as_deref(), options).await
}

/// Enqueue already serialized application JSON without converting its strings.
///
/// The caller supplies the tenant inferred from the original payload; an
/// explicit `options.org_id` still takes precedence. PostgreSQL validates the
/// JSONB value during insertion, retaining failures for escaped lone surrogates
/// and NUL characters at the same boundary as other queue writes.
pub async fn enqueue_raw<'e, E>(
    executor: E,
    queue: &str,
    name: &str,
    data: &serde_json::value::RawValue,
    inferred_org_id: Option<&str>,
    options: &EnqueueOptions,
) -> Result<(), sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    let org = options
        .org_id
        .as_ref()
        .map_or(inferred_org_id, |value| value.as_deref());
    let attempts = options
        .max_attempts
        .filter(|value| value.is_finite() && *value > 0.0)
        .map_or(3.0, |value| value.floor().max(1.0));
    let delay = if options.delay_ms.is_nan() {
        f64::NAN
    } else {
        options.delay_ms.max(0.0)
    };
    let dedupe = options
        .dedupe_key
        .as_deref()
        .map(trim_js)
        .filter(|value| !value.is_empty());
    // A job due now that someone waits for also wakes the workers (`WAKE_CHANNEL`), in the same statement so the
    // notification is delivered exactly when the row is committed. A deduplicated (not inserted) job wakes nobody.
    let sql = if delay == 0.0 && wakes_worker(queue, name) {
        WAKING_INSERT_SQL
    } else {
        INSERT_SQL
    };
    sqlx::query(sql)
        .bind(Uuid::new_v4().to_string())
        .bind(org)
        .bind(queue)
        .bind(name)
        .bind(sqlx::types::Json(data))
        .bind(attempts as i64)
        .bind(delay)
        .bind(dedupe)
        .execute(executor)
        .await?;
    Ok(())
}

#[cfg(test)]
mod wake_tests {
    use super::*;

    #[test]
    fn jobs_someone_waits_for_wake_the_workers_and_background_jobs_do_not() {
        for name in [
            "agent-reply",
            "agent-employee-message",
            "agent-employee-task",
            "employee-trigger",
            "agent-employee-trigger",
            "plan-executor",
        ] {
            assert!(wakes_worker(AGENT_JOBS, name), "{name}");
            assert!(!wakes_worker(SCHEDULED_JOBS, name), "{name}");
        }
        for name in [
            "observe-chat-message",
            "embed-content",
            "memory-capture",
            "task-extract",
            "cross-reference",
        ] {
            assert!(!wakes_worker(AGENT_JOBS, name), "{name}");
        }
    }

    #[test]
    fn the_waking_insert_is_the_plain_insert_plus_a_notification_of_the_inserted_row() {
        let plain = INSERT_SQL.strip_suffix(" ON CONFLICT DO NOTHING").unwrap();
        assert!(WAKING_INSERT_SQL.contains(plain));
        assert!(WAKING_INSERT_SQL.contains("ON CONFLICT DO NOTHING RETURNING queue)"));
        assert!(WAKING_INSERT_SQL.ends_with(&format!(
            "SELECT pg_notify('{WAKE_CHANNEL}', queue) FROM queued"
        )));
    }
}
