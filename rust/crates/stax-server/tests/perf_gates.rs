//! Release-only performance evidence for the HTTP query and installed hook.
//!
//! Ordinary workspace tests see these scenarios as ignored. The bounded
//! runner in `rust/perf-gate.sh` opts into them, records distributional latency
//! and resource evidence, and validates their fixed fixture cardinality.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::Request as HttpRequest;
use rusqlite::Connection;
use serde_json::{Value, json};
use stax_server::state::{AppState, Config, CurrentProject};
use tower::ServiceExt as _;

#[path = "../../../perf/support.rs"]
mod perf;

const QUERY_MESSAGES: i64 = 100_000;
const QUERY_SESSIONS: i64 = 1_000;
const QUERY_PROJECTS: i64 = 20;
const QUERY_SAMPLES: usize = 100;
const HOOK_SAMPLES: usize = 200;
const QUERY_SLUG: &str = "performance-query";

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

fn fresh_store(dir: &Path) -> Connection {
    let conn = stax_etl::ingest::guard::open_read_write(&dir.join("store.db"))
        .expect("open performance store through production connection path");
    stax_core::schema::apply(&conn).expect("apply production schema");
    let journal: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read query fixture journal mode");
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("read query fixture durability");
    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .expect("read query fixture foreign-key mode");
    assert_eq!((journal.as_str(), synchronous, foreign_keys), ("wal", 1, 1));
    conn
}

fn seed_query_store(dir: &Path) {
    let conn = fresh_store(dir);
    let partition = stax_etl::ingest::writer::partition_for("2026-01-01T00:00:00Z");
    assert_eq!(partition, "messages_202601");
    stax_etl::ingest::writer::ensure_partition(&conn, &partition)
        .expect("create production query message partition");
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("begin fixture");
    {
        let mut project = conn
            .prepare(
                "INSERT INTO projects \
                 (id, provider, slug, path, display_name, first_seen, last_modified) \
                 VALUES (?, ?, ?, NULL, ?, 0, 0)",
            )
            .expect("prepare projects");
        for index in 0..QUERY_PROJECTS {
            project
                .execute(rusqlite::params![
                    index + 1,
                    format!("fixture-source-{index}"),
                    QUERY_SLUG,
                    format!("Fixture {index}")
                ])
                .expect("insert project");
        }
    }
    {
        let mut session = conn
            .prepare(
                "INSERT INTO sessions \
                 (id, project_id, session_id, first_ts, last_ts, message_count) \
                 VALUES (?, ?, ?, '2026-01-01T00:00:00Z', \
                         '2026-01-01T00:01:39Z', 100)",
            )
            .expect("prepare sessions");
        for index in 0..QUERY_SESSIONS {
            session
                .execute(rusqlite::params![
                    index + 1,
                    (index % QUERY_PROJECTS) + 1,
                    format!("fixture-session-{index}")
                ])
                .expect("insert session");
        }
    }
    {
        let insert = format!(
            "INSERT INTO {partition} \
                 (id, session_fk, seq, timestamp, role, model, input_tokens, \
                  output_tokens, cache_create_tokens, cache_read_tokens, \
                  content_text, tools_json, raw_json, is_sidechain, uuid, \
                         parent_uuid, speed) \
                 VALUES (?, ?, ?, '2026-01-01T00:00:00Z', 'assistant', \
                         'fixture-model', 1, 1, 0, 0, 'trace fixture', '[]', \
                         '{{}}', 0, ?, NULL, 'standard')"
        );
        let mut message = conn.prepare(&insert).expect("prepare messages");
        for index in 0..QUERY_MESSAGES {
            let session_fk = (index % QUERY_SESSIONS) + 1;
            let seq = index / QUERY_SESSIONS;
            message
                .execute(rusqlite::params![
                    index + 1,
                    session_fk,
                    seq,
                    format!("fixture-message-{index}")
                ])
                .expect("insert message");
        }
    }
    conn.execute(
        "UPDATE _messages_id_seq SET next_id = ? WHERE rowid_kind = 1",
        [QUERY_MESSAGES + 1],
    )
    .expect("advance message sequence");
    conn.execute_batch("COMMIT").expect("commit fixture");

    let messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .expect("count messages");
    let sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .expect("count sessions");
    let projects: i64 = conn
        .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
        .expect("count projects");
    let partition_messages: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {partition}"), [], |row| {
            row.get(0)
        })
        .expect("count production-partition messages");
    let unknown_messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages_unknown", [], |row| {
            row.get(0)
        })
        .expect("count unknown-partition messages");
    perf::assert_exact(
        "query messages after deduplication",
        messages as u64,
        QUERY_MESSAGES as u64,
    );
    perf::assert_exact("query sessions", sessions as u64, QUERY_SESSIONS as u64);
    perf::assert_exact("query projects", projects as u64, QUERY_PROJECTS as u64);
    perf::assert_exact(
        "query dated partition messages",
        partition_messages as u64,
        QUERY_MESSAGES as u64,
    );
    perf::assert_exact(
        "query unknown partition messages",
        unknown_messages as u64,
        0,
    );
}

fn query_app(dir: &Path) -> Router {
    let state = AppState::new(
        dir.join("store.db"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets"),
        Config::default(),
    );
    state.set_current_project(CurrentProject {
        project_path: Some(QUERY_SLUG.to_owned()),
        log_path: Some(dir.join(QUERY_SLUG).to_string_lossy().into_owned()),
    });
    stax_server::app(state)
}

async fn get(app: &Router, target: &str) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("GET")
                .uri(target)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes).expect("JSON body");
    (status, body)
}

fn assert_query_response(response: &(u16, Value)) {
    assert_eq!(response.0, 200);
    assert_eq!(
        response.1.get("total").and_then(Value::as_u64),
        Some(QUERY_MESSAGES as u64),
        "query must cover the complete post-deduplication fixture"
    );
}

#[tokio::test]
#[ignore = "release-only performance evidence; run rust/perf-gate.sh"]
async fn cold_and_warm_query_distributions() {
    let fixture = &[
        ("messages", QUERY_MESSAGES as u64),
        ("sessions", QUERY_SESSIONS as u64),
        ("projects", QUERY_PROJECTS as u64),
    ];
    let cold_guard = perf::Guard::MaxP99Ms(1_500.0);
    let warm_guard = perf::Guard::MaxP99Ms(250.0);
    perf::prepare_scenario("query_cold", fixture, Some(QUERY_SAMPLES), cold_guard);
    perf::prepare_scenario("query_warm", fixture, Some(QUERY_SAMPLES), warm_guard);

    let cold_monitor = perf::ResourceMonitor::start();
    let setup_started = Instant::now();
    let scratch = Scratch::new("query");
    seed_query_store(scratch.path());
    let shared_setup_wall = setup_started.elapsed();
    let mut cold = Vec::with_capacity(QUERY_SAMPLES);
    let mut last = None;
    for _ in 0..QUERY_SAMPLES {
        let started = Instant::now();
        last = Some(get(&query_app(scratch.path()), "/api/messages?per_page=1").await);
        cold.push(perf::elapsed_ms(started));
    }
    let cold_resources = cold_monitor.stop();
    assert_query_response(last.as_ref().expect("cold response"));
    perf::assert_exact(
        "cold query samples",
        cold.len() as u64,
        QUERY_SAMPLES as u64,
    );
    perf::write_receipt(
        "query_cold",
        fixture,
        Some(QUERY_SAMPLES),
        &cold,
        perf::Timing::explicit(
            shared_setup_wall,
            Duration::ZERO,
            perf::sum_sample_time(&cold),
        ),
        None,
        cold_resources,
        cold_guard,
    );

    let warm_setup_started = Instant::now();
    let warm_monitor = perf::ResourceMonitor::start();
    let warm_app = query_app(scratch.path());
    assert_query_response(&get(&warm_app, "/api/messages?per_page=1").await);
    let warm_setup_wall = shared_setup_wall.saturating_add(warm_setup_started.elapsed());
    let mut warm = Vec::with_capacity(QUERY_SAMPLES);
    let mut last = None;
    for _ in 0..QUERY_SAMPLES {
        let started = Instant::now();
        last = Some(get(&warm_app, "/api/messages?per_page=1").await);
        warm.push(perf::elapsed_ms(started));
    }
    let warm_resources = warm_monitor.stop();
    assert_query_response(last.as_ref().expect("warm response"));
    perf::assert_exact(
        "warm query samples",
        warm.len() as u64,
        QUERY_SAMPLES as u64,
    );
    perf::write_receipt(
        "query_warm",
        fixture,
        Some(QUERY_SAMPLES),
        &warm,
        perf::Timing::explicit(
            warm_setup_wall,
            Duration::ZERO,
            perf::sum_sample_time(&warm),
        ),
        None,
        warm_resources,
        warm_guard,
    );
}

fn release_hook_binary() -> PathBuf {
    let executable = format!("stax-hooks{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.ancestors()
                .find(|dir| dir.join(&executable).is_file())
                .map(|dir| dir.join(&executable))
        })
        .expect("release installed hook executable next to the performance test")
}

#[test]
#[ignore = "release-only performance evidence; run rust/perf-gate.sh"]
fn installed_hook_execution_distribution() {
    let fixture = &[
        ("hook_invocations", HOOK_SAMPLES as u64),
        ("recorded_events", HOOK_SAMPLES as u64),
    ];
    let guard = perf::Guard::MaxP99Ms(50.0);
    perf::prepare_scenario("hook_execution", fixture, Some(HOOK_SAMPLES), guard);
    let scenario_started = Instant::now();
    let monitor = perf::ResourceMonitor::start();
    let scratch = Scratch::new("hook");
    let hook = release_hook_binary();

    let mut child_resources = perf::Resources::default();
    let mut samples = Vec::with_capacity(HOOK_SAMPLES);
    for invocation in 0..HOOK_SAMPLES {
        let payload = json!({
            "session_id": format!("performance-fixture-{invocation}"),
            "cwd": "",
            "hook_event_name": "Stop",
        })
        .to_string();
        let started = Instant::now();
        let mut child = std::process::Command::new(&hook)
            .args(["run", "staxtrace-stop"])
            .env("STAXTRACE_HOME", scratch.path())
            .env("PROACTIVE_DISABLED", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn installed hook command");
        let child_monitor = perf::ChildProcessMonitor::start(child.id());
        {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("hook stdin")
                .write_all(payload.as_bytes())
                .expect("write hook payload");
        }
        child_monitor.begin_processing();
        let status = child.wait().expect("wait for installed hook");
        samples.push(perf::elapsed_ms(started));
        let child_sample = child_monitor.stop();
        perf::assert_exact(
            "post-input hook child observations",
            child_sample.child_process_sample_count,
            1,
        );
        child_resources = child_resources.merge(child_sample);
        assert!(
            status.success(),
            "installed hook keeps the never-block exit contract"
        );
    }
    let resources = monitor.stop().merge(child_resources);
    perf::assert_exact("hook samples", samples.len() as u64, HOOK_SAMPLES as u64);
    perf::assert_exact(
        "observed hook child invocations",
        resources.child_process_sample_count,
        HOOK_SAMPLES as u64,
    );
    let conn = Connection::open(scratch.path().join("store.db")).expect("open hook evidence store");
    perf::assert_exact(
        "durable hook events",
        count_table(&conn, "captured_events"),
        HOOK_SAMPLES as u64,
    );
    let timed_wall = perf::sum_sample_time(&samples);
    perf::write_receipt(
        "hook_execution",
        fixture,
        Some(HOOK_SAMPLES),
        &samples,
        perf::Timing::from_started(scenario_started, timed_wall, Duration::ZERO),
        None,
        resources,
        guard,
    );
}

#[test]
fn child_monitor_observes_fast_post_input_roots_before_reaping() {
    use std::io::Write as _;

    let workload = r#"import sys
sys.stdin.buffer.read()
sum(index * index for index in range(125000))
"#;
    for invocation in 0..32 {
        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(workload)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn fast post-input root");
        let monitor = perf::ChildProcessMonitor::start(child.id());
        {
            child
                .stdin
                .take()
                .expect("fast-root stdin")
                .write_all(format!("begin-{invocation}").as_bytes())
                .expect("release fast root");
        }
        monitor.begin_processing();
        assert!(child.wait().expect("wait fast root").success());
        assert!(
            child
                .try_wait()
                .expect("confirm fast root reaped")
                .is_some(),
            "fast root remains unreaped"
        );
        let resources = monitor.stop();
        assert_eq!(resources.child_process_sample_count, 1);
        assert!(resources.child_peak_rss_bytes > 0);
        assert!(resources.child_peak_open_files > 0);
    }
}

fn count_table(conn: &Connection, table: &str) -> u64 {
    let count: i64 = conn
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("count {table}: {error}"));
    u64::try_from(count).expect("non-negative table count")
}

#[test]
fn child_monitor_observes_allocation_after_input_in_a_descendant() {
    use std::io::Write as _;

    let scratch = Scratch::new("child-monitor");
    let held_file = scratch.path().join("held-open");
    std::fs::write(&held_file, b"fixture").expect("write delayed-child fixture");
    let descendant = r#"import sys,time
held=[open(sys.argv[1], 'rb') for _ in range(64)]
memory=bytearray(64 * 1024 * 1024)
memory[::4096]=b'x' * (len(memory) // 4096)
time.sleep(0.4)
assert held and memory
"#;
    let supervisor = r#"import subprocess,sys
sys.stdin.buffer.read()
child=subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]])
raise SystemExit(child.wait())
"#;
    let mut child = std::process::Command::new("python3")
        .arg("-c")
        .arg(supervisor)
        .arg(descendant)
        .arg(&held_file)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn delayed-child supervisor");
    let monitor = perf::ChildProcessMonitor::start(child.id());
    child
        .stdin
        .take()
        .expect("delayed-child stdin")
        .write_all(b"begin")
        .expect("release delayed child");
    monitor.begin_processing();
    assert!(child.wait().expect("wait delayed child").success());
    let resources = monitor.stop();
    assert_eq!(resources.child_process_sample_count, 1);
    assert!(resources.child_peak_rss_bytes >= 48 * 1024 * 1024);
    assert!(resources.child_peak_open_files >= 64);
    assert!(resources.rss_sample_count > 0);
    assert!(resources.open_file_sample_count > 0);
}
