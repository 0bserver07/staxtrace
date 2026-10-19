//! Health of one queue's consumer, with alert thresholds (Deskroost, 2026-10-10).
//!
//! [`crate::health`] aggregates every queue and reports the general worker of the answering process. It cannot say
//! whether a queue with a consumer of its own, such as the factory executor, is being worked: "nothing is running"
//! reads the same for an executor with free slots and for one that is gone. This module answers that for one queue:
//!
//! | State | Meaning |
//! |---|---|
//! | `idle` | A consumer is connected and nothing is due: free capacity. |
//! | `working` | A consumer is connected and runs jobs, with slots to spare. |
//! | `saturated` | A consumer is connected and every slot is taken. |
//! | `unhealthy` | A consumer is connected, but running jobs lost their lease or due jobs are not being claimed. |
//! | `unreachable` | This server consumes the queue, but no process holds the consumer's lock. |
//! | `disabled` | This server is configured not to consume the queue. |
//!
//! A consumer is *connected* while some database session holds its presence lock: the session-level advisory lock a
//! consuming process takes when it starts and keeps for as long as it lives. PostgreSQL releases the lock when that
//! process or its connection dies, so the lock is evidence of a live process and not a flag someone forgot to clear.
//!
//! Alerts compare what is observed with [`Thresholds`]; each names its value and the threshold it crossed:
//!
//! | Alert | Severity | Raised when |
//! |---|---|---|
//! | `consumer_unreachable` | critical | the queue is consumed here and no process holds the lock |
//! | `expired_leases` | critical | a running job's lease ran out: its worker stopped renewing it |
//! | `stalled` | critical | a claimable job has been due longer than the lag threshold although slots are free |
//! | `backlog` | warning | a claimable job has been due longer than the lag threshold and every slot is taken |
//! | `terminal_failures` | warning | at least the threshold of jobs failed for good in the last 24 hours, not counting interrupted ones |
//! | `work_without_consumer` | warning | jobs are due, and this server does not consume the queue |
//!
//! **Interrupted is not failed.** A consumer that ends a job because its own process stopped, not because the work
//! failed, records the job's error with the prefix [`INTERRUPTED_PREFIX`] (the factory executor does, for an
//! execution that a restart or a lost lease cut short). Such jobs are counted in `interrupted_24h` and raise no
//! alert: one restart with several executions in flight is not a run of failures.
//!
//! `status` is `failing` with a critical alert, `degraded` with only warnings and `ok` without alerts. A due job
//! that waits only because its workspace is at its limit of a shared queue (`runtime::workspace_limit`) is not
//! claimable: it is counted in `held_by_workspace_limit` and raises no `stalled` alert.
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use crate::trim_js;

/// A claimable job due for longer than this many seconds raises `stalled` or `backlog`.
pub const DEFAULT_READY_LAG_SECONDS: f64 = 300.0;
/// This many terminal failures within 24 hours raise `terminal_failures`.
pub const DEFAULT_TERMINAL_FAILURES: i64 = 5;
/// The start of the error of a job that its consumer's stop cut short (see the module's "Interrupted is not failed").
pub const INTERRUPTED_PREFIX: &str = "interrupted:";
/// The setting that replaces [`DEFAULT_READY_LAG_SECONDS`]: a number of seconds above 0.
pub const READY_LAG_VAR: &str = "DESKROOST_QUEUE_ALERT_READY_LAG_SECONDS";
/// The setting that replaces [`DEFAULT_TERMINAL_FAILURES`]: a whole number of 1 or more.
pub const TERMINAL_FAILURES_VAR: &str = "DESKROOST_QUEUE_ALERT_TERMINAL_FAILURES";

/// What the caller knows about the consumer of a queue.
#[derive(Clone, Debug)]
pub struct Consumer {
    pub queue: String,
    /// Whether this server is configured to consume the queue.
    pub enabled: bool,
    /// The jobs one consuming process runs at once.
    pub slots: i64,
    /// The key of the session-level advisory lock a consuming process holds while it lives.
    pub presence_lock: i64,
    /// The limit per workspace of a queue that workspaces share (`runtime::workspace_limit`), if it is one.
    pub workspace_limit: Option<i64>,
}

/// The values alerts compare with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    pub ready_lag_seconds: f64,
    pub terminal_failures: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            ready_lag_seconds: DEFAULT_READY_LAG_SECONDS,
            terminal_failures: DEFAULT_TERMINAL_FAILURES,
        }
    }
}

impl Thresholds {
    /// The thresholds of this process: the defaults unless the settings replace them.
    pub fn from_env() -> Self {
        Self::from_settings(
            std::env::var(READY_LAG_VAR).ok().as_deref(),
            std::env::var(TERMINAL_FAILURES_VAR).ok().as_deref(),
        )
    }

    /// A setting that is not a usable number keeps the default.
    pub fn from_settings(ready_lag_seconds: Option<&str>, terminal_failures: Option<&str>) -> Self {
        Thresholds {
            ready_lag_seconds: ready_lag_seconds
                .map(trim_js)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                .unwrap_or(DEFAULT_READY_LAG_SECONDS),
            terminal_failures: terminal_failures
                .map(trim_js)
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|count| *count >= 1)
                .unwrap_or(DEFAULT_TERMINAL_FAILURES),
        }
    }
}

/// What the database says about the queue and its consumer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Observed {
    /// Sessions holding the consumer's presence lock.
    pub consumers: i64,
    pub running: i64,
    /// Running jobs whose lease ran out.
    pub expired_leases: i64,
    /// Pending jobs whose time has come.
    pub due: i64,
    /// Due jobs a consumer with a free slot would claim now.
    pub claimable: i64,
    /// Pending jobs scheduled for later.
    pub scheduled: i64,
    /// How long the oldest claimable job has been due.
    pub oldest_claimable_lag_seconds: f64,
    /// Jobs that failed for good within the last 24 hours, without the interrupted ones.
    pub terminal_failures: i64,
    /// Jobs that their consumer's stop cut short within the last 24 hours.
    pub interrupted: i64,
}

const OBSERVE_SQL: &str = "
    WITH load AS (
        SELECT org_id, count(*) AS running FROM job_queue
        WHERE queue=$1 AND status='running' AND org_id IS NOT NULL GROUP BY org_id
    ), shared AS (
        SELECT count(DISTINCT org_id) > 1 AS yes FROM job_queue WHERE queue=$1 AND org_id IS NOT NULL
    ), due AS (
        SELECT j.run_at,
               ($2::bigint IS NULL OR j.org_id IS NULL OR COALESCE(l.running,0) < $2
                OR NOT (SELECT yes FROM shared)) AS claimable
        FROM job_queue j LEFT JOIN load l ON l.org_id=j.org_id
        WHERE j.queue=$1 AND j.status='pending' AND j.run_at<=now()
    ), totals AS (
        SELECT
          count(*) FILTER (WHERE status='running') AS running,
          count(*) FILTER (WHERE status='running' AND (lock_expires_at IS NULL OR lock_expires_at<=now())) AS expired,
          count(*) FILTER (WHERE status='pending' AND run_at>now()) AS scheduled,
          count(*) FILTER (WHERE status='failed' AND completed_at>=now()-interval '24 hours'
                           AND (error IS NULL OR left(error, length($4)) <> $4)) AS failures,
          count(*) FILTER (WHERE status='failed' AND completed_at>=now()-interval '24 hours'
                           AND left(error, length($4)) = $4) AS interrupted
        FROM job_queue WHERE queue=$1
    )
    SELECT totals.running, totals.expired, totals.scheduled, totals.failures, totals.interrupted,
           (SELECT count(*) FROM due) AS due,
           (SELECT count(*) FROM due WHERE claimable) AS claimable,
           COALESCE(GREATEST(EXTRACT(EPOCH FROM (now() - (SELECT min(run_at) FROM due WHERE claimable))), 0), 0)::double precision AS lag,
           (SELECT count(*) FROM pg_locks held JOIN pg_database d ON d.oid=held.database
            WHERE d.datname=current_database() AND held.locktype='advisory' AND held.granted AND held.objsubid=1
              AND held.classid=(($3::bigint >> 32) & 4294967295)::oid
              AND held.objid=($3::bigint & 4294967295)::oid) AS consumers
    FROM totals";

/// Reads the queue and the consumer's presence lock.
pub async fn observe(pool: &PgPool, consumer: &Consumer) -> Result<Observed, sqlx::Error> {
    let row = sqlx::query(OBSERVE_SQL)
        .bind(&consumer.queue)
        .bind(consumer.workspace_limit)
        .bind(consumer.presence_lock)
        .bind(INTERRUPTED_PREFIX)
        .fetch_one(pool)
        .await?;
    Ok(Observed {
        consumers: row.try_get("consumers")?,
        running: row.try_get("running")?,
        expired_leases: row.try_get("expired")?,
        due: row.try_get("due")?,
        claimable: row.try_get("claimable")?,
        scheduled: row.try_get("scheduled")?,
        oldest_claimable_lag_seconds: row.try_get("lag")?,
        terminal_failures: row.try_get("failures")?,
        interrupted: row.try_get("interrupted")?,
    })
}

/// The consumer's health: what [`observe`] reads, judged by [`assess`].
pub async fn consumer_health(
    pool: &PgPool,
    consumer: &Consumer,
    thresholds: &Thresholds,
) -> Result<Value, sqlx::Error> {
    Ok(assess(
        consumer,
        &observe(pool, consumer).await?,
        thresholds,
    ))
}

fn alert(code: &str, severity: &str, message: String, value: Value, threshold: Value) -> Value {
    json!({"code": code, "severity": severity, "message": message, "value": value, "threshold": threshold})
}

/// The state, the alerts and the numbers behind them.
pub fn assess(consumer: &Consumer, observed: &Observed, thresholds: &Thresholds) -> Value {
    let slots = consumer.slots.max(1);
    let connected = observed.consumers > 0;
    // Every connected process has its own slots.
    let capacity = slots * observed.consumers.max(1);
    let free = (capacity - observed.running).max(0);
    let lagging = observed.claimable > 0
        && observed.oldest_claimable_lag_seconds > thresholds.ready_lag_seconds;
    let queue = &consumer.queue;
    let mut alerts = Vec::new();
    if !consumer.enabled {
        if observed.due > 0 {
            alerts.push(alert(
                "work_without_consumer",
                "warning",
                format!(
                    "{} {queue} job(s) are due, and this server does not consume the queue",
                    observed.due
                ),
                json!(observed.due),
                json!(0),
            ));
        }
    } else if !connected {
        alerts.push(alert(
            "consumer_unreachable",
            "critical",
            format!("No process holds the consumer's lock: nothing executes {queue} jobs"),
            json!(observed.consumers),
            json!(1),
        ));
    }
    if observed.expired_leases > 0 {
        alerts.push(alert(
            "expired_leases",
            "critical",
            format!(
                "{} running {queue} job(s) have a lease that ran out: their worker stopped renewing it",
                observed.expired_leases
            ),
            json!(observed.expired_leases),
            json!(0),
        ));
    }
    if consumer.enabled && connected && lagging {
        let (code, severity, cause) = if free > 0 {
            (
                "stalled",
                "critical",
                "although the consumer has free slots",
            )
        } else {
            ("backlog", "warning", "and every slot is taken")
        };
        alerts.push(alert(
            code,
            severity,
            format!(
                "A {queue} job has been due for {:.0} seconds, {cause}",
                observed.oldest_claimable_lag_seconds
            ),
            json!(observed.oldest_claimable_lag_seconds),
            json!(thresholds.ready_lag_seconds),
        ));
    }
    if observed.terminal_failures >= thresholds.terminal_failures {
        alerts.push(alert(
            "terminal_failures",
            "warning",
            format!(
                "{} {queue} job(s) failed for good in the last 24 hours",
                observed.terminal_failures
            ),
            json!(observed.terminal_failures),
            json!(thresholds.terminal_failures),
        ));
    }
    let state = if !consumer.enabled {
        "disabled"
    } else if !connected {
        "unreachable"
    } else if observed.expired_leases > 0 || (lagging && free > 0) {
        "unhealthy"
    } else if free == 0 {
        "saturated"
    } else if observed.running > 0 {
        "working"
    } else {
        "idle"
    };
    let critical = alerts.iter().any(|alert| alert["severity"] == "critical");
    json!({
        "queue": queue,
        "status": if critical { "failing" } else if alerts.is_empty() { "ok" } else { "degraded" },
        "state": state,
        "enabled": consumer.enabled,
        "consumers": observed.consumers,
        "slots": if connected { capacity } else { slots },
        "running": observed.running,
        "free_slots": if consumer.enabled && connected { free } else { 0 },
        "due": observed.due,
        "claimable": observed.claimable,
        "held_by_workspace_limit": (observed.due - observed.claimable).max(0),
        "workspace_limit": consumer.workspace_limit,
        "scheduled": observed.scheduled,
        "oldest_claimable_lag_seconds": observed.oldest_claimable_lag_seconds,
        "expired_leases": observed.expired_leases,
        "terminal_failures_24h": observed.terminal_failures,
        "interrupted_24h": observed.interrupted,
        "thresholds": {
            "ready_lag_seconds": thresholds.ready_lag_seconds,
            "terminal_failures": thresholds.terminal_failures,
        },
        "alerts": alerts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consumer(enabled: bool) -> Consumer {
        Consumer {
            queue: "factory-runs".into(),
            enabled,
            slots: 4,
            presence_lock: 7,
            workspace_limit: Some(3),
        }
    }

    fn codes(health: &Value) -> Vec<&str> {
        health["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|alert| alert["code"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn free_capacity_is_told_from_a_consumer_that_is_gone() {
        let nothing_running = Observed::default();
        // Nothing runs in both cases; only the presence lock tells them apart.
        let idle = assess(
            &consumer(true),
            &Observed {
                consumers: 1,
                ..nothing_running
            },
            &Thresholds::default(),
        );
        assert_eq!(
            (idle["state"].as_str(), idle["status"].as_str()),
            (Some("idle"), Some("ok"))
        );
        assert_eq!(idle["free_slots"], 4);
        assert!(codes(&idle).is_empty());
        let gone = assess(&consumer(true), &nothing_running, &Thresholds::default());
        assert_eq!(
            (gone["state"].as_str(), gone["status"].as_str()),
            (Some("unreachable"), Some("failing"))
        );
        assert_eq!(gone["free_slots"], 0);
        assert_eq!(codes(&gone), ["consumer_unreachable"]);
        // A server that does not consume the queue is neither: it alerts only when work waits for nobody.
        let off = assess(&consumer(false), &nothing_running, &Thresholds::default());
        assert_eq!(
            (off["state"].as_str(), off["status"].as_str()),
            (Some("disabled"), Some("ok"))
        );
        let waiting = assess(
            &consumer(false),
            &Observed {
                due: 2,
                claimable: 2,
                ..nothing_running
            },
            &Thresholds::default(),
        );
        assert_eq!(codes(&waiting), ["work_without_consumer"]);
        assert_eq!(waiting["status"], "degraded");
    }

    #[test]
    fn states_follow_the_slots_and_the_leases() {
        let connected = Observed {
            consumers: 1,
            ..Observed::default()
        };
        for (running, expired, state) in [
            (0, 0, "idle"),
            (1, 0, "working"),
            (3, 0, "working"),
            (4, 0, "saturated"),
            (2, 1, "unhealthy"),
            (4, 4, "unhealthy"),
        ] {
            let health = assess(
                &consumer(true),
                &Observed {
                    running,
                    expired_leases: expired,
                    ..connected
                },
                &Thresholds::default(),
            );
            assert_eq!(
                health["state"], state,
                "{running} running, {expired} expired"
            );
            assert_eq!(
                codes(&health).contains(&"expired_leases"),
                expired > 0,
                "{health}"
            );
        }
        // Two connected processes have twice the slots.
        let two = assess(
            &consumer(true),
            &Observed {
                consumers: 2,
                running: 4,
                ..Observed::default()
            },
            &Thresholds::default(),
        );
        assert_eq!(
            (two["state"].as_str(), two["slots"].as_i64()),
            (Some("working"), Some(8))
        );
    }

    #[test]
    fn the_lag_threshold_separates_a_stall_from_a_backlog_and_is_strict() {
        let thresholds = Thresholds {
            ready_lag_seconds: 60.0,
            terminal_failures: 3,
        };
        let waiting = |running, lag| Observed {
            consumers: 1,
            running,
            due: 5,
            claimable: 5,
            oldest_claimable_lag_seconds: lag,
            ..Observed::default()
        };
        // At the threshold nothing is raised; just past it, with free slots, the consumer is stalled.
        let at = assess(&consumer(true), &waiting(1, 60.0), &thresholds);
        assert!(codes(&at).is_empty(), "{at}");
        let stalled = assess(&consumer(true), &waiting(1, 60.5), &thresholds);
        assert_eq!(codes(&stalled), ["stalled"]);
        assert_eq!(
            (stalled["state"].as_str(), stalled["status"].as_str()),
            (Some("unhealthy"), Some("failing"))
        );
        assert_eq!(stalled["alerts"][0]["threshold"], 60.0);
        assert_eq!(stalled["alerts"][0]["value"], 60.5);
        // With every slot taken the same lag is a backlog: the consumer works, there is more than it can take.
        let backlog = assess(&consumer(true), &waiting(4, 600.0), &thresholds);
        assert_eq!(codes(&backlog), ["backlog"]);
        assert_eq!(
            (backlog["state"].as_str(), backlog["status"].as_str()),
            (Some("saturated"), Some("degraded"))
        );
        // Jobs held back by their workspace's limit are not claimable and stall nothing.
        let held = assess(
            &consumer(true),
            &Observed {
                consumers: 1,
                running: 3,
                due: 9,
                claimable: 0,
                oldest_claimable_lag_seconds: 0.0,
                ..Observed::default()
            },
            &thresholds,
        );
        assert!(codes(&held).is_empty(), "{held}");
        assert_eq!(held["held_by_workspace_limit"], 9);
        assert_eq!(held["state"], "working");
    }

    #[test]
    fn terminal_failures_alert_from_the_threshold_on() {
        let thresholds = Thresholds {
            ready_lag_seconds: 60.0,
            terminal_failures: 3,
        };
        for (failures, alerts) in [(0, false), (2, false), (3, true), (40, true)] {
            let health = assess(
                &consumer(true),
                &Observed {
                    consumers: 1,
                    terminal_failures: failures,
                    ..Observed::default()
                },
                &thresholds,
            );
            assert_eq!(
                codes(&health).contains(&"terminal_failures"),
                alerts,
                "{failures}"
            );
            assert_eq!(health["status"], if alerts { "degraded" } else { "ok" });
        }
    }

    #[test]
    fn interrupted_jobs_are_counted_apart_and_raise_no_alert() {
        let thresholds = Thresholds {
            ready_lag_seconds: 60.0,
            terminal_failures: 3,
        };
        // A restart with forty executions in flight, and two real failures: under the threshold of three.
        let health = assess(
            &consumer(true),
            &Observed {
                consumers: 1,
                terminal_failures: 2,
                interrupted: 40,
                ..Observed::default()
            },
            &thresholds,
        );
        assert!(codes(&health).is_empty(), "{health}");
        assert_eq!(health["status"], "ok");
        assert_eq!(health["interrupted_24h"], 40);
        assert_eq!(health["terminal_failures_24h"], 2);
        assert!(INTERRUPTED_PREFIX.ends_with(':'));
    }

    #[test]
    fn thresholds_read_their_settings_and_keep_the_defaults_otherwise() {
        assert_eq!(Thresholds::from_settings(None, None), Thresholds::default());
        assert_eq!(
            Thresholds::default(),
            Thresholds {
                ready_lag_seconds: 300.0,
                terminal_failures: 5
            }
        );
        assert_eq!(
            Thresholds::from_settings(Some(" 45.5 "), Some("2")),
            Thresholds {
                ready_lag_seconds: 45.5,
                terminal_failures: 2
            }
        );
        for (lag, failures) in [
            ("0", "0"),
            ("-1", "-3"),
            ("soon", "few"),
            ("", ""),
            ("NaN", "1.5"),
        ] {
            assert_eq!(
                Thresholds::from_settings(Some(lag), Some(failures)),
                Thresholds::default(),
                "{lag:?} {failures:?}"
            );
        }
    }
}
