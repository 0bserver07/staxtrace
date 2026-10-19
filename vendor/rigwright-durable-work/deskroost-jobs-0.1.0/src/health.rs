//! Global operational queue health, including the actual local worker heartbeat.

use chrono::{DateTime, Datelike, Utc};
use serde_json::{Value, json};
use sqlx::{Executor, PgPool, Postgres, Row};

pub const QUEUE_WORKER_STALE_AFTER_MS: i64 = 15_000;

/// Process state captured by the worker owner before querying queue health.
#[derive(Clone, Debug, Default)]
pub struct WorkerStatus {
    pub running: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub in_flight: usize,
}

const HEALTH_SQL: &str = "
    SELECT
      count(*) FILTER (WHERE status = 'pending')::int AS pending,
      count(*) FILTER (WHERE status = 'running')::int AS running,
      count(*) FILTER (WHERE status = 'completed')::int AS completed,
      count(*) FILTER (WHERE status = 'failed')::int AS failed,
      COALESCE(
        GREATEST(
          EXTRACT(EPOCH FROM (now() - min(run_at) FILTER (
            WHERE status = 'pending' AND run_at <= now()
          ))),
          0
        ),
        0
      )::double precision AS oldest_ready_lag_seconds,
      count(*) FILTER (
        WHERE status = 'running'
          AND (lock_expires_at IS NULL OR lock_expires_at <= now())
      )::int AS expired_leases,
      count(*) FILTER (
        WHERE status = 'failed'
          AND completed_at >= now() - interval '24 hours'
      )::int AS recent_terminal_failures
    FROM job_queue
    WHERE true
  ";

/// The operator snapshot intentionally covers every queue and tenant.
///
/// `now` is the time captured before the database await. PostgreSQL's `now()`
/// independently determines the persisted lease and ready-lag aggregates.
pub async fn get_queue_health_snapshot(
    pool: &PgPool,
    worker: &WorkerStatus,
    now: DateTime<Utc>,
) -> Result<Value, sqlx::Error> {
    query_snapshot(pool, worker, now).await
}

async fn query_snapshot<'e, E>(
    executor: E,
    worker: &WorkerStatus,
    now: DateTime<Utc>,
) -> Result<Value, sqlx::Error>
where
    E: Executor<'e, Database = Postgres>,
{
    let row = sqlx::query(HEALTH_SQL)
        .fetch_optional(executor)
        .await?
        .map(|row| {
            Ok::<_, sqlx::Error>(json!({
                "pending": row.try_get::<i32, _>("pending")?,
                "running": row.try_get::<i32, _>("running")?,
                "completed": row.try_get::<i32, _>("completed")?,
                "failed": row.try_get::<i32, _>("failed")?,
                "oldest_ready_lag_seconds": row.try_get::<f64, _>("oldest_ready_lag_seconds")?,
                "expired_leases": row.try_get::<i32, _>("expired_leases")?,
                "recent_terminal_failures": row.try_get::<i32, _>("recent_terminal_failures")?,
            }))
        })
        .transpose()?;
    Ok(summarize(row.as_ref(), worker.into(), now))
}

// The source's pure helper accepts numeric worker input independently of the
// process tracker. Retain that normalization boundary for its original tests;
// actual tracked in-flight counts are naturally unsigned.
struct WorkerProjection {
    running: bool,
    started_at: Option<DateTime<Utc>>,
    last_poll_at: Option<DateTime<Utc>>,
    in_flight: f64,
}

impl From<&WorkerStatus> for WorkerProjection {
    fn from(worker: &WorkerStatus) -> Self {
        Self {
            running: worker.running,
            started_at: worker.started_at,
            last_poll_at: worker.last_poll_at,
            in_flight: worker.in_flight as f64,
        }
    }
}

fn summarize(row: Option<&Value>, worker: WorkerProjection, now: DateTime<Utc>) -> Value {
    // JavaScript Date stores integer milliseconds, including before the epoch.
    let heartbeat_age = worker
        .last_poll_at
        .map(|last| ((now.timestamp_millis() - last.timestamp_millis()) as f64 / 1000.0).max(0.0));
    let heartbeat_stale = !worker.running
        || heartbeat_age.is_none_or(|age| age * 1000.0 > QUEUE_WORKER_STALE_AFTER_MS as f64);
    let value = |key| numeric(row.and_then(|row| row.get(key)));
    let expired = value("expired_leases");
    json!({
        "status": if heartbeat_stale || expired > 0.0 { "degraded" } else { "ok" },
        "pending": number(value("pending")),
        "running": number(value("running")),
        "completed": number(value("completed")),
        "failed": number(value("failed")),
        "oldest_ready_lag_seconds": number(value("oldest_ready_lag_seconds")),
        "expired_leases": number(expired),
        "recent_terminal_failures": number(value("recent_terminal_failures")),
        "worker": {
            "running": worker.running,
            "started_at": worker.started_at.map(iso_date),
            "last_poll_at": worker.last_poll_at.map(iso_date),
            "heartbeat_age_seconds": heartbeat_age.map(number),
            "heartbeat_stale": heartbeat_stale,
            "in_flight": number(nonnegative(worker.in_flight)),
        },
    })
}

fn nonnegative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn numeric(value: Option<&Value>) -> f64 {
    nonnegative(match value {
        Some(Value::Number(value)) => value.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(value)) => string_number(value),
        // The declared SQL-row input is number | string | null | undefined.
        _ => 0.0,
    })
}

fn string_number(value: &str) -> f64 {
    let value = value.trim_matches(|character| {
        matches!(character, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
    });
    if value.is_empty() {
        return 0.0;
    }
    let radix = match value.as_bytes().get(..2) {
        Some(b"0x" | b"0X") => Some((16, 4)),
        Some(b"0o" | b"0O") => Some((8, 3)),
        Some(b"0b" | b"0B") => Some((2, 1)),
        _ => None,
    };
    if let Some((radix, width)) = radix {
        let mut bits = Vec::new();
        for character in value[2..].chars() {
            let Some(digit) = character.to_digit(radix) else {
                return f64::NAN;
            };
            for shift in (0..width).rev() {
                let bit = (digit >> shift) & 1;
                if bit != 0 || !bits.is_empty() {
                    bits.push(bit as u64);
                }
            }
        }
        if value.len() == 2 {
            return f64::NAN;
        }
        if bits.len() > 1024 {
            return f64::INFINITY;
        }
        let length = bits.len().min(53);
        let mut significand = bits[..length].iter().fold(0_u64, |n, bit| n * 2 + bit);
        if bits.len() > length
            && bits[length] == 1
            && (significand & 1 == 1 || bits[length + 1..].contains(&1))
        {
            significand += 1;
        }
        return significand as f64 * 2_f64.powi((bits.len() - length) as i32);
    }
    // Rust accepts "inf" spellings that Number() does not. They normalize to
    // zero either way, but exclude them explicitly at the conversion boundary.
    if value.contains(['i', 'I', 'n', 'N']) {
        return f64::NAN;
    }
    value.parse().unwrap_or(f64::NAN)
}

fn number(value: f64) -> Value {
    // Counts and integral seconds stringify without a trailing decimal in JS.
    // The upper bound is exclusive because u64::MAX rounds to 2^64 as f64.
    if value.fract() == 0.0 && (0.0..18_446_744_073_709_551_616.0).contains(&value) {
        Value::from(value as u64)
    } else {
        Value::from(value)
    }
}

fn iso_date(value: DateTime<Utc>) -> String {
    let year = value.year();
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+07}")
    };
    format!("{year}{}Z", value.format("-%m-%dT%H:%M:%S%.3f"))
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
