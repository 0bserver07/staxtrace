//! Release-only evidence for ingest, refresh, rebuild, and WAL concurrency.
//!
//! Fixtures are generated deterministically from checked-in cardinalities. The
//! timed code is the production ingest/mart/storage path; setup and fixture
//! generation remain outside each latency sample.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::json;
use stax_adapters::base::{Record, SessionRef, SourceAdapter, Speed};
use stax_etl::ingest::{FixedClock, ReindexConfig, ingest_file, run_ingest};
use stax_etl::normalize::NormalizeContext;

#[path = "../../../perf/support.rs"]
mod perf;

const EVENTS_PER_FILE: usize = 1_000;
const BATCH_FILES: usize = 20;
const ONE_FILE_SAMPLES: usize = 30;
const BATCH_SAMPLES: usize = 10;
const NOOP_SAMPLES: usize = 100;
const CORPUS_ROWS: i64 = 100_000;
const PREFIX_ROWS: i64 = 90_000;
const CHANGED_ROWS: i64 = 10_000;
const INCREMENTAL_SAMPLES: usize = 10;
const FULL_SAMPLES: usize = 5;
const CORPUS_PROJECTS: i64 = 20;
const CORPUS_SESSIONS: i64 = 1_000;
const CORPUS_DAYS: i64 = 28;
const BLOCKS_PER_SESSION: i64 = 10;
const EVENTS_PER_BLOCK: i64 = 10;
const USER_PROMPTS: i64 = CORPUS_SESSIONS * BLOCKS_PER_SESSION;
const TOOL_RESULT_MESSAGES: i64 = USER_PROMPTS;
const MESSAGE_ROWS: i64 = CORPUS_ROWS + USER_PROMPTS + TOOL_RESULT_MESSAGES;
const TOOL_EVENTS: i64 = USER_PROMPTS;
const TAIL_DAY_PROJECT_GROUPS: i64 = 140;
const REGISTERED_MARTS: i64 = 8;
const MESSAGES_PER_BLOCK: i64 = EVENTS_PER_BLOCK + 2;
const PREFIX_PROJECT_MTIME: f64 = 1_767_225_600.0;
const TAIL_PROJECT_MTIME: f64 = 1_767_225_601.0;
// A power-of-two fraction keeps incremental and full SQLite SUM paths bit-exact.
const FIXTURE_COST_USD: f64 = 1.0 / 1024.0;
const CONCURRENT_WRITERS: u64 = 1;
const CONCURRENT_READERS: u64 = 8;
const CONCURRENT_SECONDS: u64 = 30;
const FIXTURE_PROVIDER: &str = "codex";

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "stax-performance-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create performance scratch directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fresh_store(path: &Path) -> Connection {
    let conn = stax_etl::ingest::guard::open_read_write(path)
        .expect("open performance store through production connection path");
    stax_core::schema::apply(&conn).expect("apply production schema");
    assert_production_pragmas(&conn);
    conn
}

fn production_pragma_mismatch(conn: &Connection) -> Result<(), String> {
    let journal: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .map_err(|error| format!("read journal_mode: {error}"))?;
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .map_err(|error| format!("read synchronous: {error}"))?;
    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(|error| format!("read foreign_keys: {error}"))?;
    if journal != "wal" || synchronous != 1 || foreign_keys != 1 {
        return Err(format!(
            "production pragma mismatch: journal_mode={journal}, synchronous={synchronous}, \
             foreign_keys={foreign_keys}"
        ));
    }
    Ok(())
}

fn assert_production_pragmas(conn: &Connection) {
    production_pragma_mismatch(conn)
        .expect("timed database must use WAL, synchronous=NORMAL, and foreign-key enforcement");
}

#[test]
fn production_pragma_contract_rejects_a_mismatch() {
    let scratch = Scratch::new("pragma-negative");
    let conn = fresh_store(&scratch.path().join("store.db"));
    conn.pragma_update(None, "synchronous", "FULL")
        .expect("induce pragma mismatch");
    let error = production_pragma_mismatch(&conn).expect_err("FULL must be rejected");
    assert!(error.contains("synchronous=2"), "typed mismatch: {error}");
}

fn normalize_context() -> NormalizeContext {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/data/models.toml");
    NormalizeContext::unprimed(&manifest).expect("load checked-in model manifest")
}

fn fixed_clock() -> FixedClock {
    FixedClock::new(1_767_225_600.0, "2026-01-01T00:00:00+00:00")
}

fn count(conn: &Connection, table: &str) -> u64 {
    let value: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("counting {table}: {error}"));
    u64::try_from(value).expect("non-negative row count")
}

fn remove_store_files(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        match std::fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove performance sample store: {error}"),
        }
    }
}

struct FixtureAdapter {
    refs: Vec<SessionRef>,
    records: BTreeMap<String, Vec<Record>>,
}

impl FixtureAdapter {
    fn new(root: &Path, files: usize, events_per_file: usize) -> Self {
        let mut refs = Vec::with_capacity(files);
        let mut records = BTreeMap::new();
        for file_index in 0..files {
            let session_id = format!("fixture-session-{file_index}");
            refs.push(SessionRef::file(
                FIXTURE_PROVIDER,
                format!("fixture-project-{}", file_index % 4),
                &session_id,
                root.join(format!("trace-{file_index}.jsonl")),
                1.0,
                (events_per_file * 128) as u64,
            ));
            let rows = (1..=events_per_file)
                .map(|index| Record {
                    provider: FIXTURE_PROVIDER.to_owned(),
                    session_id: session_id.clone(),
                    seq: index as i64,
                    timestamp: "2026-01-01T00:00:00+00:00".to_owned(),
                    role: "assistant".to_owned(),
                    model: Some("gpt-5".to_owned()),
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_create_tokens: 0,
                    cache_read_tokens: 0,
                    content_text: "deterministic trace event".to_owned(),
                    tools: Vec::new(),
                    cwd: None,
                    is_sidechain: false,
                    uuid: format!("{session_id}:{index}"),
                    parent_uuid: None,
                    raw: json!({}),
                    speed: Speed::Standard,
                })
                .collect();
            records.insert(session_id, rows);
        }
        Self { refs, records }
    }

    fn assert_cardinality(&self, files: usize, events_per_file: usize) {
        perf::assert_exact("ingest files", self.refs.len() as u64, files as u64);
        perf::assert_exact(
            "ingest events before timing",
            self.records.values().map(Vec::len).sum::<usize>() as u64,
            (files * events_per_file) as u64,
        );
        for reference in &self.refs {
            let rows = self
                .records
                .get(&reference.session_id)
                .expect("records for every fixture ref");
            let unique: BTreeSet<i64> = rows.iter().map(|record| record.seq).collect();
            perf::assert_exact(
                "post-deduplication events per file",
                unique.len() as u64,
                events_per_file as u64,
            );
        }
    }
}

impl SourceAdapter for FixtureAdapter {
    fn name(&self) -> &str {
        FIXTURE_PROVIDER
    }

    fn enumerate(&self) -> Vec<SessionRef> {
        self.refs.clone()
    }

    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record)) {
        if let Some(records) = self.records.get(&session.session_id) {
            for record in records {
                if since_offset == 0 || record.seq > since_offset {
                    sink(record.clone());
                }
            }
        }
    }
}

#[test]
#[ignore = "release-only performance evidence; run rust/perf-gate.sh"]
fn one_file_append_distribution() {
    let fixture = &[("events_per_file", EVENTS_PER_FILE as u64), ("files", 1)];
    let guard = perf::Guard::MinThroughput(1_000.0);
    perf::prepare_scenario("one_file_append", fixture, Some(ONE_FILE_SAMPLES), guard);
    let scenario_started = Instant::now();
    let monitor = perf::ResourceMonitor::start_continuous();
    let scratch = Scratch::new("one-file");
    let adapter = FixtureAdapter::new(scratch.path(), 1, EVENTS_PER_FILE);
    adapter.assert_cardinality(1, EVENTS_PER_FILE);
    let session = adapter.refs.first().expect("one fixture ref").clone();
    let context = normalize_context();
    let clock = fixed_clock();

    let mut samples = Vec::with_capacity(ONE_FILE_SAMPLES);
    for sample in 0..ONE_FILE_SAMPLES {
        let path = scratch.path().join(format!("sample-{sample}.db"));
        let conn = fresh_store(&path);
        perf::assert_exact("one-file pre-ingest messages", count(&conn, "messages"), 0);
        let started = Instant::now();
        let report = ingest_file(&conn, &adapter, &session, 0, &context, &clock)
            .expect("ingest one fixture file");
        samples.push(perf::elapsed_ms(started));
        perf::assert_exact(
            "one-file messages added",
            report.messages_added,
            EVENTS_PER_FILE as u64,
        );
        perf::assert_exact(
            "one-file usage events added",
            report.events_inserted,
            EVENTS_PER_FILE as u64,
        );
        perf::assert_exact(
            "one-file post-deduplication messages",
            count(&conn, "messages"),
            EVENTS_PER_FILE as u64,
        );
        perf::assert_exact(
            "one-file post-deduplication usage events",
            count(&conn, "usage_events"),
            EVENTS_PER_FILE as u64,
        );
        drop(conn);
        remove_store_files(&path);
    }
    let resources = monitor.stop();
    let timed_wall = perf::sum_sample_time(&samples);
    perf::write_receipt(
        "one_file_append",
        fixture,
        Some(ONE_FILE_SAMPLES),
        &samples,
        perf::Timing::from_started(scenario_started, timed_wall, Duration::ZERO),
        Some((EVENTS_PER_FILE * ONE_FILE_SAMPLES) as u64),
        resources,
        guard,
    );
}

#[test]
#[ignore = "release-only performance evidence; run rust/perf-gate.sh"]
fn multi_file_batch_ingest_distribution() {
    let expected_events = (BATCH_FILES * EVENTS_PER_FILE) as u64;
    let fixture = &[
        ("events_per_file", EVENTS_PER_FILE as u64),
        ("files", BATCH_FILES as u64),
        ("events", expected_events),
    ];
    let guard = perf::Guard::MinThroughput(2_000.0);
    perf::prepare_scenario("batch_ingest", fixture, Some(BATCH_SAMPLES), guard);
    let scenario_started = Instant::now();
    let monitor = perf::ResourceMonitor::start_continuous();
    let scratch = Scratch::new("batch");
    let adapter = FixtureAdapter::new(scratch.path(), BATCH_FILES, EVENTS_PER_FILE);
    adapter.assert_cardinality(BATCH_FILES, EVENTS_PER_FILE);
    let context = normalize_context();
    let clock = fixed_clock();
    let mut samples = Vec::with_capacity(BATCH_SAMPLES);
    for sample in 0..BATCH_SAMPLES {
        let path = scratch.path().join(format!("sample-{sample}.db"));
        let conn = fresh_store(&path);
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(FixtureAdapter {
            refs: adapter.refs.clone(),
            records: adapter.records.clone(),
        })];
        perf::assert_exact("batch pre-ingest messages", count(&conn, "messages"), 0);
        let started = Instant::now();
        let report = run_ingest(
            &conn,
            &adapters,
            &context,
            &clock,
            &ReindexConfig::default(),
        )
        .expect("ingest deterministic batch");
        samples.push(perf::elapsed_ms(started));
        perf::assert_exact(
            "batch files processed",
            report.files_processed,
            BATCH_FILES as u64,
        );
        perf::assert_exact(
            "batch normalized events",
            report.events_inserted,
            expected_events,
        );
        perf::assert_exact(
            "batch messages after deduplication",
            count(&conn, "messages"),
            expected_events,
        );
        perf::assert_exact(
            "batch events after deduplication",
            count(&conn, "usage_events"),
            expected_events,
        );
        drop(conn);
        remove_store_files(&path);
    }
    let resources = monitor.stop();
    let timed_wall = perf::sum_sample_time(&samples);
    perf::write_receipt(
        "batch_ingest",
        fixture,
        Some(BATCH_SAMPLES),
        &samples,
        perf::Timing::from_started(scenario_started, timed_wall, Duration::ZERO),
        Some(expected_events * BATCH_SAMPLES as u64),
        resources,
        guard,
    );
}

#[test]
#[ignore = "release-only performance evidence; run rust/perf-gate.sh"]
fn unchanged_multi_file_refresh_distribution() {
    let expected_events = (BATCH_FILES * EVENTS_PER_FILE) as u64;
    let fixture = &[
        ("unchanged_files", BATCH_FILES as u64),
        ("events", expected_events),
    ];
    let guard = perf::Guard::MaxP99Ms(100.0);
    perf::prepare_scenario("noop_refresh", fixture, Some(NOOP_SAMPLES), guard);
    let scenario_started = Instant::now();
    let monitor = perf::ResourceMonitor::start_continuous();
    let scratch = Scratch::new("noop");
    let adapter = FixtureAdapter::new(scratch.path(), BATCH_FILES, EVENTS_PER_FILE);
    adapter.assert_cardinality(BATCH_FILES, EVENTS_PER_FILE);
    let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(adapter)];
    let context = normalize_context();
    let clock = fixed_clock();
    let conn = fresh_store(&scratch.path().join("store.db"));
    let initial = run_ingest(
        &conn,
        &adapters,
        &context,
        &clock,
        &ReindexConfig::default(),
    )
    .expect("prime no-op fixture");
    perf::assert_exact(
        "no-op initial files",
        initial.files_processed,
        BATCH_FILES as u64,
    );
    perf::assert_exact(
        "no-op fixture messages",
        count(&conn, "messages"),
        expected_events,
    );
    perf::assert_exact(
        "no-op fixture events",
        count(&conn, "usage_events"),
        expected_events,
    );

    let mut samples = Vec::with_capacity(NOOP_SAMPLES);
    for _ in 0..NOOP_SAMPLES {
        let started = Instant::now();
        let report = run_ingest(
            &conn,
            &adapters,
            &context,
            &clock,
            &ReindexConfig::default(),
        )
        .expect("refresh unchanged fixture");
        samples.push(perf::elapsed_ms(started));
        perf::assert_exact(
            "no-op unchanged files",
            report.files_skipped,
            BATCH_FILES as u64,
        );
        perf::assert_exact("no-op processed files", report.files_processed, 0);
        perf::assert_exact("no-op inserted events", report.events_inserted, 0);
    }
    let resources = monitor.stop();
    perf::assert_exact(
        "no-op stable messages",
        count(&conn, "messages"),
        expected_events,
    );
    perf::assert_exact(
        "no-op stable events",
        count(&conn, "usage_events"),
        expected_events,
    );
    perf::write_receipt(
        "noop_refresh",
        fixture,
        Some(NOOP_SAMPLES),
        &samples,
        perf::Timing::from_started(
            scenario_started,
            perf::sum_sample_time(&samples),
            Duration::ZERO,
        ),
        None,
        resources,
        guard,
    );
}

fn reindex_fixture() -> [(&'static str, u64); 17] {
    [
        ("corpus_rows", CORPUS_ROWS as u64),
        ("prefix_rows", PREFIX_ROWS as u64),
        ("changed_rows", CHANGED_ROWS as u64),
        ("message_rows", MESSAGE_ROWS as u64),
        ("assistant_source_messages", CORPUS_ROWS as u64),
        ("user_prompts", USER_PROMPTS as u64),
        ("tool_result_messages", TOOL_RESULT_MESSAGES as u64),
        ("source_linked_events", CORPUS_ROWS as u64),
        ("tool_events", TOOL_EVENTS as u64),
        ("projects", CORPUS_PROJECTS as u64),
        ("sessions", CORPUS_SESSIONS as u64),
        ("days", CORPUS_DAYS as u64),
        ("tail_projects", CORPUS_PROJECTS as u64),
        ("tail_sessions", CORPUS_SESSIONS as u64),
        ("tail_days", CORPUS_DAYS as u64),
        ("tail_day_project_groups", TAIL_DAY_PROJECT_GROUPS as u64),
        ("registered_marts", REGISTERED_MARTS as u64),
    ]
}

fn scalar(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0))
        .unwrap_or_else(|error| panic!("fixture scalar query failed: {error}"))
}

fn interaction_timestamp(day: &str, interaction_ordinal: i64, position: i64) -> String {
    assert!(interaction_ordinal >= 0);
    assert!((0..MESSAGES_PER_BLOCK).contains(&position));
    let seconds = interaction_ordinal * MESSAGES_PER_BLOCK + position;
    assert!(
        seconds < 60 * 60,
        "fixture interaction must stay within its day"
    );
    format!("{day}T00:{:02}:{:02}Z", seconds / 60, seconds % 60)
}

fn interaction_prompt(block: i64, session_index: i64) -> String {
    format!("/trace fixture block-{block} session-{session_index}")
}

fn insert_reindex_dimensions(conn: &Connection) {
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("begin reindex dimensions");
    {
        let mut project = conn
            .prepare(
                "INSERT INTO projects \
                 (id, provider, slug, path, display_name, first_seen, last_modified) \
                 VALUES (?, ?, ?, NULL, ?, ?, ?)",
            )
            .expect("prepare reindex projects");
        for index in 0..CORPUS_PROJECTS {
            project
                .execute(rusqlite::params![
                    index + 1,
                    format!("fixture-source-{index}"),
                    format!("fixture-project-{index}"),
                    format!("Fixture {index}"),
                    PREFIX_PROJECT_MTIME,
                    PREFIX_PROJECT_MTIME,
                ])
                .expect("insert reindex project");
        }
    }
    {
        let mut session = conn
            .prepare(
                "INSERT INTO sessions \
                 (id, project_id, session_id) VALUES (?, ?, ?)",
            )
            .expect("prepare reindex sessions");
        for index in 0..CORPUS_SESSIONS {
            session
                .execute(rusqlite::params![
                    index + 1,
                    (index % CORPUS_PROJECTS) + 1,
                    format!("fixture-session-{index}")
                ])
                .expect("insert reindex session");
        }
    }
    conn.execute_batch("COMMIT")
        .expect("commit reindex dimensions");
}

fn update_source_dimensions_for_window(conn: &Connection, start: i64, end: i64) {
    let start_block = start / (CORPUS_SESSIONS * EVENTS_PER_BLOCK);
    let end_block = end / (CORPUS_SESSIONS * EVENTS_PER_BLOCK);
    let first_seq = start_block * MESSAGES_PER_BLOCK;
    let end_seq = end_block * MESSAGES_PER_BLOCK;
    let added_per_session = (end_block - start_block) * MESSAGES_PER_BLOCK;
    let project_mtime = if end == CORPUS_ROWS {
        TAIL_PROJECT_MTIME
    } else {
        PREFIX_PROJECT_MTIME
    };

    let mut bounds = conn
        .prepare(
            "SELECT MIN(timestamp), MAX(timestamp) FROM messages \
             WHERE session_fk=? AND seq>=? AND seq<?",
        )
        .expect("prepare source timestamp bounds");
    let mut update = conn
        .prepare(
            "UPDATE sessions SET message_count = message_count + ?1, \
             last_ts = COALESCE(MAX(COALESCE(last_ts, ''), ?2), last_ts), \
             first_ts = CASE \
               WHEN ?3 = '' THEN first_ts \
               WHEN first_ts IS NULL OR first_ts = '' THEN ?3 \
               ELSE MIN(first_ts, ?3) END \
             WHERE id = ?4",
        )
        .expect("prepare production-shaped source metadata update");
    for session_fk in 1..=CORPUS_SESSIONS {
        let (oldest, newest): (String, String) = bounds
            .query_row(rusqlite::params![session_fk, first_seq, end_seq], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("window has source messages for every session");
        update
            .execute(rusqlite::params![
                added_per_session,
                newest,
                oldest,
                session_fk
            ])
            .expect("update session metadata as the production writer does");
    }
    conn.execute(
        "UPDATE projects SET last_modified=MAX(last_modified, ?)",
        [project_mtime],
    )
    .expect("advance project modification time for the appended source window");
}

fn assert_source_metadata(conn: &Connection, blocks: i64, project_mtime: f64) {
    let messages_per_session = blocks * MESSAGES_PER_BLOCK;
    perf::assert_exact(
        "truthful session message metadata",
        scalar(
            conn,
            &format!(
                "SELECT COUNT(*) FROM sessions s \
                 WHERE s.message_count={messages_per_session} \
                   AND s.first_ts=(SELECT MIN(m.timestamp) FROM messages m \
                                   WHERE m.session_fk=s.id AND m.seq<{messages_per_session}) \
                   AND s.last_ts=(SELECT MAX(m.timestamp) FROM messages m \
                                  WHERE m.session_fk=s.id AND m.seq<{messages_per_session})"
            ),
        ) as u64,
        CORPUS_SESSIONS as u64,
    );
    perf::assert_exact(
        "truthful per-session sequence metadata",
        scalar(
            conn,
            &format!(
                "SELECT COUNT(*) FROM sessions s \
                 WHERE (SELECT COUNT(*) FROM messages m WHERE m.session_fk=s.id \
                        AND m.seq<{messages_per_session})={messages_per_session} \
                   AND (SELECT COUNT(DISTINCT m.seq) FROM messages m WHERE m.session_fk=s.id \
                        AND m.seq<{messages_per_session})={messages_per_session} \
                   AND (SELECT MIN(m.seq) FROM messages m WHERE m.session_fk=s.id)=0 \
                   AND (SELECT MAX(m.seq) FROM messages m WHERE m.session_fk=s.id)={}",
                messages_per_session - 1
            ),
        ) as u64,
        CORPUS_SESSIONS as u64,
    );
    perf::assert_exact(
        "truthful project source metadata",
        scalar(
            conn,
            &format!(
                "SELECT COUNT(*) FROM projects p \
                 WHERE p.first_seen={PREFIX_PROJECT_MTIME} \
                   AND p.last_modified={project_mtime} \
                   AND (SELECT COUNT(*) FROM sessions s WHERE s.project_id=p.id)={}",
                CORPUS_SESSIONS / CORPUS_PROJECTS
            ),
        ) as u64,
        CORPUS_PROJECTS as u64,
    );
    perf::assert_exact(
        "global message id sequence",
        scalar(
            conn,
            "SELECT next_id FROM _messages_id_seq WHERE rowid_kind=1",
        ) as u64,
        scalar(conn, "SELECT COALESCE(MAX(id),0)+1 FROM messages") as u64,
    );
}

fn insert_reindex_window(conn: &Connection, start: i64, end: i64) {
    assert!(start >= 0 && start < end && end <= CORPUS_ROWS);
    assert_eq!(start % (CORPUS_SESSIONS * EVENTS_PER_BLOCK), 0);
    assert_eq!(end % (CORPUS_SESSIONS * EVENTS_PER_BLOCK), 0);
    let routed_partition = stax_etl::ingest::writer::partition_for("2026-01-01T00:00:00Z");
    assert_eq!(routed_partition, "messages_202601");
    stax_etl::ingest::writer::ensure_partition(conn, &routed_partition)
        .expect("create production month partition");
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("begin reindex window");
    {
        let insert_message_sql = format!(
            "INSERT INTO {routed_partition} \
             (id, session_fk, seq, timestamp, role, model, input_tokens, \
              output_tokens, cache_create_tokens, cache_read_tokens, \
              content_text, tools_json, raw_json, is_sidechain, uuid, \
              parent_uuid, speed) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?, ?, 0, ?, NULL, 'standard')"
        );
        let mut message = conn
            .prepare(&insert_message_sql)
            .expect("prepare reindex messages");
        let mut event = conn
            .prepare(
                "INSERT INTO usage_events \
                 (id, source_message_fk, provider, account, project_id, session_id, \
                  ts, day, model, speed, input_tokens, output_tokens, \
                  cache_read_tokens, cache_create_tokens, reasoning_tokens, \
                  cost_usd, cost_source, role, raw_extras) \
                 VALUES (?, ?, ?, 'default', ?, ?, ?, ?, 'fixture-model', \
                         'standard', 10, 5, 0, 0, 0, ?, 'unknown', \
                         'assistant', NULL)",
            )
            .expect("prepare reindex events");

        for index in start..end {
            let event_id = index + 1;
            let session_index = index % CORPUS_SESSIONS;
            let session_fk = session_index + 1;
            let within_session = index / CORPUS_SESSIONS;
            let block = within_session / EVENTS_PER_BLOCK;
            let slot = within_session % EVENTS_PER_BLOCK;
            let project_id = (session_index % CORPUS_PROJECTS) + 1;
            let day = format!("2026-01-{:02}", (session_index % CORPUS_DAYS) + 1);
            // Sessions with the same day/project pair recur every 140 ids.
            // Give each of their ten prompt blocks a non-overlapping 12-record
            // time window so the production timestamp sort cannot interleave
            // two interactions from different sessions.
            let day_project_ordinal = session_index / TAIL_DAY_PROJECT_GROUPS;
            let interaction_ordinal = day_project_ordinal * BLOCKS_PER_SESSION + block;
            let prompt_timestamp = interaction_timestamp(&day, interaction_ordinal, 0);
            let assistant_timestamp = interaction_timestamp(&day, interaction_ordinal, slot + 1);
            let result_timestamp =
                interaction_timestamp(&day, interaction_ordinal, MESSAGES_PER_BLOCK - 1);
            assert!(prompt_timestamp < assistant_timestamp);
            assert!(assistant_timestamp < result_timestamp);
            assert_eq!(
                stax_etl::ingest::writer::partition_for(&assistant_timestamp),
                routed_partition,
                "every dated fixture message must follow production month routing"
            );
            let session_id = format!("fixture-session-{session_index}");
            let provider = format!("fixture-source-{}", project_id - 1);
            let sequence_base = block * (EVENTS_PER_BLOCK + 2);
            let tool_id = format!("fixture-tool-{block}-{session_index}");

            if slot == 0 {
                let prompt_id = CORPUS_ROWS + block * CORPUS_SESSIONS + session_fk;
                let prompt_text = interaction_prompt(block, session_index);
                let prompt_raw = json!({
                    "type": "human",
                    "message": {"role": "user", "content": prompt_text.clone()},
                })
                .to_string();
                message
                    .execute(rusqlite::params![
                        prompt_id,
                        session_fk,
                        sequence_base,
                        prompt_timestamp,
                        "user",
                        Option::<String>::None,
                        0,
                        0,
                        prompt_text,
                        "[]",
                        prompt_raw,
                        format!("fixture-prompt-{block}-{session_index}")
                    ])
                    .expect("insert deterministic prompt");
            }

            // Keep the real prompt as the nearest preceding user turn for all
            // ten assistants in the block. The one deterministic tool result
            // follows the tenth assistant, which also makes it the production
            // message/tool mart's immediate-next-message match.
            let assistant_sequence = sequence_base + slot + 1;
            let (tools_json, assistant_raw) = if slot == EVENTS_PER_BLOCK - 1 {
                (
                    r#"["Read"]"#.to_owned(),
                    json!({
                        "type": "assistant",
                        "message": {
                            "role": "assistant",
                            "content": [{
                                "type": "tool_use",
                                "id": tool_id,
                                "name": "Read",
                                "input": {"file_path": "fixture/input.rs"},
                            }],
                        },
                    })
                    .to_string(),
                )
            } else {
                (
                    "[]".to_owned(),
                    json!({
                        "type": "assistant",
                        "message": {
                            "role": "assistant",
                            "content": [{"type": "text", "text": "fixture"}],
                        },
                    })
                    .to_string(),
                )
            };
            message
                .execute(rusqlite::params![
                    event_id,
                    session_fk,
                    assistant_sequence,
                    assistant_timestamp,
                    "assistant",
                    "fixture-model",
                    10,
                    5,
                    "deterministic trace event",
                    tools_json,
                    assistant_raw,
                    format!("fixture-assistant-{index}")
                ])
                .expect("insert source assistant message");

            if slot == EVENTS_PER_BLOCK - 1 {
                let result_id = CORPUS_ROWS + USER_PROMPTS + block * CORPUS_SESSIONS + session_fk;
                let result_raw = json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": "fixture-result",
                        }],
                    },
                })
                .to_string();
                message
                    .execute(rusqlite::params![
                        result_id,
                        session_fk,
                        sequence_base + EVENTS_PER_BLOCK + 1,
                        result_timestamp,
                        "user",
                        Option::<String>::None,
                        0,
                        0,
                        "",
                        "[]",
                        result_raw,
                        format!("fixture-result-{block}-{session_index}")
                    ])
                    .expect("insert deterministic tool result");
            }

            event
                .execute(rusqlite::params![
                    event_id,
                    event_id,
                    provider,
                    project_id,
                    session_id,
                    assistant_timestamp,
                    day,
                    FIXTURE_COST_USD,
                ])
                .expect("insert reindex event");
        }
    }
    update_source_dimensions_for_window(conn, start, end);
    let next_message_id = scalar(conn, "SELECT COALESCE(MAX(id),0)+1 FROM messages");
    conn.execute(
        "UPDATE _messages_id_seq SET next_id = ? WHERE rowid_kind = 1",
        [next_message_id],
    )
    .expect("advance message sequence");
    conn.execute_batch("COMMIT").expect("commit reindex window");
}

fn assert_source_distribution(conn: &Connection, events: i64, prompt_blocks: i64) {
    let prompts = CORPUS_SESSIONS * prompt_blocks;
    let tool_results = prompts;
    let message_rows = events + prompts + tool_results;
    perf::assert_exact("reindex events", count(conn, "usage_events"), events as u64);
    perf::assert_exact(
        "reindex message rows",
        count(conn, "messages"),
        message_rows as u64,
    );
    perf::assert_exact(
        "dated messages in the production month partition",
        count(conn, "messages_202601"),
        message_rows as u64,
    );
    perf::assert_exact(
        "unknown message partition remains empty",
        count(conn, "messages_unknown"),
        0,
    );
    perf::assert_exact(
        "assistant source messages",
        scalar(conn, "SELECT COUNT(*) FROM messages WHERE role='assistant'") as u64,
        events as u64,
    );
    perf::assert_exact(
        "deterministic user prompts",
        scalar(
            conn,
            "SELECT COUNT(*) FROM messages WHERE raw_json LIKE '%\"type\":\"human\"%'",
        ) as u64,
        prompts as u64,
    );
    perf::assert_exact(
        "unique production interaction keys",
        scalar(
            conn,
            "SELECT COUNT(DISTINCT timestamp || '|' || substr(content_text,1,64)) \
             FROM messages WHERE raw_json LIKE '%\"type\":\"human\"%'",
        ) as u64,
        prompts as u64,
    );
    perf::assert_exact(
        "deterministic tool results",
        scalar(
            conn,
            "SELECT COUNT(*) FROM messages WHERE raw_json LIKE '%\"type\":\"tool_result\"%'",
        ) as u64,
        tool_results as u64,
    );
    perf::assert_exact(
        "deterministic tool-bearing sources",
        scalar(
            conn,
            "SELECT COUNT(*) FROM messages WHERE tools_json <> '[]'",
        ) as u64,
        prompts as u64,
    );
    perf::assert_exact(
        "unique one-to-one in-corpus source links",
        scalar(
            conn,
            "SELECT COUNT(*) FROM usage_events e \
             JOIN messages m ON m.id=e.source_message_fk \
             WHERE m.role='assistant' AND e.id=m.id",
        ) as u64,
        events as u64,
    );
    perf::assert_exact(
        "distinct source links",
        scalar(
            conn,
            "SELECT COUNT(DISTINCT source_message_fk) FROM usage_events",
        ) as u64,
        events as u64,
    );
    perf::assert_exact(
        "assistant links anchored to their block prompt",
        scalar(
            conn,
            "SELECT COUNT(*) FROM usage_events e \
             JOIN messages m ON m.id=e.source_message_fk \
             JOIN messages p ON p.session_fk=m.session_fk \
                            AND p.seq=m.seq-(m.seq % 12) \
             WHERE p.role='user' AND p.raw_json LIKE '%\"type\":\"human\"%'",
        ) as u64,
        events as u64,
    );
    perf::assert_exact(
        "tool-bearing assistants followed by tool results",
        scalar(
            conn,
            "SELECT COUNT(*) FROM usage_events e \
             JOIN messages m ON m.id=e.source_message_fk \
             JOIN messages r ON r.session_fk=m.session_fk AND r.seq=m.seq+1 \
             WHERE m.tools_json <> '[]' \
               AND r.raw_json LIKE '%\"type\":\"tool_result\"%'",
        ) as u64,
        prompts as u64,
    );
    perf::assert_exact(
        "strictly ordered assistant steps",
        scalar(
            conn,
            "SELECT COUNT(*) FROM messages a \
             JOIN messages previous ON previous.session_fk=a.session_fk \
                                   AND previous.seq=a.seq-1 \
             WHERE a.role='assistant' \
               AND substr(previous.timestamp,1,10)=substr(a.timestamp,1,10) \
               AND previous.timestamp<a.timestamp",
        ) as u64,
        events as u64,
    );
    perf::assert_exact(
        "strictly ordered tool results",
        scalar(
            conn,
            "SELECT COUNT(*) FROM messages r \
             JOIN messages previous ON previous.session_fk=r.session_fk \
                                   AND previous.seq=r.seq-1 \
             WHERE r.raw_json LIKE '%\"type\":\"tool_result\"%' \
               AND previous.role='assistant' \
               AND substr(previous.timestamp,1,10)=substr(r.timestamp,1,10) \
               AND previous.timestamp<r.timestamp",
        ) as u64,
        prompts as u64,
    );
    perf::assert_exact(
        "unique per-session ordered timestamps",
        scalar(
            conn,
            "SELECT COUNT(*) FROM sessions s WHERE \
             (SELECT COUNT(*) FROM messages m WHERE m.session_fk=s.id)= \
             (SELECT COUNT(DISTINCT m.timestamp) FROM messages m WHERE m.session_fk=s.id)",
        ) as u64,
        CORPUS_SESSIONS as u64,
    );
    perf::assert_exact(
        "reindex sessions",
        count(conn, "sessions"),
        CORPUS_SESSIONS as u64,
    );
    perf::assert_exact(
        "reindex projects",
        count(conn, "projects"),
        CORPUS_PROJECTS as u64,
    );
    perf::assert_exact(
        "reindex day spread",
        scalar(conn, "SELECT COUNT(DISTINCT day) FROM usage_events") as u64,
        CORPUS_DAYS as u64,
    );
    perf::assert_exact(
        "registered mart builders",
        stax_etl::marts::all().len() as u64,
        REGISTERED_MARTS as u64,
    );
    let project_mtime = if prompt_blocks == BLOCKS_PER_SESSION {
        TAIL_PROJECT_MTIME
    } else {
        PREFIX_PROJECT_MTIME
    };
    assert_source_metadata(conn, prompt_blocks, project_mtime);
}

fn assert_tail_distribution(conn: &Connection) {
    perf::assert_exact(
        "tail rows",
        scalar(conn, "SELECT COUNT(*) FROM usage_events WHERE id > 90000") as u64,
        CHANGED_ROWS as u64,
    );
    perf::assert_exact(
        "tail sessions",
        scalar(
            conn,
            "SELECT COUNT(DISTINCT session_id) FROM usage_events WHERE id > 90000",
        ) as u64,
        CORPUS_SESSIONS as u64,
    );
    perf::assert_exact(
        "tail projects",
        scalar(
            conn,
            "SELECT COUNT(DISTINCT project_id) FROM usage_events WHERE id > 90000",
        ) as u64,
        CORPUS_PROJECTS as u64,
    );
    perf::assert_exact(
        "tail days",
        scalar(
            conn,
            "SELECT COUNT(DISTINCT day) FROM usage_events WHERE id > 90000",
        ) as u64,
        CORPUS_DAYS as u64,
    );
    perf::assert_exact(
        "tail day/project groups",
        scalar(
            conn,
            "SELECT COUNT(*) FROM (SELECT day, project_id FROM usage_events \
             WHERE id > 90000 GROUP BY day, project_id)",
        ) as u64,
        TAIL_DAY_PROJECT_GROUPS as u64,
    );
}

fn assert_watermarks(conn: &Connection, expected: i64) {
    perf::assert_exact(
        "mart watermark rows",
        count(conn, "mart_watermark"),
        REGISTERED_MARTS as u64,
    );
    for mart in stax_etl::marts::all() {
        let actual: i64 = conn
            .query_row(
                "SELECT last_event_id FROM mart_watermark WHERE mart_name=?",
                [mart.name()],
                |row| row.get(0),
            )
            .expect("registered mart watermark");
        assert_eq!(actual, expected, "{} watermark", mart.name());
    }
}

fn assert_report(report: &[(String, i64)], expected_rows: i64) {
    perf::assert_exact(
        "mart report entries",
        report.len() as u64,
        REGISTERED_MARTS as u64,
    );
    let actual: BTreeSet<&str> = report.iter().map(|(name, _)| name.as_str()).collect();
    let expected: BTreeSet<&str> = stax_etl::marts::all()
        .iter()
        .map(|mart| mart.name())
        .collect();
    assert_eq!(actual, expected, "mart report names");
    assert!(
        report.iter().all(|(_, rows)| *rows == expected_rows),
        "every registered mart must report the fixed event window"
    );
}

#[derive(Debug, Clone, PartialEq)]
struct TableSnapshot {
    columns: Vec<String>,
    rows: Vec<Vec<SqlValue>>,
}

#[derive(Debug, Clone, PartialEq)]
struct MartSnapshot {
    tables: BTreeMap<String, TableSnapshot>,
}

fn snapshot_query(conn: &Connection, label: &str, sql: &str) -> (String, TableSnapshot) {
    let mut statement = conn
        .prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {label} snapshot: {error}"));
    let columns = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let column_count = statement.column_count();
    let rows = statement
        .query_map([], |row| {
            (0..column_count)
                .map(|index| row.get::<_, SqlValue>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_else(|error| panic!("query {label} snapshot: {error}"))
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap_or_else(|error| panic!("read {label} snapshot: {error}"));
    (label.to_owned(), TableSnapshot { columns, rows })
}

impl MartSnapshot {
    fn capture(conn: &Connection) -> Self {
        // `mart_watermark.last_refresh_ts` is the sole excluded volatile value.
        // Every semantic column from every registered output (plus the command
        // builder's command-day output) is captured in deterministic key order.
        let queries = [
            (
                "daily_mart",
                "SELECT * FROM daily_mart \
                 ORDER BY day, project_id, provider, model, speed",
            ),
            (
                "session_mart",
                "SELECT * FROM session_mart ORDER BY session_id",
            ),
            (
                "project_mart",
                "SELECT * FROM project_mart ORDER BY project_id",
            ),
            (
                "provider_day_mart",
                "SELECT * FROM provider_day_mart ORDER BY day, provider",
            ),
            (
                "model_day_mart",
                "SELECT * FROM model_day_mart ORDER BY day, model, speed",
            ),
            (
                "tool_mart",
                "SELECT * FROM tool_mart \
                 ORDER BY day, project_id, provider, tool_name",
            ),
            (
                "command_mart",
                "SELECT * FROM command_mart \
                 ORDER BY day, project_id, command_name",
            ),
            (
                "message_tool_mart",
                "SELECT * FROM message_tool_mart \
                 ORDER BY message_id, tool_name, call_index, id",
            ),
            (
                "command_day_mart",
                "SELECT * FROM command_day_mart ORDER BY day, project_id",
            ),
            (
                "mart_watermark",
                "SELECT mart_name, last_event_id FROM mart_watermark ORDER BY mart_name",
            ),
        ];
        Self {
            tables: queries
                .into_iter()
                .map(|(label, sql)| snapshot_query(conn, label, sql))
                .collect(),
        }
    }

    fn expected(conn: &Connection, events: i64, prompt_blocks: i64) -> Self {
        let seq_cap = prompt_blocks * MESSAGES_PER_BLOCK;
        let project = format!(
            "WITH event_rollup AS ( \
               SELECT e.project_id AS project_id, MIN(e.ts) AS first_ts, MAX(e.ts) AS last_ts, \
                      COUNT(*) AS total_messages, COUNT(DISTINCT e.session_id) AS total_sessions, \
                      SUM(e.input_tokens) AS total_input_tokens, \
                      SUM(e.output_tokens) AS total_output_tokens, \
                      SUM(e.cache_read_tokens) AS total_cache_read, \
                      SUM(e.cache_create_tokens) AS total_cache_create, \
                      SUM(e.cost_usd) AS total_cost_usd \
                 FROM usage_events e WHERE e.id<={events} GROUP BY e.project_id \
             ), message_dims AS ( \
               SELECT s.project_id AS project_id, \
                      SUM(CASE WHEN m.role='user' THEN 1 ELSE 0 END) \
                        AS total_user_messages, \
                      SUM(CASE WHEN m.role='assistant' THEN 1 ELSE 0 END) \
                        AS total_assistant_messages, \
                      SUM(CASE WHEN m.tools_json<>'[]' THEN 1 ELSE 0 END) \
                        AS total_tool_use_messages, \
                      SUM(CASE WHEN m.raw_json LIKE '%\"type\":\"tool_result\"%' THEN 1 ELSE 0 END) \
                        AS total_tool_result_messages, \
                      SUM(CASE WHEN m.raw_json LIKE '%\"type\":\"human\"%' THEN 1 ELSE 0 END) \
                        AS total_commands, \
                      COUNT(*) AS total_records \
                 FROM messages m JOIN sessions s ON s.id=m.session_fk \
                WHERE m.seq<{seq_cap} GROUP BY s.project_id \
             ) \
             SELECT e.project_id AS project_id, p.provider AS provider, p.slug AS slug, \
                    p.display_name AS display_name, e.first_ts AS first_ts, e.last_ts AS last_ts, \
                    e.total_messages AS total_messages, e.total_sessions AS total_sessions, \
                    e.total_input_tokens AS total_input_tokens, \
                    e.total_output_tokens AS total_output_tokens, \
                    e.total_cache_read AS total_cache_read, \
                    e.total_cache_create AS total_cache_create, \
                    e.total_cost_usd AS total_cost_usd, \
                    d.total_user_messages AS total_user_messages, \
                    d.total_assistant_messages AS total_assistant_messages, \
                    d.total_tool_use_messages AS total_tool_use_messages, \
                    d.total_tool_result_messages AS total_tool_result_messages, \
                    d.total_commands AS total_commands, d.total_records AS total_records, \
                    CAST(0 AS INTEGER) AS total_errors, '{{}}' AS errors_by_category, \
                    CAST(0 AS INTEGER) AS total_cache_read_messages, \
                    CAST(0 AS INTEGER) AS total_commands_followed_by_interruption, \
                    d.total_tool_use_messages AS total_command_tools, \
                    d.total_assistant_messages AS total_command_steps \
               FROM event_rollup e JOIN projects p ON p.id=e.project_id \
               JOIN message_dims d ON d.project_id=e.project_id ORDER BY e.project_id"
        );
        let queries = vec![
            (
                "daily_mart",
                format!(
                    "SELECT day AS day, project_id AS project_id, provider AS provider, \
                            model AS model, speed AS speed, SUM(input_tokens) AS input_tokens, \
                            SUM(output_tokens) AS output_tokens, \
                            SUM(cache_read_tokens) AS cache_read, \
                            SUM(cache_create_tokens) AS cache_create, \
                            COUNT(*) AS message_count, COUNT(DISTINCT session_id) AS session_count, \
                            SUM(cost_usd) AS cost_usd \
                       FROM usage_events WHERE id<={events} \
                      GROUP BY day, project_id, provider, model, speed \
                      ORDER BY day, project_id, provider, model, speed"
                ),
            ),
            (
                "session_mart",
                format!(
                    "SELECT e.session_id AS session_id, MIN(e.project_id) AS project_id, \
                            MIN(e.provider) AS provider, 'fixture-model' AS primary_model, \
                            MIN(e.ts) AS first_ts, MAX(e.ts) AS last_ts, \
                            COUNT(*) AS message_count, \
                            SUM(CASE WHEN e.role='user' THEN 1 ELSE 0 END) AS user_message_count, \
                            SUM(CASE WHEN e.role='assistant' THEN 1 ELSE 0 END) \
                              AS assistant_message_count, \
                            SUM(e.input_tokens) AS input_tokens, \
                            SUM(e.output_tokens) AS output_tokens, \
                            SUM(e.cache_read_tokens) AS cache_read, \
                            SUM(e.cache_create_tokens) AS cache_create, \
                            SUM(e.cost_usd) AS cost_usd, \
                            CASE WHEN SUM(CASE WHEN e.role='user' THEN 1 ELSE 0 END)=1 \
                                       AND SUM(CASE WHEN e.role='assistant' THEN 1 ELSE 0 END)=1 \
                                 THEN 1 ELSE 0 END AS is_one_shot, NULL AS cwd \
                       FROM usage_events e WHERE e.id<={events} GROUP BY e.session_id \
                      ORDER BY e.session_id"
                ),
            ),
            ("project_mart", project),
            (
                "provider_day_mart",
                format!(
                    "SELECT day AS day, provider AS provider, SUM(cost_usd) AS cost_usd, \
                            COUNT(*) AS message_count, \
                            COUNT(DISTINCT session_id) AS session_count, \
                            COUNT(DISTINCT project_id) AS project_count \
                       FROM usage_events WHERE id<={events} GROUP BY day, provider \
                      ORDER BY day, provider"
                ),
            ),
            (
                "model_day_mart",
                format!(
                    "SELECT day AS day, model AS model, speed AS speed, \
                            SUM(cost_usd) AS cost_usd, SUM(input_tokens) AS input_tokens, \
                            SUM(output_tokens) AS output_tokens, \
                            SUM(cache_read_tokens) AS cache_read, \
                            SUM(cache_create_tokens) AS cache_create, \
                            COUNT(*) AS message_count, \
                            COUNT(DISTINCT session_id) AS session_count \
                       FROM usage_events WHERE id<={events} GROUP BY day, model, speed \
                      ORDER BY day, model, speed"
                ),
            ),
            (
                "tool_mart",
                format!(
                    "SELECT e.day AS day, e.project_id AS project_id, e.provider AS provider, \
                            'Read' AS tool_name, COUNT(*) AS event_count, \
                            SUM(e.cost_usd) AS cost_usd, SUM(e.input_tokens) AS tokens_in, \
                            SUM(e.output_tokens) AS tokens_out, \
                            COUNT(DISTINCT e.session_id) AS session_count, \
                            COUNT(*) AS calls_total, \
                            SUM(e.cache_read_tokens) AS cache_read, \
                            SUM(e.cache_create_tokens) AS cache_create \
                       FROM usage_events e JOIN messages m ON m.id=e.source_message_fk \
                      WHERE e.id<={events} AND m.tools_json<>'[]' \
                      GROUP BY e.day, e.project_id, e.provider \
                      ORDER BY e.day, e.project_id, e.provider, tool_name"
                ),
            ),
            (
                "command_mart",
                format!(
                    "SELECT e.day AS day, e.project_id AS project_id, '/trace' AS command_name, \
                            COUNT(*) AS event_count, SUM(e.cost_usd) AS cost_usd, \
                            SUM(e.input_tokens) AS tokens_in, \
                            SUM(e.output_tokens) AS tokens_out, \
                            COUNT(DISTINCT e.session_id) AS session_count \
                       FROM usage_events e WHERE e.id<={events} \
                      GROUP BY e.day, e.project_id \
                      ORDER BY e.day, e.project_id, command_name"
                ),
            ),
            (
                "message_tool_mart",
                format!(
                    "SELECT ROW_NUMBER() OVER (ORDER BY e.id) AS id, \
                            e.source_message_fk AS message_id, e.project_id AS project_id, \
                            e.session_id AS session_id, e.ts AS ts, e.day AS day, \
                            'Read' AS tool_name, 'fixture/input.rs' AS file_path, \
                            CAST(14 AS INTEGER) AS byte_count, CAST(0 AS INTEGER) AS call_index \
                       FROM usage_events e JOIN messages m ON m.id=e.source_message_fk \
                      WHERE e.id<={events} AND m.tools_json<>'[]' ORDER BY e.id"
                ),
            ),
            (
                "command_day_mart",
                format!(
                    "SELECT substr(m.timestamp,1,10) AS day, s.project_id AS project_id, \
                            COUNT(*) AS command_count \
                       FROM messages m JOIN sessions s ON s.id=m.session_fk \
                      WHERE m.seq<{seq_cap} AND m.raw_json LIKE '%\"type\":\"human\"%' \
                      GROUP BY substr(m.timestamp,1,10), s.project_id \
                      ORDER BY day, project_id"
                ),
            ),
            (
                "mart_watermark",
                format!(
                    "WITH names(mart_name) AS (VALUES ('daily'), ('session'), ('project'), \
                     ('provider_day'), ('model_day'), ('tool'), ('command'), ('message_tool')) \
                     SELECT mart_name AS mart_name, CAST({events} AS INTEGER) AS last_event_id \
                     FROM names ORDER BY mart_name"
                ),
            ),
        ];
        Self {
            tables: queries
                .into_iter()
                .map(|(label, sql)| snapshot_query(conn, label, &sql))
                .collect(),
        }
    }
}

fn compare_snapshot_direction(left: &MartSnapshot, right: &MartSnapshot, label: &str) {
    assert_eq!(
        left.tables.len(),
        right.tables.len(),
        "{label}: snapshot table count"
    );
    for (table, left_table) in &left.tables {
        let right_table = right
            .tables
            .get(table)
            .unwrap_or_else(|| panic!("{label}: missing snapshot table {table}"));
        assert_eq!(
            left_table.columns, right_table.columns,
            "{label}: {table} columns"
        );
        assert_eq!(
            left_table.rows.len(),
            right_table.rows.len(),
            "{label}: {table} row count"
        );
        if let Some((index, (left_row, right_row))) = left_table
            .rows
            .iter()
            .zip(&right_table.rows)
            .enumerate()
            .find(|(_, (left_row, right_row))| left_row != right_row)
        {
            panic!("{label}: {table} row {index} differs: left={left_row:?}, right={right_row:?}");
        }
    }
}

fn assert_snapshots_equivalent(left: &MartSnapshot, right: &MartSnapshot, label: &str) {
    compare_snapshot_direction(left, right, label);
    compare_snapshot_direction(right, left, label);
}

fn snapshots_equivalent(left: &MartSnapshot, right: &MartSnapshot) -> bool {
    left == right
}

#[test]
fn all_column_snapshot_rejects_key_and_semantic_mutations_with_stable_totals() {
    let scratch = Scratch::new("snapshot-negative");
    let conn = fresh_store(&scratch.path().join("store.db"));
    conn.execute_batch(
        "INSERT INTO daily_mart \
           (day, project_id, provider, model, speed, message_count, session_count) VALUES \
           ('2026-01-01', 1, 'fixture', 'model', 'standard', 10, 2), \
           ('2026-01-02', 1, 'fixture', 'model', 'standard', 20, 3);",
    )
    .expect("seed cheap snapshot mutation fixture");
    let baseline = MartSnapshot::capture(&conn);
    let totals = (
        scalar(&conn, "SELECT COUNT(*) FROM daily_mart"),
        scalar(
            &conn,
            "SELECT COALESCE(SUM(message_count),0) FROM daily_mart",
        ),
        scalar(
            &conn,
            "SELECT COALESCE(SUM(session_count),0) FROM daily_mart",
        ),
    );

    conn.execute(
        "UPDATE daily_mart SET day='2026-01-03' WHERE day='2026-01-01'",
        [],
    )
    .expect("mutate previously unchecked key");
    assert_eq!(
        (
            scalar(&conn, "SELECT COUNT(*) FROM daily_mart"),
            scalar(
                &conn,
                "SELECT COALESCE(SUM(message_count),0) FROM daily_mart"
            ),
            scalar(
                &conn,
                "SELECT COALESCE(SUM(session_count),0) FROM daily_mart"
            ),
        ),
        totals,
        "key mutation preserves old aggregate-only totals"
    );
    assert!(
        !snapshots_equivalent(&baseline, &MartSnapshot::capture(&conn)),
        "ordered snapshot must reject a key mutation"
    );

    conn.execute(
        "UPDATE daily_mart SET day='2026-01-01' WHERE day='2026-01-03'",
        [],
    )
    .expect("restore key");
    conn.execute_batch(
        "UPDATE daily_mart SET session_count=session_count+1 WHERE day='2026-01-01'; \
         UPDATE daily_mart SET session_count=session_count-1 WHERE day='2026-01-02';",
    )
    .expect("redistribute previously unchecked semantic column");
    assert_eq!(
        (
            scalar(&conn, "SELECT COUNT(*) FROM daily_mart"),
            scalar(
                &conn,
                "SELECT COALESCE(SUM(message_count),0) FROM daily_mart"
            ),
            scalar(
                &conn,
                "SELECT COALESCE(SUM(session_count),0) FROM daily_mart"
            ),
        ),
        totals,
        "semantic mutation preserves every old aggregate-only total"
    );
    assert!(
        !snapshots_equivalent(&baseline, &MartSnapshot::capture(&conn)),
        "all-column snapshot must reject redistributed session_count"
    );
}

#[test]
fn repeated_multi_session_prompts_keep_exact_command_totals() {
    use stax_etl::marts::dims::{DimRow, count_dims};
    use stax_etl::stats::classifier::{RawEntry, tag};
    use stax_etl::stats::enricher::{build_detailed, interaction_key};

    #[derive(Clone)]
    struct FixtureRow {
        storage_id: i64,
        session_id: String,
        timestamp: String,
        payload: serde_json::Value,
    }

    let day = "2026-01-01";
    let mut fixture_rows = Vec::new();
    let mut logical_row = 0_i64;
    let mut expected = BTreeMap::new();
    for session_index in [0_i64, TAIL_DAY_PROJECT_GROUPS] {
        let session_id = format!("fixture-session-{session_index}");
        let day_project_ordinal = session_index / TAIL_DAY_PROJECT_GROUPS;
        for block in 0..2 {
            let ordinal = day_project_ordinal * BLOCKS_PER_SESSION + block;
            let prompt = interaction_prompt(block, session_index);
            let prompt_timestamp = interaction_timestamp(day, ordinal, 0);
            let interaction_key =
                format!("{}|{}", prompt_timestamp, &prompt[..prompt.len().min(64)]);
            let storage_id = 1_000 + (logical_row * 37) % 53;
            logical_row += 1;
            fixture_rows.push(FixtureRow {
                storage_id,
                session_id: session_id.clone(),
                timestamp: prompt_timestamp.clone(),
                payload: json!({
                    "uuid": format!("fixture-row-{storage_id}"),
                    "timestamp": prompt_timestamp,
                    "type": "human",
                    "message": {"role": "user", "content": prompt},
                }),
            });

            let tool_id = format!("fixture-tool-{block}-{session_index}");
            let mut response_timestamps = Vec::new();
            for response_index in 0..EVENTS_PER_BLOCK {
                let timestamp = interaction_timestamp(day, ordinal, response_index + 1);
                response_timestamps.push(timestamp.clone());
                let mut content = vec![json!({
                    "type": "text",
                    "text": format!("fixture response {response_index}"),
                })];
                if response_index == EVENTS_PER_BLOCK - 1 {
                    content.push(json!({
                        "type": "tool_use",
                        "id": tool_id,
                        "name": "Read",
                        "input": {"file_path": "fixture/input.rs"},
                    }));
                }
                let storage_id = 1_000 + (logical_row * 37) % 53;
                logical_row += 1;
                fixture_rows.push(FixtureRow {
                    storage_id,
                    session_id: session_id.clone(),
                    timestamp: timestamp.clone(),
                    payload: json!({
                        "uuid": format!("fixture-row-{storage_id}"),
                        "timestamp": timestamp,
                        "fixture_response_index": response_index,
                        "type": "assistant",
                        "message": {
                            "role": "assistant",
                            "model": "fixture-model",
                            "content": content,
                        },
                    }),
                });
            }

            let result_timestamp = interaction_timestamp(day, ordinal, MESSAGES_PER_BLOCK - 1);
            let storage_id = 1_000 + (logical_row * 37) % 53;
            logical_row += 1;
            fixture_rows.push(FixtureRow {
                storage_id,
                session_id: session_id.clone(),
                timestamp: result_timestamp.clone(),
                payload: json!({
                    "uuid": format!("fixture-row-{storage_id}"),
                    "timestamp": result_timestamp,
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": "fixture-result",
                        }],
                    },
                }),
            });
            assert!(
                expected
                    .insert(
                        interaction_key,
                        (
                            session_id.clone(),
                            response_timestamps,
                            result_timestamp,
                            tool_id,
                        ),
                    )
                    .is_none()
            );
        }
    }

    assert_eq!(logical_row, 48);
    assert_eq!(
        fixture_rows
            .iter()
            .map(|row| row.storage_id)
            .collect::<BTreeSet<_>>()
            .len(),
        48,
        "fixture storage ids must remain unique"
    );
    // IDs deliberately disagree with logical time, as can happen after
    // ingestion. Production reconstruction must sort by authoritative
    // timestamps rather than depending on insertion or row-id order.
    fixture_rows.sort_by_key(|row| row.storage_id);
    assert!(
        fixture_rows
            .windows(2)
            .any(|pair| pair[0].timestamp > pair[1].timestamp),
        "storage-id order must be observably shuffled"
    );

    let rows: Vec<DimRow> = fixture_rows
        .iter()
        .map(|row| DimRow {
            raw_json: Some(row.payload.to_string()),
            session_id: Some(row.session_id.clone()),
            timestamp: Some(row.timestamp.clone()),
            provider: Some(FIXTURE_PROVIDER.to_owned()),
        })
        .collect();
    let (messages, interactions) = count_dims(&rows);
    let dataset = build_detailed(tag(fixture_rows
        .iter()
        .map(|row| RawEntry {
            payload: row.payload.clone(),
            session_id: row.session_id.clone(),
            provider: FIXTURE_PROVIDER.to_owned(),
        })
        .collect()));

    assert_eq!(expected.len(), 4);
    assert_eq!(dataset.records.len(), 48);
    assert_eq!(dataset.interactions.len(), 4);
    let mut reconstructed = BTreeSet::new();
    for interaction in &dataset.interactions {
        let (session_id, response_timestamps, result_timestamp, tool_id) = expected
            .get(&interaction.key)
            .expect("every reconstructed interaction must match one expected prompt");
        assert!(reconstructed.insert(interaction.key.clone()));
        assert_eq!(&interaction.session_id, session_id);

        let command = &dataset.records[interaction.command];
        assert_eq!(&command.session_id, session_id);
        assert!(command.content.starts_with("/trace "));
        assert_eq!(interaction_key(command), interaction.key);

        assert_eq!(interaction.responses.len(), EVENTS_PER_BLOCK as usize);
        let response_indices: Vec<u64> = interaction
            .responses
            .iter()
            .map(|&record_index| {
                let response = &dataset.records[record_index];
                assert_eq!(&response.session_id, session_id);
                response.raw_data["fixture_response_index"]
                    .as_u64()
                    .expect("response fixture index")
            })
            .collect();
        assert_eq!(response_indices, (0..10).collect::<Vec<_>>());
        assert_eq!(
            interaction
                .responses
                .iter()
                .map(|&record_index| dataset.records[record_index].timestamp.clone())
                .collect::<Vec<_>>(),
            *response_timestamps
        );

        assert_eq!(interaction.tool_results.len(), 1);
        let result = &dataset.records[interaction.tool_results[0]];
        assert_eq!(&result.session_id, session_id);
        assert_eq!(&result.timestamp, result_timestamp);
        assert!(result.has_tool_result);
        assert_eq!(
            result.raw_data["message"]["content"][0]["tool_use_id"].as_str(),
            Some(tool_id.as_str())
        );

        assert_eq!(interaction.assistant_steps, EVENTS_PER_BLOCK as usize);
        assert_eq!(interaction.tool_count, 1);
        assert_eq!(interaction.tools_used.len(), 1);
        assert_eq!(
            interaction.tools_used[0]
                .block
                .as_ref()
                .expect("detailed build retains the tool block")
                .id
                .as_str(),
            Some(tool_id.as_str())
        );
    }
    assert_eq!(reconstructed, expected.keys().cloned().collect());

    // These are every aggregate emitted by count_dims, not just the command
    // count that originally exposed the collision.
    assert_eq!(messages.commands, 4);
    assert_eq!(messages.user, 8);
    assert_eq!(messages.assistant, 40);
    assert_eq!(messages.tool_use, 4);
    assert_eq!(messages.tool_result, 4);
    assert_eq!(interactions.records, 48);
    assert_eq!(interactions.cache_read_messages, 0);
    assert_eq!(interactions.errors_total, 0);
    assert_eq!(interactions.errors_by_category_json, "{}");
    assert_eq!(interactions.commands_followed_by_interruption, 0);
    assert_eq!(interactions.command_steps, 40);
    assert_eq!(interactions.command_tools, 4);
}

#[test]
fn source_derived_oracle_matches_every_registered_output_on_a_small_fixture() {
    let scratch = Scratch::new("snapshot-positive");
    let conn = fresh_store(&scratch.path().join("store.db"));
    conn.execute(
        "INSERT INTO projects \
         (id, provider, slug, path, display_name, first_seen, last_modified) \
         VALUES (1, 'fixture-source-0', 'fixture-project-0', NULL, 'Fixture 0', 1, 1)",
        [],
    )
    .expect("insert oracle project");
    conn.execute(
        "INSERT INTO sessions \
         (id, project_id, session_id, first_ts, last_ts, message_count) \
         VALUES (1, 1, 'fixture-session-0', \
                 '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z', 3)",
        [],
    )
    .expect("insert oracle session");
    let partition = stax_etl::ingest::writer::partition_for("2026-01-01T00:00:00Z");
    stax_etl::ingest::writer::ensure_partition(&conn, &partition)
        .expect("ensure oracle message partition");
    let insert = format!(
        "INSERT INTO {partition} \
         (id, session_fk, seq, timestamp, role, model, input_tokens, output_tokens, \
          cache_create_tokens, cache_read_tokens, content_text, tools_json, raw_json, \
          is_sidechain, uuid, parent_uuid, speed) \
         VALUES (?, 1, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?, ?, 0, ?, NULL, 'standard')"
    );
    let prompt_raw = json!({
        "type": "human",
        "message": {"role": "user", "content": "/trace fixture"},
    })
    .to_string();
    conn.execute(
        &insert,
        rusqlite::params![
            100_001,
            0,
            "2026-01-01T00:00:00Z",
            "user",
            Option::<String>::None,
            0,
            0,
            "/trace fixture",
            "[]",
            prompt_raw,
            "fixture-prompt-small"
        ],
    )
    .expect("insert oracle prompt");
    let assistant_raw = json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "fixture-tool-small",
                "name": "Read",
                "input": {"file_path": "fixture/input.rs"},
            }],
        },
    })
    .to_string();
    conn.execute(
        &insert,
        rusqlite::params![
            1,
            1,
            "2026-01-01T00:01:00Z",
            "assistant",
            "fixture-model",
            10,
            5,
            "deterministic trace event",
            r#"["Read"]"#,
            assistant_raw,
            "fixture-assistant-small"
        ],
    )
    .expect("insert oracle assistant");
    let result_raw = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "fixture-tool-small",
                "content": "fixture-result",
            }],
        },
    })
    .to_string();
    conn.execute(
        &insert,
        rusqlite::params![
            110_001,
            2,
            "2026-01-01T00:01:00Z",
            "user",
            Option::<String>::None,
            0,
            0,
            "",
            "[]",
            result_raw,
            "fixture-result-small"
        ],
    )
    .expect("insert oracle tool result");
    conn.execute(
        "INSERT INTO usage_events \
         (id, source_message_fk, provider, account, project_id, session_id, ts, day, \
          model, speed, input_tokens, output_tokens, cache_read_tokens, \
          cache_create_tokens, reasoning_tokens, cost_usd, cost_source, role, raw_extras) \
         VALUES (1, 1, 'fixture-source-0', 'default', 1, 'fixture-session-0', \
                 '2026-01-01T00:01:00Z', '2026-01-01', 'fixture-model', 'standard', \
                 10, 5, 0, 0, 0, ?, 'unknown', 'assistant', NULL)",
        [FIXTURE_COST_USD],
    )
    .expect("insert oracle usage event");
    stax_etl::marts::watermark::refresh_all_marts(&conn, "fixture-refresh")
        .expect("refresh small oracle marts");

    let actual = MartSnapshot::capture(&conn);
    let expected = MartSnapshot::expected(&conn, 1, 1);
    assert_snapshots_equivalent(&actual, &expected, "small source-derived all-column oracle");
    stax_etl::marts::watermark::rebuild_all_marts(&conn, "fixture-rebuild")
        .expect("rebuild small oracle marts");
    let rebuilt = MartSnapshot::capture(&conn);
    assert_snapshots_equivalent(
        &actual,
        &rebuilt,
        "incremental and full paths must agree bidirectionally",
    );
}

fn assert_mart_semantics(conn: &Connection, events: i64, prompt_blocks: i64) -> MartSnapshot {
    let prompts = prompt_blocks * CORPUS_SESSIONS;
    let tool_results = prompts;
    let tool_events = prompts;
    let assert_pair = |label: &str, sql: &str, expected_rows: i64, expected_total: i64| {
        let (rows, total): (i64, i64) = conn
            .query_row(sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap_or_else(|error| panic!("{label} semantic query: {error}"));
        assert_eq!(rows, expected_rows, "{label} row cardinality");
        assert_eq!(total, expected_total, "{label} semantic total");
    };
    assert_pair(
        "daily mart",
        "SELECT COUNT(*), COALESCE(SUM(message_count),0) FROM daily_mart",
        TAIL_DAY_PROJECT_GROUPS,
        events,
    );
    assert_pair(
        "session mart",
        "SELECT COUNT(*), COALESCE(SUM(message_count),0) FROM session_mart",
        CORPUS_SESSIONS,
        events,
    );
    assert_pair(
        "project mart",
        "SELECT COUNT(*), COALESCE(SUM(total_messages),0) FROM project_mart",
        CORPUS_PROJECTS,
        events,
    );
    assert_pair(
        "provider mart",
        "SELECT COUNT(*), COALESCE(SUM(message_count),0) FROM provider_day_mart",
        TAIL_DAY_PROJECT_GROUPS,
        events,
    );
    assert_pair(
        "model mart",
        "SELECT COUNT(*), COALESCE(SUM(message_count),0) FROM model_day_mart",
        CORPUS_DAYS,
        events,
    );

    let project_dims: (i64, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(total_sessions),0), \
                    COALESCE(SUM(total_user_messages),0), \
                    COALESCE(SUM(total_assistant_messages),0), \
                    COALESCE(SUM(total_tool_use_messages),0), \
                    COALESCE(SUM(total_tool_result_messages),0) \
             FROM project_mart",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("project mart dimensions");
    assert_eq!(
        project_dims,
        (
            CORPUS_SESSIONS,
            prompts + tool_results,
            events,
            tool_events,
            tool_results,
        ),
        "project mart message dimensions"
    );
    assert_eq!(
        scalar(
            conn,
            "SELECT COALESCE(SUM(total_commands),0) FROM project_mart",
        ),
        prompts,
        "project command conservation"
    );
    assert_eq!(
        scalar(
            conn,
            "SELECT COALESCE(SUM(total_records),0) FROM project_mart",
        ),
        events + prompts + tool_results,
        "project all-record conservation"
    );
    for (column, expected) in [
        ("total_errors", 0),
        ("total_cache_read_messages", 0),
        ("total_commands_followed_by_interruption", 0),
        ("total_command_tools", tool_events),
        ("total_command_steps", events),
    ] {
        assert_eq!(
            scalar(
                conn,
                &format!("SELECT COALESCE(SUM({column}),0) FROM project_mart")
            ),
            expected,
            "project {column} conservation"
        );
    }

    let command_rows = count(conn, "command_mart") as i64;
    let command_events = scalar(
        conn,
        "SELECT COALESCE(SUM(event_count),0) FROM command_mart",
    );
    assert!(command_rows > 0, "command mart must be nonempty");
    assert_eq!(command_events, events, "command mart event conservation");

    let command_day_rows = count(conn, "command_day_mart") as i64;
    let command_days = scalar(
        conn,
        "SELECT COALESCE(SUM(command_count),0) FROM command_day_mart",
    );
    assert!(command_day_rows > 0, "command-day mart must be nonempty");
    assert_eq!(command_days, prompts, "command-day prompt conservation");

    let tool_rows = count(conn, "tool_mart") as i64;
    let tool_event_total = scalar(conn, "SELECT COALESCE(SUM(event_count),0) FROM tool_mart");
    assert!(tool_rows > 0, "tool mart must be nonempty");
    assert_eq!(tool_event_total, tool_events, "tool event conservation");

    let message_tool_rows = count(conn, "message_tool_mart") as i64;
    assert!(message_tool_rows > 0, "message/tool mart must be nonempty");
    assert_eq!(
        message_tool_rows, tool_events,
        "message/tool call conservation"
    );

    let actual = MartSnapshot::capture(conn);
    let expected = MartSnapshot::expected(conn, events, prompt_blocks);
    assert_snapshots_equivalent(
        &actual,
        &expected,
        "incremental/full canonical all-column mart oracle",
    );
    actual
}

fn seed_reindex_corpus(path: &Path) -> MartSnapshot {
    let conn = fresh_store(path);
    insert_reindex_dimensions(&conn);
    insert_reindex_window(&conn, 0, PREFIX_ROWS);
    assert_source_distribution(&conn, PREFIX_ROWS, BLOCKS_PER_SESSION - 1);

    let prefix_report =
        stax_etl::marts::watermark::refresh_all_marts(&conn, "2026-01-01T00:00:00+00:00")
            .expect("build canonical prefix marts");
    assert_report(&prefix_report, PREFIX_ROWS);
    assert_watermarks(&conn, PREFIX_ROWS);
    let prefix_snapshot = assert_mart_semantics(&conn, PREFIX_ROWS, BLOCKS_PER_SESSION - 1);

    insert_reindex_window(&conn, PREFIX_ROWS, CORPUS_ROWS);
    assert_source_distribution(&conn, CORPUS_ROWS, BLOCKS_PER_SESSION);
    assert_tail_distribution(&conn);
    assert_watermarks(&conn, PREFIX_ROWS);
    let stale_snapshot = assert_mart_semantics(&conn, PREFIX_ROWS, BLOCKS_PER_SESSION - 1);
    assert_snapshots_equivalent(
        &stale_snapshot,
        &prefix_snapshot,
        "tail insertion must not mutate the prefix mart snapshot",
    );
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .expect("seal canonical reindex fixture");
    prefix_snapshot
}

fn copy_corpus(source: &Path, destination: &Path) -> (Connection, Duration) {
    let started = Instant::now();
    std::fs::copy(source, destination).expect("copy deterministic corpus");
    let copy_wall = started.elapsed();
    let conn = stax_etl::ingest::guard::open_read_write(destination)
        .expect("open copied corpus through production connection path");
    assert_production_pragmas(&conn);
    assert_source_distribution(&conn, CORPUS_ROWS, BLOCKS_PER_SESSION);
    assert_tail_distribution(&conn);
    (conn, copy_wall)
}

#[test]
#[ignore = "release-only performance evidence; run rust/perf-gate.sh"]
fn incremental_reindex_distribution() {
    let fixture = reindex_fixture();
    let guard = perf::Guard::MinThroughput(2_000.0);
    perf::prepare_scenario(
        "incremental_reindex",
        &fixture,
        Some(INCREMENTAL_SAMPLES),
        guard,
    );
    let scenario_started = Instant::now();
    let monitor = perf::ResourceMonitor::start_continuous();
    let scratch = Scratch::new("incremental-reindex");
    let corpus = scratch.path().join("corpus.db");
    let prefix_snapshot = seed_reindex_corpus(&corpus);

    let mut copy_wall = Duration::ZERO;
    let mut samples = Vec::with_capacity(INCREMENTAL_SAMPLES);
    let mut completed_snapshot: Option<MartSnapshot> = None;
    for sample in 0..INCREMENTAL_SAMPLES {
        let path = scratch.path().join(format!("sample-{sample}.db"));
        let (conn, copied) = copy_corpus(&corpus, &path);
        copy_wall = copy_wall.saturating_add(copied);
        assert_watermarks(&conn, PREFIX_ROWS);
        let stale_snapshot = MartSnapshot::capture(&conn);
        assert_snapshots_equivalent(
            &stale_snapshot,
            &prefix_snapshot,
            "copied incremental source must retain the exact stale prefix snapshot",
        );
        assert_production_pragmas(&conn);
        let started = Instant::now();
        let report =
            stax_etl::marts::watermark::refresh_all_marts(&conn, "2026-01-01T00:00:00+00:00")
                .expect("incremental mart refresh");
        samples.push(perf::elapsed_ms(started));
        assert_report(&report, CHANGED_ROWS);
        assert_watermarks(&conn, CORPUS_ROWS);
        let snapshot = assert_mart_semantics(&conn, CORPUS_ROWS, BLOCKS_PER_SESSION);
        if let Some(reference) = &completed_snapshot {
            assert_snapshots_equivalent(
                &snapshot,
                reference,
                "every incremental sample must produce the same full snapshot",
            );
        } else {
            completed_snapshot = Some(snapshot);
        }
        drop(conn);
        remove_store_files(&path);
    }
    let resources = monitor.stop();
    let timed_wall = perf::sum_sample_time(&samples);
    perf::write_receipt(
        "incremental_reindex",
        &fixture,
        Some(INCREMENTAL_SAMPLES),
        &samples,
        perf::Timing::from_started(scenario_started, timed_wall, copy_wall),
        Some(CHANGED_ROWS as u64 * INCREMENTAL_SAMPLES as u64),
        resources,
        guard,
    );
}

#[test]
#[ignore = "release-only performance evidence; run rust/perf-gate.sh"]
fn full_reindex_distribution() {
    let fixture = reindex_fixture();
    let guard = perf::Guard::MinThroughput(1_500.0);
    perf::prepare_scenario("full_reindex", &fixture, Some(FULL_SAMPLES), guard);
    let scenario_started = Instant::now();
    let monitor = perf::ResourceMonitor::start_continuous();
    let scratch = Scratch::new("full-reindex");
    let corpus = scratch.path().join("corpus.db");
    let prefix_snapshot = seed_reindex_corpus(&corpus);

    let mut copy_wall = Duration::ZERO;
    let mut samples = Vec::with_capacity(FULL_SAMPLES);
    let mut completed_snapshot: Option<MartSnapshot> = None;
    for sample in 0..FULL_SAMPLES {
        let path = scratch.path().join(format!("sample-{sample}.db"));
        let (conn, copied) = copy_corpus(&corpus, &path);
        copy_wall = copy_wall.saturating_add(copied);
        assert_watermarks(&conn, PREFIX_ROWS);
        let stale_snapshot = MartSnapshot::capture(&conn);
        assert_snapshots_equivalent(
            &stale_snapshot,
            &prefix_snapshot,
            "copied full-rebuild source must retain the exact stale prefix snapshot",
        );
        assert_production_pragmas(&conn);
        let started = Instant::now();
        let report =
            stax_etl::marts::watermark::rebuild_all_marts(&conn, "2026-01-01T00:00:00+00:00")
                .expect("full mart rebuild");
        samples.push(perf::elapsed_ms(started));
        assert_report(&report, CORPUS_ROWS);
        assert_watermarks(&conn, CORPUS_ROWS);
        let snapshot = assert_mart_semantics(&conn, CORPUS_ROWS, BLOCKS_PER_SESSION);
        if let Some(reference) = &completed_snapshot {
            assert_snapshots_equivalent(
                &snapshot,
                reference,
                "every full-rebuild sample must produce the same full snapshot",
            );
        } else {
            completed_snapshot = Some(snapshot);
        }
        drop(conn);
        remove_store_files(&path);
    }
    let resources = monitor.stop();
    let timed_wall = perf::sum_sample_time(&samples);
    perf::write_receipt(
        "full_reindex",
        &fixture,
        Some(FULL_SAMPLES),
        &samples,
        perf::Timing::from_started(scenario_started, timed_wall, copy_wall),
        Some(CORPUS_ROWS as u64 * FULL_SAMPLES as u64),
        resources,
        guard,
    );
}

#[test]
#[ignore = "release-only performance evidence; run rust/perf-gate.sh"]
fn concurrent_writer_and_readers_distribution() {
    let fixture = &[
        ("writers", CONCURRENT_WRITERS),
        ("readers", CONCURRENT_READERS),
        ("duration_seconds", CONCURRENT_SECONDS),
    ];
    let policy_guard = perf::Guard::Concurrent {
        max_p99_ms: 500.0,
        errors: 0,
    };
    perf::prepare_scenario("concurrent_read_write", fixture, None, policy_guard);
    let scenario_started = Instant::now();
    let monitor = perf::ResourceMonitor::start_continuous();
    let scratch = Scratch::new("concurrent");
    let store_path = scratch.path().join("store.db");
    let conn = fresh_store(&store_path);
    perf::assert_exact(
        "concurrency initial rows",
        count(&conn, "captured_events"),
        0,
    );
    drop(conn);

    let participants = (CONCURRENT_WRITERS + CONCURRENT_READERS + 1) as usize;
    let ready = Arc::new(Barrier::new(participants));
    let start = Arc::new(Barrier::new(participants));
    let deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let errors = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicU64::new(0));
    let samples = Arc::new(Mutex::new(Vec::<f64>::new()));
    let mut workers = Vec::new();

    {
        let path = store_path.clone();
        let ready = Arc::clone(&ready);
        let start = Arc::clone(&start);
        let deadline = Arc::clone(&deadline);
        let errors = Arc::clone(&errors);
        let writes = Arc::clone(&writes);
        workers.push(thread::spawn(move || {
            let conn = stax_etl::ingest::guard::open_read_write(&path)
                .expect("open writer through production connection path");
            assert_production_pragmas(&conn);
            conn.busy_timeout(Duration::from_millis(500))
                .expect("writer busy timeout");
            ready.wait();
            start.wait();
            let until = deadline
                .lock()
                .expect("deadline lock")
                .expect("deadline set");
            let mut sequence = 0u64;
            while Instant::now() < until {
                sequence += 1;
                let result = conn.execute(
                    "INSERT INTO captured_events \
                     (ts, project_id, session_id, hook_id, event_kind, payload_json) \
                     VALUES (?, NULL, 'fixture-session', 'fixture-hook', 'boundary', '{}')",
                    [format!("fixture-{sequence:020}")],
                );
                match result {
                    Ok(_) => {
                        writes.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    for _ in 0..CONCURRENT_READERS {
        let path = store_path.clone();
        let ready = Arc::clone(&ready);
        let start = Arc::clone(&start);
        let deadline = Arc::clone(&deadline);
        let errors = Arc::clone(&errors);
        let reads = Arc::clone(&reads);
        let samples = Arc::clone(&samples);
        workers.push(thread::spawn(move || {
            let store = stax_core::store::Store::open_read_only(&path)
                .expect("open reader through production connection path");
            let conn = store.conn();
            conn.busy_timeout(Duration::from_millis(500))
                .expect("reader busy timeout");
            ready.wait();
            start.wait();
            let until = deadline
                .lock()
                .expect("deadline lock")
                .expect("deadline set");
            let mut local = Vec::new();
            while Instant::now() < until {
                let started = Instant::now();
                match conn.query_row("SELECT COUNT(*) FROM captured_events", [], |row| {
                    row.get::<_, i64>(0)
                }) {
                    Ok(_) => {
                        local.push(perf::elapsed_ms(started));
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            samples.lock().expect("samples lock").extend(local);
        }));
    }

    ready.wait();
    *deadline.lock().expect("deadline lock") =
        Some(Instant::now() + Duration::from_secs(CONCURRENT_SECONDS));
    let wall = Instant::now();
    start.wait();
    for worker in workers {
        worker.join().expect("concurrency worker");
    }
    let wall = wall.elapsed();
    let resources = monitor.stop();
    let operation_errors = errors.load(Ordering::Relaxed);
    let write_count = writes.load(Ordering::Relaxed);
    let read_count = reads.load(Ordering::Relaxed);
    let samples = Arc::try_unwrap(samples)
        .expect("reader sample ownership")
        .into_inner()
        .expect("reader samples");
    assert!(write_count > 0, "writer completed no operations");
    assert!(read_count > 0, "readers completed no operations");
    let conn = stax_etl::ingest::guard::open_read_write(&store_path)
        .expect("reopen concurrency store through production connection path");
    assert_production_pragmas(&conn);
    perf::assert_exact(
        "concurrency durable unique rows",
        count(&conn, "captured_events"),
        write_count,
    );
    perf::write_receipt(
        "concurrent_read_write",
        fixture,
        None,
        &samples,
        perf::Timing::from_started(scenario_started, wall, Duration::ZERO),
        None,
        resources,
        perf::Guard::Concurrent {
            max_p99_ms: 500.0,
            errors: operation_errors,
        },
    );
}
