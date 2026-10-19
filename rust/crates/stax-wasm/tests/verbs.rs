//! Native tests for the wasm query layer.
//!
//! The same `verbs` module the browser runs, driven natively against a small
//! store built here — which is the point of the `rlib`/`cdylib` split: the
//! logic is gateable by `cargo test` on a box with no wasm toolchain at all,
//! and `rust/wasm-differ.sh` then proves the *compiled* artifact agrees with
//! the CLI on the maintainer's real data.
//!
//! The fixture is a trimmed real schema: `projects`, `sessions`, one monthly
//! `messages_YYYYMM` partition with the `messages` UNION-ALL view over it
//! (§6b — the view is the shape the queries plan against, so a fixture with a
//! plain `messages` table would test a database this project does not have),
//! and `session_mart` for the cost term.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use stax_wasm::db;
use stax_wasm::verbs::{self, Options, Request};

// ── fixture ──────────────────────────────────────────────────────────────────

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before the epoch")
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("stax-wasm-{}-{nanos}-{seq}", std::process::id()));
        std::fs::create_dir_all(&path).expect("scratch dir");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const SCHEMA: &str = "
CREATE TABLE projects (
  id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL, path TEXT,
  display_name TEXT NOT NULL, first_seen REAL NOT NULL, last_modified REAL NOT NULL,
  worktree_of TEXT, UNIQUE (provider, slug));
CREATE TABLE sessions (
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id),
  session_id TEXT NOT NULL, first_ts TEXT, last_ts TEXT,
  message_count INTEGER NOT NULL DEFAULT 0, team_id TEXT, spawned_by_session_id TEXT,
  spawn_prompt TEXT, agent_role TEXT, UNIQUE (project_id, session_id));
CREATE TABLE messages_202607 (
  id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL REFERENCES sessions(id),
  seq INTEGER NOT NULL, timestamp TEXT NOT NULL, role TEXT NOT NULL, model TEXT,
  input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
  cache_create_tokens INTEGER NOT NULL DEFAULT 0, cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  content_text TEXT NOT NULL DEFAULT '', tools_json TEXT NOT NULL DEFAULT '[]',
  raw_json TEXT NOT NULL, is_sidechain INTEGER NOT NULL DEFAULT 0, uuid TEXT,
  parent_uuid TEXT, speed TEXT NOT NULL DEFAULT 'standard', UNIQUE (session_fk, seq));
CREATE VIEW messages AS SELECT id, session_fk, seq, timestamp, role, model,
  input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, content_text,
  tools_json, raw_json, is_sidechain, uuid, parent_uuid, speed FROM messages_202607;
CREATE TABLE session_mart (
  session_id TEXT PRIMARY KEY, project_id INTEGER NOT NULL, provider TEXT NOT NULL,
  primary_model TEXT, first_ts TEXT NOT NULL, last_ts TEXT NOT NULL,
  message_count INTEGER NOT NULL DEFAULT 0, user_message_count INTEGER NOT NULL DEFAULT 0,
  assistant_message_count INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0, cache_read INTEGER NOT NULL DEFAULT 0,
  cache_create INTEGER NOT NULL DEFAULT 0, cost_usd REAL NOT NULL DEFAULT 0.0,
  is_one_shot INTEGER NOT NULL DEFAULT 0, cwd TEXT);
";

/// One project, two sessions, four messages — enough for every verb to have
/// both a hit and a miss.
fn fixture() -> (Scratch, PathBuf) {
    let scratch = Scratch::new();
    let path = scratch.join("store.db");
    let conn = rusqlite::Connection::open(&path).expect("create fixture");
    conn.execute_batch(SCHEMA).expect("schema");
    conn.pragma_update(None, "user_version", 30)
        .expect("user_version");
    conn.execute_batch(
        "
INSERT INTO projects VALUES (1,'claude','-tmp-demo','/tmp/demo','demo',0.0,0.0,NULL);
INSERT INTO sessions VALUES (1,1,'sess-one','2026-07-01T00:00:00Z','2026-07-02T00:00:00Z',3,NULL,NULL,NULL,NULL);
INSERT INTO sessions VALUES (2,1,'sess-two','2026-07-03T00:00:00Z','2026-07-04T00:00:00Z',1,NULL,NULL,NULL,NULL);
INSERT INTO messages_202607 (id,session_fk,seq,timestamp,role,content_text,tools_json,raw_json)
  VALUES (1,1,0,'2026-07-01T00:00:00Z','user','we decided to add a cache layer','[]','{}');
INSERT INTO messages_202607 (id,session_fk,seq,timestamp,role,content_text,tools_json,raw_json)
  VALUES (2,1,1,'2026-07-01T00:01:00Z','assistant','the cache worked, tests pass','[{\"name\":\"Edit\",\"input\":{\"file_path\":\"/tmp/demo/app.py\"}}]','{}');
INSERT INTO messages_202607 (id,session_fk,seq,timestamp,role,content_text,tools_json,raw_json)
  VALUES (3,1,2,'2026-07-02T00:00:00Z','assistant','reverted the cache change, it failed','[{\"name\":\"Edit\",\"input\":{\"file_path\":\"/tmp/demo/app.py\"}}]','{}');
INSERT INTO messages_202607 (id,session_fk,seq,timestamp,role,content_text,tools_json,raw_json)
  VALUES (4,2,0,'2026-07-03T00:00:00Z','user','unrelated chatter','[]','{}');
INSERT INTO session_mart VALUES ('sess-one',1,'claude',NULL,'2026-07-01T00:00:00Z','2026-07-02T00:00:00Z',3,1,2,0,0,0,0,1.25,0,'/tmp/demo');
",
    )
    .expect("rows");
    let prefix = stax_core::queries::paths::resolve_input_path("/tmp/demo");
    conn.execute("UPDATE projects SET path=?1", [&prefix])
        .unwrap();
    conn.execute("UPDATE session_mart SET cwd=?1", [&prefix])
        .unwrap();
    conn.execute(
        "UPDATE messages_202607 SET tools_json=replace(tools_json,'/tmp/demo',?1)",
        [&prefix],
    )
    .unwrap();
    drop(conn);
    (scratch, path)
}

/// A fixed clock: 2026-08-01T00:00:00Z. Every test pins it, so no assertion in
/// this file can pass or fail because of when it ran.
const NOW: f64 = 1_785_888_000.0;

fn options() -> Options {
    Options {
        now_epoch: NOW,
        cwd: stax_core::queries::paths::resolve_input_path("/tmp/demo"),
        ..Options::default()
    }
}

fn run(path: &Path, request: &Request) -> verbs::Outcome {
    let conn = db::open_path(path).expect("open the fixture read-only");
    verbs::run(&conn, request).expect("the verb ran")
}

fn envelope(outcome: &verbs::Outcome) -> Value {
    serde_json::from_str(&outcome.stdout).expect("the envelope parses")
}

// ── the contract ─────────────────────────────────────────────────────────────

#[test]
fn decisions_finds_the_seeded_session_and_keeps_the_envelope_shape() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Decisions {
            query: "cache".into(),
            options: options(),
        },
    );
    assert_eq!(outcome.code, 0);
    let value = envelope(&outcome);
    assert_eq!(value["schema"], "staxtrace.memory/1");
    assert_eq!(value["command"], "decisions");
    assert_eq!(value["query"]["text"], "cache");
    // The cwd fell inside the project, so the scope echoed back is its slug —
    // the reference's `_detect_cwd_project_slug` behaviour, not a default.
    assert_eq!(value["query"]["project"], "-tmp-demo");
    assert_eq!(value["result_count"], 1);
    assert_eq!(value["results"][0]["session_id"], "sess-one");
    assert_eq!(value["results"][0]["cost_usd"], 1.25);
    assert_eq!(value["budget"], 2000);
    assert_eq!(value["truncated"], false);
}

#[test]
fn the_envelope_key_order_is_the_contract() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Decisions {
            query: "cache".into(),
            options: options(),
        },
    );
    let keys: Vec<&str> = outcome
        .stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix('"'))
        .filter_map(|line| line.split('"').next())
        .collect();
    // `preserve_order` is on workspace-wide; this asserts the top-level order a
    // golden file would, without needing the golden.
    let head: Vec<&str> = keys.iter().take(3).copied().collect();
    assert_eq!(head, vec!["schema", "command", "query"]);
    assert!(outcome.stdout.ends_with("}\n"), "click.echo's newline");
}

#[test]
fn an_empty_project_scope_means_every_project_not_the_cwd() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Decisions {
            query: "cache".into(),
            options: Options {
                project: Some(String::new()),
                cwd: "/somewhere/else".into(),
                ..options()
            },
        },
    );
    let value = envelope(&outcome);
    assert_eq!(value["query"]["project"], "");
    assert_eq!(
        value["result_count"], 1,
        "the empty slug did not scope away the hit"
    );
}

#[test]
fn a_query_with_no_word_characters_is_the_error_envelope_and_exit_1() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Decisions {
            query: "***".into(),
            options: options(),
        },
    );
    assert_eq!(outcome.code, 1);
    let value = envelope(&outcome);
    assert_eq!(
        value["error"],
        "query has no searchable terms — provide at least one word to search for"
    );
    assert!(
        value.get("results").is_none(),
        "an error envelope has no results"
    );
}

#[test]
fn a_malformed_since_is_a_value_error_envelope_not_a_panic() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Decisions {
            query: "cache".into(),
            options: Options {
                since: Some("not-a-date".into()),
                ..options()
            },
        },
    );
    assert_eq!(outcome.code, 1);
    assert!(envelope(&outcome)["error"].as_str().is_some());
}

#[test]
fn file_reports_risk_and_tags_each_row_with_its_kind() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::File {
            path: "/tmp/demo/app.py".into(),
            options: options(),
        },
    );
    let value = envelope(&outcome);
    assert_eq!(value["command"], "file");
    assert_eq!(
        value["query"]["path"],
        stax_core::queries::paths::resolve_input_path("/tmp/demo/app.py")
    );
    let risk = &value["risk"];
    assert_eq!(
        risk["path"],
        stax_core::queries::paths::resolve_input_path("/tmp/demo/app.py")
    );
    assert_eq!(risk["total_sessions"], 1);
    // The keys and their order are the contract (`RiskSummary::to_dict`); the
    // classifier's own counts are stax-core's business, and this fixture's
    // synthetic transcript does not clear its confidence ladder — asserting a
    // number here would be asserting the fixture, not the port.
    let keys: Vec<&str> = risk
        .as_object()
        .expect("risk")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "path",
            "since",
            "total_sessions",
            "reverted",
            "failed",
            "worked",
            "recent_session_ids"
        ]
    );
    let kinds: Vec<&str> = value["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|row| row["kind"].as_str().expect("kind"))
        .collect();
    // `touched`, not `failure_mode`: this fixture's synthetic transcript does
    // not clear the outcome classifier's confidence ladder, so the merge falls
    // through to the touching leg. The `failure_mode` tag is exercised against
    // the maintainer's real store by `rust/wasm-differ.sh` (W-file-cli), which
    // is the right place for a heuristic that reads prose.
    assert_eq!(kinds, vec!["touched"]);
}

#[test]
fn a_relative_file_path_resolves_against_the_declared_cwd() {
    // The divergence the first differ run caught: without this, wasm resolved
    // against `/` and answered a different question than the CLI did.
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::File {
            path: "app.py".into(),
            options: options(),
        },
    );
    assert_eq!(
        envelope(&outcome)["risk"]["path"],
        stax_core::queries::paths::resolve_input_path("/tmp/demo/app.py")
    );
}

#[test]
fn worked_finds_the_positive_outcome_only() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Worked {
            action: "cache".into(),
            options: options(),
        },
    );
    let value = envelope(&outcome);
    assert_eq!(value["command"], "worked");
    for row in value["results"].as_array().expect("results") {
        assert_eq!(row["outcome"], "worked");
    }
}

#[test]
fn sessions_scopes_to_a_path_and_labels_the_scope() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Sessions {
            path: Some("/tmp/demo".into()),
            options: options(),
        },
    );
    let value = envelope(&outcome);
    assert_eq!(value["query"]["scope"], "path");
    assert_eq!(
        value["result_count"], 2,
        "both sessions live under the path"
    );
}

#[test]
fn sessions_takes_the_callers_word_on_whether_the_target_is_a_file() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Sessions {
            path: Some("/tmp/demo/app.py".into()),
            options: Options {
                is_file: true,
                ..options()
            },
        },
    );
    let value = envelope(&outcome);
    assert_eq!(value["query"]["scope"], "file");
    assert_eq!(value["result_count"], 1, "only the session that touched it");
}

#[test]
fn a_tiny_budget_truncates_and_says_so() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Sessions {
            path: Some("/tmp/demo".into()),
            options: Options {
                context_budget: Some(1),
                ..options()
            },
        },
    );
    let value = envelope(&outcome);
    assert_eq!(value["budget"], 1);
    assert_eq!(value["truncated"], true);
    assert_eq!(value["result_count"], 0, "not one row fits in one token");
}

#[test]
fn store_renders_the_bytes_the_cli_prints() {
    let (_scratch, path) = fixture();
    let outcome = run(
        &path,
        &Request::Store {
            options: Options {
                store_label: "/data/su/store.db".into(),
                ..options()
            },
        },
    );
    let lines: Vec<&str> = outcome.stdout.lines().collect();
    assert_eq!(lines[0], "store: /data/su/store.db");
    assert_eq!(lines[1], "schema: v030");
    assert_eq!(lines[2], "objects: 5");
    assert_eq!(lines[3], "");
    assert_eq!(
        lines[4],
        "NAME                                     KIND           ROWS"
    );
    // The view is tagged as one: its rows are also counted under the partition.
    let messages = lines
        .iter()
        .find(|line| line.starts_with("messages  "))
        .expect("the messages row");
    assert!(messages.contains("view"), "{messages}");
    assert!(messages.trim_end().ends_with('4'), "{messages}");
}

#[test]
fn a_request_that_is_not_json_is_a_deserialize_error_not_a_panic() {
    let bad: Result<Request, _> = serde_json::from_str("{\"verb\":\"nope\"}");
    assert!(bad.is_err());
    let unknown: Result<Request, _> =
        serde_json::from_str("{\"verb\":\"decisions\",\"query\":\"x\",\"nope\":1}");
    assert!(
        unknown.is_err(),
        "deny_unknown_fields catches a typo'd option"
    );
}

#[test]
fn defaults_are_the_references_defaults() {
    let options = Options::default();
    assert_eq!(options.limit, 20);
    assert_eq!(options.budget_default, 2000);
    assert_eq!(options.weights, (0.5, 0.2, 0.3));
    assert!(!options.is_file);
}

#[test]
fn opening_something_that_is_not_a_store_is_an_error() {
    let scratch = Scratch::new();
    let path = scratch.join("not-a-db");
    std::fs::write(&path, b"this is not a database").expect("write");
    let conn = db::open_path(&path).expect("sqlite opens lazily");
    let request = Request::Store { options: options() };
    assert!(
        verbs::run(&conn, &request).is_err(),
        "the first real read must fail rather than answer"
    );
}
