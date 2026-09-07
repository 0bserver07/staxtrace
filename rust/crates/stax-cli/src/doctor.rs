//! `stax doctor` — `cli.py:6222`–`:6288` plus the four helpers it stands on
//! (`:5854`–`:6178`): the read-only health + delivery check.
//!
//! # Why this lives in `stax-cli` and not in `stax-reports`
//!
//! The ownership law says a service with two callers gets one home. These have
//! ONE: `_doctor_store_path` / `_open_store_readonly` / `_sqlite_object_exists`
//! / `_run_store_health_checks` / `_run_delivery_checks` are defined in
//! `cli.py`, imported by nothing outside it, and reached only by `doctor` and
//! (for the first two) `resume`. There is no route, no service module and no
//! second surface — `GET /api/doctor` does not exist. Enumerated the way
//! DIV-385 says a blocker should be: `grep -rn` over the whole Python tree,
//! and the only other hits are the two test modules. So the file-local home is
//! the deduped home, and moving it to `stax-reports` would invent a seam the
//! reference does not have.
//!
//! # Read-only is a guarantee, not an intention
//!
//! The store is opened `mode=ro` + `PRAGMA query_only = ON`, and every failure
//! to open — missing file, not a database, a locked WAL — becomes a *finding*
//! rather than an exception. `test_doctor_never_writes_the_store` asserts the
//! file's bytes AND its mtime are unchanged, and
//! `test_doctor_does_not_migrate_an_old_schema` asserts a v6 store stays v6.
//! That is the one place in the CLI where [`crate::reports::open_store`] (which
//! creates and migrates, DIV-374) would be actively wrong, so it is not used.
//!
//! # The delivery scoreboard is the point of the verb
//!
//! `_run_delivery_checks`'s docstring names the failure it exists to catch:
//! "codex (model=None), antigravity (no normalizer), and cursor-agent
//! (mis-keyed normalizer) went dark while 3,000+ tests stayed green". Six
//! statuses, and the ladder's ORDER is load-bearing:
//!
//! ```text
//! EXEMPT       capabilities.json says the source cannot bill  (checked FIRST)
//! OK           usage_events exist
//! GAP          billable base rows, zero events — stranded
//! NO_BILLABLE  base rows, none of them assistant-role — zero is CORRECT
//! DISK_GAP     sessions on disk, nothing ingested
//! EMPTY        nothing anywhere
//! ```
//!
//! `EXEMPT` before `OK` means an exempt provider that somehow *did* emit events
//! still reads `EXEMPT`; `GAP` before `NO_BILLABLE` is what makes
//! `billable_scan_error` bias toward the alarm. Both are inherited verbatim.
//!
//! # DIV-386 — `disk_sessions: null` cannot happen here
//!
//! Python's `_disk_count` wraps `adapter.enumerate()` in a bare `except` and
//! degrades to `None`, which the text renderer prints as `?`. The port's
//! [`stax_adapters::base::SourceAdapter::enumerate`] **cannot fail** — its
//! contract is `-> Vec<SessionRef>`, and wave 2 made "an absent source directory
//! yields an empty vector" the type. So the `None` leg is unreachable through
//! the registry, and a row can never prove it. It is kept in the *shape*
//! ([`ProviderRow::disk_sessions`] is an `Option`) and crossed by a unit test
//! instead, because a renderer branch nothing crosses is dead corpus (wave 6's
//! law) — and because the day an adapter's enumerate becomes fallible, the `?`
//! cell has to already be right.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use rusqlite::{Connection, OpenFlags};
use stax_adapters::Capabilities;
use stax_adapters::base::SourceAdapter;
use stax_adapters::capabilities::CAPABILITIES_PATH_ENV;
use stax_core::queries::pyjson;
use stax_core::queries::pyjson::Value;

use crate::click::Output;

/// `stax doctor`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit {"ok": bool, "findings": [...], "delivery": {...}} as JSON.
    #[arg(
        long = "json",
        help = "Emit {\"ok\": bool, \"findings\": [...], \"delivery\": {...}} as JSON."
    )]
    pub as_json: bool,
    /// Also exit non-zero when any provider's data is stranded.
    #[arg(
        long = "fail-on-gap",
        help = "Also exit non-zero when any provider's data is stranded \
                (GAP/DISK_GAP in the delivery scoreboard). For CI / pre-release gates."
    )]
    pub fail_on_gap: bool,
    /// Verify the installation itself (binary, siblings, signatures, data
    /// dir) instead of the store. Additive: the store report is parity-pinned.
    #[arg(long)]
    pub install: bool,
}

// ── health ───────────────────────────────────────────────────────────────────

/// One `{check, message}` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The check that produced it: `store`, `integrity`, `foreign_key`,
    /// `schema`, `watermark` or `orphan`.
    pub check: String,
    /// The human message, verbatim from `cli.py`.
    pub message: String,
}

/// `_run_store_health_checks`'s return value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    /// `not findings`.
    pub ok: bool,
    /// `str(store_path)`.
    pub store_path: String,
    /// In check order: integrity, foreign_key, schema, watermark, orphan.
    pub findings: Vec<Finding>,
}

impl Health {
    fn to_value(&self) -> Value {
        Value::Array(
            self.findings
                .iter()
                .map(|finding| {
                    Value::Object(vec![
                        ("check".into(), Value::from(&finding.check)),
                        ("message".into(), Value::from(&finding.message)),
                    ])
                })
                .collect(),
        )
    }
}

/// `_doctor_store_path()` — `Path(deps.store_path)`.
#[must_use]
pub fn doctor_store_path() -> PathBuf {
    stax_core::settings::store_path()
}

/// `f"file:{store_path}?mode=ro"` — the f-string, not a URI encoder.
///
/// A path containing `?`, `#` or `%` breaks this on BOTH sides, identically,
/// which is why `stax_core::store::sqlite_uri`'s escaping (and its
/// `?immutable=0`) is deliberately not reused here: percent-escaping would make
/// the port *work* on a path where the reference does not.
fn read_only_uri(store_path: &Path) -> String {
    format!("file:{}?mode=ro", store_path.display())
}

/// `_open_store_readonly` — `mode=ro` + `PRAGMA query_only`, or `None`.
///
/// The one home for the idiom, as the reference's own docstring insists.
#[must_use]
pub fn open_store_readonly(store_path: &Path) -> Option<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(read_only_uri(store_path), flags).ok()?;
    conn.execute_batch("PRAGMA query_only = ON").ok()?;
    Some(conn)
}

/// `_sqlite_object_exists` — a table or a view, read-only probe.
///
/// # Errors
/// When the query itself fails, which on a corrupt file is where the
/// `DatabaseError` surfaces; the callers catch it exactly where Python does.
pub fn sqlite_object_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?",
        [name],
        |_| Ok(()),
    )
    .map(|()| true)
    .or_else(|error| match error {
        rusqlite::Error::QueryReturnedNoRows => Ok(false),
        other => Err(other),
    })
}

/// `_run_store_health_checks(store_path)`.
///
/// Never returns an error: every failure mode is a finding, which is the
/// reference's contract and what `test_missing_store_is_a_finding_not_a_crash`
/// pins.
#[must_use]
pub fn run_store_health_checks(store_path: &Path) -> Health {
    let mut findings: Vec<Finding> = Vec::new();
    let rendered = store_path.display().to_string();

    if !store_path.exists() {
        push(
            &mut findings,
            "store",
            format!("store not found at {rendered} — run `stax start` to create it"),
        );
        return Health {
            ok: false,
            store_path: rendered,
            findings,
        };
    }

    // `sqlite3.connect(..., uri=True)` is lazy: it does not read the header, so
    // a garbage file opens fine and `PRAGMA query_only = ON` (a connection
    // setting) succeeds too. The first statement that touches a page is
    // `integrity_check`, and THAT is where "file is not a database" lands —
    // which is why the corrupt-store finding's check is `integrity`, not
    // `store` (`test_corrupt_store_is_a_finding`).
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = match Connection::open_with_flags(read_only_uri(store_path), flags) {
        Ok(conn) => conn,
        Err(error) => {
            push(
                &mut findings,
                "store",
                format!("cannot open store read-only: {error}"),
            );
            return Health {
                ok: false,
                store_path: rendered,
                findings,
            };
        }
    };
    let _ = conn.execute_batch("PRAGMA query_only = ON");

    // 1) Physical integrity. A non-database file raises here — that IS the
    //    finding, and nothing else is worth checking on a corrupt file.
    match integrity_rows(&conn) {
        Ok(rows) => {
            for row in rows {
                if row != "ok" {
                    push(&mut findings, "integrity", row);
                }
            }
        }
        Err(error) => {
            push(
                &mut findings,
                "integrity",
                format!("integrity check failed: {error}"),
            );
            return Health {
                ok: false,
                store_path: rendered,
                findings,
            };
        }
    }

    // 2) Declared foreign keys.
    match foreign_key_rows(&conn) {
        Ok(rows) => {
            for (table, rowid, referred) in rows {
                let where_ = match rowid {
                    Some(rowid) => format!("row {rowid}"),
                    None => "a row".to_owned(),
                };
                push(
                    &mut findings,
                    "foreign_key",
                    format!(
                        "foreign-key violation: {table} {where_} points at a missing {referred}"
                    ),
                );
            }
        }
        Err(error) => push(
            &mut findings,
            "foreign_key",
            format!("foreign-key check failed: {error}"),
        ),
    }

    // 3) Schema written by a newer build. `except Exception: pass` — advisory.
    if let Ok(user_version) = conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
    {
        let current = stax_core::schema::CURRENT_VERSION;
        if user_version > current {
            push(
                &mut findings,
                "schema",
                format!(
                    "store schema is v{user_version} but this build understands up to \
                     v{current} — it was written by a newer version"
                ),
            );
        }
    }

    // 4) Watermark sanity.
    if sqlite_object_exists(&conn, "mart_watermark").unwrap_or(false)
        && sqlite_object_exists(&conn, "usage_events").unwrap_or(false)
        && let Ok(max_eid) =
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM usage_events", [], |row| {
                row.get::<_, i64>(0)
            })
    {
        for (mart_name, last_event_id) in watermark_rows(&conn).unwrap_or_default() {
            if last_event_id > max_eid {
                push(
                    &mut findings,
                    "watermark",
                    format!(
                        "mart '{mart_name}' watermark (event {last_event_id}) is ahead of \
                         the newest event (id {max_eid})"
                    ),
                );
            }
        }
    }

    // 5) Orphan sanity on the denormalized marts.
    if sqlite_object_exists(&conn, "projects").unwrap_or(false) {
        for mart in ["session_mart", "daily_mart", "project_mart"] {
            if !sqlite_object_exists(&conn, mart).unwrap_or(false) {
                continue;
            }
            let count = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {mart} \
                         WHERE project_id NOT IN (SELECT id FROM projects)"
                    ),
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0);
            if count != 0 {
                push(
                    &mut findings,
                    "orphan",
                    format!("{mart}: {count} row(s) reference a project that no longer exists"),
                );
            }
        }
    }

    Health {
        ok: findings.is_empty(),
        store_path: rendered,
        findings,
    }
}

/// `_finding(check, message)` — the closure `_run_store_health_checks` defines.
fn push(findings: &mut Vec<Finding>, check: &str, message: String) {
    findings.push(Finding {
        check: check.to_owned(),
        message,
    });
}

fn integrity_rows(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    stmt.query_map([], |row| row.get::<_, String>(0))?.collect()
}

type ForeignKeyRow = (String, Option<i64>, String);

fn foreign_key_rows(conn: &Connection) -> rusqlite::Result<Vec<ForeignKeyRow>> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?
    .collect()
}

fn watermark_rows(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare("SELECT mart_name, last_event_id FROM mart_watermark")?;
    stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?
    .collect()
}

// ── delivery ─────────────────────────────────────────────────────────────────

/// One row of the per-provider scoreboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    /// The provider key.
    pub provider: String,
    /// Sessions the adapter can see on disk. `None` renders as `?` — see
    /// DIV-386 for why the port cannot reach it through the registry.
    pub disk_sessions: Option<i64>,
    /// `COUNT(DISTINCT sessions.id)`.
    pub base_sessions: i64,
    /// `SUM(sessions.message_count)`.
    pub base_messages: i64,
    /// `COUNT(*)` in `usage_events`.
    pub usage_events: i64,
    /// `SUM(provider_day_mart.message_count)`.
    pub mart_messages: i64,
    /// One of `OK` / `EXEMPT` / `GAP` / `NO_BILLABLE` / `DISK_GAP` / `EMPTY`.
    pub status: String,
}

/// `_run_delivery_checks`'s return value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// `not gaps`.
    pub ok: bool,
    /// Sorted by `(-base_messages, provider)`.
    pub providers: Vec<ProviderRow>,
    /// The `GAP` / `DISK_GAP` provider names, in row order.
    pub gaps: Vec<String>,
    /// Only present in the payload when true.
    pub billable_scan_error: bool,
}

impl Delivery {
    fn to_value(&self) -> Value {
        let mut out = vec![
            ("ok".into(), Value::Bool(self.ok)),
            (
                "providers".into(),
                Value::Array(
                    self.providers
                        .iter()
                        .map(|row| {
                            Value::Object(vec![
                                ("provider".into(), Value::from(&row.provider)),
                                (
                                    "disk_sessions".into(),
                                    row.disk_sessions.map_or(Value::Null, Value::Int),
                                ),
                                ("base_sessions".into(), Value::Int(row.base_sessions)),
                                ("base_messages".into(), Value::Int(row.base_messages)),
                                ("usage_events".into(), Value::Int(row.usage_events)),
                                ("mart_messages".into(), Value::Int(row.mart_messages)),
                                ("status".into(), Value::from(&row.status)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "gaps".into(),
                Value::Array(self.gaps.iter().map(Value::from).collect()),
            ),
        ];
        // `out["billable_scan_error"] = True` — appended last, and absent
        // entirely when the scan succeeded.
        if self.billable_scan_error {
            out.push(("billable_scan_error".into(), Value::Bool(true)));
        }
        Value::Object(out)
    }
}

/// `_disk_count` over every registered adapter, in registration order.
///
/// Python runs these in a `ThreadPoolExecutor(max_workers=min(8, n))` because
/// the walks are I/O-bound and independent; `pool.map` preserves order, so the
/// *result* is the sequential one and this port computes it sequentially. The
/// pool is a wall-time decision, not an output one — and `stax`'s enumerate is
/// the fast half of the 69–124× the hooks measured.
#[must_use]
pub fn enumerate_disk(adapters: &[Box<dyn SourceAdapter>]) -> Vec<(String, Option<i64>)> {
    adapters
        .iter()
        .map(|adapter| {
            (
                adapter.name().to_owned(),
                Some(i64::try_from(adapter.enumerate().len()).unwrap_or(i64::MAX)),
            )
        })
        .collect()
}

/// The per-metric store reads, each degrading INDEPENDENTLY.
///
/// The reference's comment says why in as many words: "a shared handler once let
/// a single odd partition skip the mart read and misreport a real GAP as
/// NO_BILLABLE". Four separate `try`s, and a fifth for the billable scan whose
/// failure sets a FLAG rather than swallowing the answer.
#[derive(Debug, Default)]
struct StoreTruth {
    base: BTreeMap<String, (i64, i64)>,
    events: BTreeMap<String, i64>,
    marts: BTreeMap<String, i64>,
    billable: BTreeMap<String, i64>,
    billable_scan_error: bool,
}

fn read_store_truth(
    conn: &Connection,
    disk_names: &BTreeSet<String>,
    exempt: &BTreeSet<String>,
) -> StoreTruth {
    let mut truth = StoreTruth::default();

    // `provider` is `TEXT NOT NULL` in every table read here, so a NULL key —
    // which would be a `None` in the Python dict and then a `TypeError` inside
    // `sorted()` — is unreachable by schema rather than by handling.
    if sqlite_object_exists(conn, "sessions").unwrap_or(false)
        && sqlite_object_exists(conn, "projects").unwrap_or(false)
    {
        let read = || -> rusqlite::Result<Vec<(String, i64, i64)>> {
            let mut stmt = conn.prepare(
                "SELECT p.provider AS provider,\
                 \x20      COUNT(DISTINCT s.id) AS sc,\
                 \x20      COALESCE(SUM(s.message_count), 0) AS mc\
                 \x20 FROM sessions s\
                 \x20 JOIN projects p ON s.project_id = p.id\
                 \x20GROUP BY p.provider",
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect()
        };
        if let Ok(rows) = read() {
            for (provider, sessions, messages) in rows {
                truth.base.insert(provider, (sessions, messages));
            }
        }
    }

    if sqlite_object_exists(conn, "usage_events").unwrap_or(false)
        && let Ok(rows) = group_count(
            conn,
            "SELECT provider, COUNT(*) AS n FROM usage_events GROUP BY provider",
        )
    {
        truth.events = rows;
    }

    if sqlite_object_exists(conn, "provider_day_mart").unwrap_or(false)
        && let Ok(rows) = group_count(
            conn,
            "SELECT provider, COALESCE(SUM(message_count), 0) AS n \
             FROM provider_day_mart GROUP BY provider",
        )
    {
        truth.marts = rows;
    }

    // The billable scan runs ONLY for providers with zero events and no
    // exemption, because it is a per-partition JOIN on an unindexed `role`.
    let need_billable: BTreeSet<String> = truth
        .base
        .keys()
        .chain(disk_names.iter())
        .filter(|name| {
            truth.events.get(*name).copied().unwrap_or(0) == 0 && !exempt.contains(*name)
        })
        .cloned()
        .collect();
    if !need_billable.is_empty() {
        match scan_billable(conn, &need_billable) {
            Ok(rows) => truth.billable = rows,
            // Fail SAFE: a read error must surface a possible gap, never mask one.
            Err(_) => truth.billable_scan_error = true,
        }
    }
    truth
}

fn group_count(conn: &Connection, sql: &str) -> rusqlite::Result<BTreeMap<String, i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().collect())
}

fn scan_billable(
    conn: &Connection,
    need: &BTreeSet<String>,
) -> rusqlite::Result<BTreeMap<String, i64>> {
    let placeholders = std::iter::repeat_n("?", need.len())
        .collect::<Vec<_>>()
        .join(",");
    // `sorted(need_billable)` — a `BTreeSet`'s iteration order IS that sort.
    let params: Vec<String> = need.iter().cloned().collect();

    let partitions: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'messages_%'",
        )?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut billable: BTreeMap<String, i64> = BTreeMap::new();
    for part in partitions {
        let sql = format!(
            "SELECT p.provider AS provider, COUNT(*) AS n \
             FROM {part} m \
             JOIN sessions s ON m.session_fk = s.id \
             JOIN projects p ON s.project_id = p.id \
             WHERE m.role = 'assistant' AND p.provider IN ({placeholders}) \
             GROUP BY p.provider"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (provider, count) in rows {
            *billable.entry(provider).or_insert(0) += count;
        }
    }
    Ok(billable)
}

/// `_run_delivery_checks(store_path, adapters_override=…)`.
///
/// `disk` is passed in rather than enumerated here — that is what
/// `adapters_override` exists for on the Python side, and it is also what keeps
/// the unreachable `disk_sessions: None` leg (DIV-386) crossable by a test.
#[must_use]
pub fn run_delivery_checks(
    store_path: &Path,
    disk: &[(String, Option<i64>)],
    exempt: &BTreeSet<String>,
) -> Delivery {
    let disk_map: BTreeMap<String, Option<i64>> = disk.iter().cloned().collect();
    let disk_names: BTreeSet<String> = disk_map.keys().cloned().collect();

    let truth = if store_path.exists() {
        match open_store_readonly(store_path) {
            Some(conn) => read_store_truth(&conn, &disk_names, exempt),
            None => StoreTruth::default(),
        }
    } else {
        StoreTruth::default()
    };

    // `sorted(set(disk) | set(base) | set(events))` — note `marts` is NOT in the
    // union, so a provider present only in `provider_day_mart` is invisible.
    let providers: BTreeSet<String> = disk_names
        .iter()
        .chain(truth.base.keys())
        .chain(truth.events.keys())
        .cloned()
        .collect();

    let mut rows: Vec<ProviderRow> = Vec::with_capacity(providers.len());
    let mut gaps: Vec<String> = Vec::new();
    for name in providers {
        let disk_sessions = disk_map.get(&name).copied().flatten();
        let (base_sessions, base_messages) = truth.base.get(&name).copied().unwrap_or((0, 0));
        let n_events = truth.events.get(&name).copied().unwrap_or(0);
        let status = if exempt.contains(&name) {
            "EXEMPT"
        } else if n_events > 0 {
            "OK"
        } else if truth.billable.get(&name).copied().unwrap_or(0) > 0
            || (truth.billable_scan_error && base_messages > 0)
        {
            // A failed billable scan biases base-bearing providers to GAP —
            // flag, never mask.
            "GAP"
        } else if base_messages > 0 {
            "NO_BILLABLE"
        } else if disk_sessions.is_some_and(|count| count != 0) {
            // `elif disk_sessions:` — Python truthiness, so BOTH `None` and `0`
            // fall through to EMPTY.
            "DISK_GAP"
        } else {
            "EMPTY"
        };
        if status == "GAP" || status == "DISK_GAP" {
            gaps.push(name.clone());
        }
        let mart_messages = truth.marts.get(&name).copied().unwrap_or(0);
        rows.push(ProviderRow {
            provider: name,
            disk_sessions,
            base_sessions,
            base_messages,
            usage_events: n_events,
            mart_messages,
            status: status.to_owned(),
        });
    }
    // `rows.sort(key=lambda r: (-r["base_messages"], r["provider"]))` — a STABLE
    // sort over a list already in provider order, so the tiebreak is redundant
    // and inherited anyway.
    rows.sort_by(|left, right| {
        (-left.base_messages, &left.provider).cmp(&(-right.base_messages, &right.provider))
    });
    // `gaps` is built during the pre-sort pass, so it is in PROVIDER order while
    // the table is in spend order. The "stranded providers:" line therefore does
    // not follow the table's row order. Inherited.
    Delivery {
        ok: gaps.is_empty(),
        providers: rows,
        gaps,
        billable_scan_error: truth.billable_scan_error,
    }
}

/// The `emits_usage_events: false` names, over the WHOLE capability table.
///
/// Python iterates `_CAPABILITIES.items()`, not the registry, so a curated row
/// for a provider no adapter answers for still contributes an exemption — it
/// simply never appears in `providers` unless the store carries it.
#[must_use]
pub fn exempt_providers(capabilities: &Capabilities) -> BTreeSet<String> {
    capabilities
        .iter()
        .filter(|entry| !entry.emits_usage_events)
        .map(|entry| entry.provider.clone())
        .collect()
}

// ── rendering ────────────────────────────────────────────────────────────────

/// The text block: health, a blank line, the scoreboard, and the stranded note.
#[must_use]
pub fn render_doctor_text(health: &Health, delivery: &Delivery) -> String {
    let mut out = String::new();
    if health.ok {
        out.push_str("ok\n");
    } else {
        for finding in &health.findings {
            out.push_str(&finding.message);
            out.push('\n');
        }
    }
    out.push('\n');
    out.push_str("delivery (disk sessions → base messages → usage events → marts):\n");
    out.push_str(&format!(
        "  {:<14} {:>6} {:>10} {:>8} {:>8}  status\n",
        "provider", "disk", "base_msgs", "events", "marts"
    ));
    for row in &delivery.providers {
        // `"?" if row["disk_sessions"] is None else row["disk_sessions"]`, then
        // `{disk_cell!s:>6}` — `str()` of either, right-aligned in six.
        let disk_cell = row
            .disk_sessions
            .map_or_else(|| "?".to_owned(), |count| count.to_string());
        out.push_str(&format!(
            "  {:<14} {:>6} {:>10} {:>8} {:>8}  {}\n",
            row.provider,
            disk_cell,
            row.base_messages,
            row.usage_events,
            row.mart_messages,
            row.status
        ));
    }
    if !delivery.gaps.is_empty() {
        out.push_str(&format!(
            "  stranded providers: {} — data exists but never reaches usage_events \
             (run ingest + `etl backfill`, or fix the adapter/normalizer)\n",
            delivery.gaps.join(", ")
        ));
    }
    out
}

/// `json.dumps({...}, indent=2)` — the four top-level keys, in `cli.py`'s order.
#[must_use]
pub fn render_doctor_json(health: &Health, delivery: &Delivery) -> String {
    let payload = Value::Object(vec![
        ("ok".into(), Value::Bool(health.ok)),
        ("findings".into(), health.to_value()),
        ("store_path".into(), Value::from(&health.store_path)),
        ("delivery".into(), delivery.to_value()),
    ]);
    format!("{}\n", pyjson::dumps_indent2(&payload))
}

/// Run `doctor`.
///
/// # Errors
/// When `adapters/capabilities.json` cannot be found or parsed — the reference
/// reaches the same table through a module-level import, so a broken table is a
/// hard failure there too.
pub fn run_doctor(args: &DoctorArgs) -> Result<Output> {
    if args.install {
        return crate::doctor_install::run_install_checks();
    }
    let store_path = doctor_store_path();
    let health = run_store_health_checks(&store_path);

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe = std::env::current_exe().ok();
    let table = crate::resume::resolve_capabilities_path(
        std::env::var_os(CAPABILITIES_PATH_ENV).as_deref(),
        &cwd,
        exe.as_deref(),
    );
    // `load_or_embedded`: the walk-up finds the file in any checkout, and a
    // binary running where there is no `stackunderflow/` package reads the copy
    // compiled into `stax-adapters` instead of failing (wave-10 item 2c).
    let capabilities = Capabilities::load_or_embedded(&table)
        .with_context(|| format!("loading {}", table.display()))?;
    let adapters = stax_adapters::registry::registered();
    let delivery = run_delivery_checks(
        &store_path,
        &enumerate_disk(&adapters),
        &exempt_providers(&capabilities),
    );

    let body = if args.as_json {
        render_doctor_json(&health, &delivery)
    } else {
        render_doctor_text(&health, &delivery)
    };
    if !health.ok || (args.fail_on_gap && !delivery.ok) {
        return Ok(Output::exit1(body));
    }
    Ok(Output::ok(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(provider: &str, disk: Option<i64>, base: i64, events: i64, status: &str) -> ProviderRow {
        ProviderRow {
            provider: provider.into(),
            disk_sessions: disk,
            base_sessions: 0,
            base_messages: base,
            usage_events: events,
            mart_messages: 0,
            status: status.into(),
        }
    }

    fn healthy() -> Health {
        Health {
            ok: true,
            store_path: "/h/store.db".into(),
            findings: Vec::new(),
        }
    }

    #[test]
    fn the_header_columns_are_pythons_f_string_widths() {
        let delivery = Delivery {
            ok: true,
            providers: vec![row("claude", Some(0), 56, 0, "GAP")],
            gaps: vec!["claude".into()],
            billable_scan_error: false,
        };
        let text = render_doctor_text(&healthy(), &delivery);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "ok");
        assert_eq!(lines[1], "");
        assert_eq!(
            lines[2],
            "delivery (disk sessions → base messages → usage events → marts):"
        );
        assert_eq!(
            lines[3],
            "  provider         disk  base_msgs   events    marts  status"
        );
        assert_eq!(
            lines[4],
            "  claude              0         56        0        0  GAP"
        );
        assert_eq!(
            lines[5],
            "  stranded providers: claude — data exists but never reaches usage_events \
             (run ingest + `etl backfill`, or fix the adapter/normalizer)"
        );
    }

    #[test]
    fn an_unknown_disk_count_renders_as_a_question_mark() {
        // DIV-386: unreachable through the registry, so the branch lives here.
        let delivery = Delivery {
            ok: true,
            providers: vec![row("cursor", None, 0, 0, "EMPTY")],
            gaps: Vec::new(),
            billable_scan_error: false,
        };
        let text = render_doctor_text(&healthy(), &delivery);
        assert!(
            text.contains("  cursor              ?          0        0        0  EMPTY"),
            "`{{disk_cell!s:>6}}` right-aligns the literal `?`:\n{text}"
        );
        assert!(!text.contains("stranded"), "no gaps, no note");
    }

    #[test]
    fn every_finding_message_is_printed_when_health_fails() {
        let health = Health {
            ok: false,
            store_path: "/h/store.db".into(),
            findings: vec![
                Finding {
                    check: "integrity".into(),
                    message: "integrity check failed: file is not a database".into(),
                },
                Finding {
                    check: "orphan".into(),
                    message: "session_mart: 1 row(s) reference a project that no longer exists"
                        .into(),
                },
            ],
        };
        let delivery = Delivery {
            ok: true,
            providers: Vec::new(),
            gaps: Vec::new(),
            billable_scan_error: false,
        };
        let text = render_doctor_text(&health, &delivery);
        assert!(text.starts_with(
            "integrity check failed: file is not a database\n\
             session_mart: 1 row(s) reference a project that no longer exists\n\n"
        ));
        assert!(
            !text.contains("\nok\n"),
            "`ok` is the else branch, not a prefix"
        );
    }

    #[test]
    fn the_json_envelope_is_four_keys_in_cli_order_with_ascii_escapes() {
        let health = Health {
            ok: false,
            store_path: "/h/store.db".into(),
            findings: vec![Finding {
                check: "store".into(),
                message: "store not found at /h/store.db — run `stax start` to create it".into(),
            }],
        };
        let delivery = Delivery {
            ok: true,
            providers: Vec::new(),
            gaps: Vec::new(),
            billable_scan_error: false,
        };
        let json = render_doctor_json(&health, &delivery);
        assert!(json.starts_with("{\n  \"ok\": false,\n  \"findings\": [\n"));
        assert!(
            json.contains("\\u2014"),
            "`json.dumps` is ensure_ascii=True, so the em-dash escapes:\n{json}"
        );
        assert!(json.contains("\"store_path\": \"/h/store.db\""));
        assert!(json.ends_with("  }\n}\n"));
        // The delivery block's key order, and no `billable_scan_error` key.
        assert!(json.contains(
            "\"delivery\": {\n    \"ok\": true,\n    \"providers\": [],\n    \"gaps\": []\n  }"
        ));
    }

    #[test]
    fn billable_scan_error_is_appended_last_and_only_when_true() {
        let mut delivery = Delivery {
            ok: true,
            providers: Vec::new(),
            gaps: Vec::new(),
            billable_scan_error: true,
        };
        let json = render_doctor_json(&healthy(), &delivery);
        assert!(json.contains("\"gaps\": [],\n    \"billable_scan_error\": true"));
        delivery.billable_scan_error = false;
        assert!(!render_doctor_json(&healthy(), &delivery).contains("billable_scan_error"));
    }

    #[test]
    fn the_table_sorts_by_descending_base_messages_then_name() {
        let disk = vec![
            ("zeta".to_owned(), Some(0)),
            ("alpha".to_owned(), Some(0)),
            ("beta".to_owned(), Some(0)),
        ];
        let delivery = run_delivery_checks(
            Path::new("/definitely/not/a/store.db"),
            &disk,
            &BTreeSet::new(),
        );
        let names: Vec<&str> = delivery
            .providers
            .iter()
            .map(|row| row.provider.as_str())
            .collect();
        assert_eq!(
            names,
            ["alpha", "beta", "zeta"],
            "all zero base_messages, so the tiebreak is the provider name"
        );
        assert!(delivery.ok && delivery.gaps.is_empty());
        assert!(delivery.providers.iter().all(|row| row.status == "EMPTY"));
    }

    #[test]
    fn a_provider_with_sessions_on_disk_and_nothing_ingested_is_disk_gap() {
        let disk = vec![("claude".to_owned(), Some(12))];
        let delivery = run_delivery_checks(Path::new("/no/store.db"), &disk, &BTreeSet::new());
        assert_eq!(delivery.providers[0].status, "DISK_GAP");
        assert_eq!(delivery.gaps, ["claude"]);
        assert!(!delivery.ok);
    }

    #[test]
    fn exempt_wins_over_every_other_rung() {
        let disk = vec![("antigravity".to_owned(), Some(9))];
        let exempt: BTreeSet<String> = ["antigravity".to_owned()].into_iter().collect();
        let delivery = run_delivery_checks(Path::new("/no/store.db"), &disk, &exempt);
        assert_eq!(
            delivery.providers[0].status, "EXEMPT",
            "the exemption is checked FIRST, ahead of DISK_GAP"
        );
        assert!(delivery.gaps.is_empty());
    }

    #[test]
    fn a_disk_count_of_zero_is_falsy_and_lands_on_empty() {
        let delivery = run_delivery_checks(
            Path::new("/no/store.db"),
            &[("codex".to_owned(), Some(0))],
            &BTreeSet::new(),
        );
        assert_eq!(delivery.providers[0].status, "EMPTY");
        // And so is `None` — `elif disk_sessions:` on a `None`.
        let delivery = run_delivery_checks(
            Path::new("/no/store.db"),
            &[("codex".to_owned(), None)],
            &BTreeSet::new(),
        );
        assert_eq!(delivery.providers[0].status, "EMPTY");
        assert_eq!(delivery.providers[0].disk_sessions, None);
    }

    #[test]
    fn a_missing_store_is_a_finding_and_the_file_is_not_created() {
        let dir = std::env::temp_dir().join(format!("stax-doctor-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = dir.join("nope.db");
        let _ = std::fs::remove_file(&store);
        let health = run_store_health_checks(&store);
        assert!(!health.ok);
        assert_eq!(health.findings.len(), 1);
        assert_eq!(health.findings[0].check, "store");
        assert!(
            health.findings[0]
                .message
                .ends_with("— run `stax start` to create it")
        );
        assert!(
            !store.exists(),
            "doctor must not create what it cannot find"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_garbage_file_is_an_integrity_finding_not_a_crash() {
        let dir = std::env::temp_dir().join(format!("stax-doctor-c-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = dir.join("store.db");
        let junk = b"definitely not a sqlite database".repeat(64);
        std::fs::write(&store, &junk).expect("writing the junk file");
        let health = run_store_health_checks(&store);
        assert!(!health.ok);
        assert_eq!(
            health
                .findings
                .iter()
                .map(|f| f.check.as_str())
                .collect::<Vec<_>>(),
            ["integrity"],
            "the corrupt file short-circuits every later check"
        );
        assert_eq!(
            std::fs::read(&store).expect("re-reading"),
            junk,
            "the garbage file is untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_read_only_uri_is_the_f_string_and_not_a_url_encoder() {
        assert_eq!(
            read_only_uri(Path::new("/h/store.db")),
            "file:/h/store.db?mode=ro"
        );
        assert_eq!(
            read_only_uri(Path::new("/h/a?b.db")),
            "file:/h/a?b.db?mode=ro",
            "the reference builds this with an f-string, so a `?` breaks BOTH sides"
        );
    }
}
