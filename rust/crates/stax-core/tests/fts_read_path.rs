//! RS-1-007 — the FTS5/bm25 branch of the `memory` read path.
//!
//! `services/discovery.py` takes a *different* code path in four of the five
//! `memory` verbs when `search_index.db` is populated, and the wave-1 port
//! implemented only the `search_service=None` half. These tests pin the other
//! half: which verbs consult the index, what makes them fall back, how the two
//! halves merge, and the two places where the merged output is self-
//! contradictory on purpose (bug-for-bug).
//!
//! Everything runs against a real FTS5 index and a real partitioned store, on
//! disk, because the contract is about two databases joined in Rust by
//! `session_id` — an in-memory shortcut would test a different program.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use stax_core::lexical::LexicalIndex;
use stax_core::queries::{self, rank};

// ── fixtures ─────────────────────────────────────────────────────────────────

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "stax-fts-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Schema v030, trimmed to what discovery reads. `messages` is a UNION-ALL view
/// over monthly partitions, as the real store's is.
const STORE_SCHEMA: &str = "
    CREATE TABLE projects (
      id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
      path TEXT, display_name TEXT NOT NULL, first_seen REAL NOT NULL,
      last_modified REAL NOT NULL, worktree_of TEXT);
    CREATE TABLE sessions (
      id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
      session_id TEXT NOT NULL, first_ts TEXT, last_ts TEXT,
      message_count INTEGER NOT NULL DEFAULT 0);
    CREATE TABLE session_mart (
      session_id TEXT PRIMARY KEY, project_id INTEGER NOT NULL,
      provider TEXT NOT NULL, first_ts TEXT NOT NULL, last_ts TEXT NOT NULL,
      message_count INTEGER NOT NULL DEFAULT 0,
      cost_usd REAL NOT NULL DEFAULT 0.0);
    CREATE TABLE messages_202601 (
      id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
      timestamp TEXT NOT NULL, role TEXT NOT NULL, model TEXT,
      content_text TEXT NOT NULL DEFAULT '', tools_json TEXT NOT NULL DEFAULT '[]',
      raw_json TEXT NOT NULL DEFAULT '{}', is_sidechain INTEGER NOT NULL DEFAULT 0);
    CREATE TABLE messages_202602 (
      id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
      timestamp TEXT NOT NULL, role TEXT NOT NULL, model TEXT,
      content_text TEXT NOT NULL DEFAULT '', tools_json TEXT NOT NULL DEFAULT '[]',
      raw_json TEXT NOT NULL DEFAULT '{}', is_sidechain INTEGER NOT NULL DEFAULT 0);
    CREATE VIEW messages AS
      SELECT id, session_fk, seq, timestamp, role, model, content_text,
             tools_json, raw_json, is_sidechain FROM messages_202601
      UNION ALL
      SELECT id, session_fk, seq, timestamp, role, model, content_text,
             tools_json, raw_json, is_sidechain FROM messages_202602;
";

const SID_A: &str = "aaaaaaaa-1111-4111-8111-111111111111";
const SID_B: &str = "bbbbbbbb-2222-4222-8222-222222222222";
const SID_C: &str = "cccccccc-3333-4333-8333-333333333333";

/// Three sessions in one project.
///
/// * A — says "cache" once, and *edits* `/home/dev/alpha/main.py`.
/// * B — says "cache" three times across two messages, touches nothing.
/// * C — mentions `/home/dev/alpha/main.py` in prose only, and the words
///   `cache` and `lookup` non-adjacently (the phrase the `LIKE` path misses).
fn store(scratch: &Scratch) -> Connection {
    let conn = Connection::open(scratch.join("store.db")).expect("store");
    conn.execute_batch(STORE_SCHEMA).expect("schema");
    conn.execute_batch(&format!(
        r#"
        INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified)
        VALUES (1, 'claude', '-home-dev-alpha', NULL, 'alpha', 0, 0);

        INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count)
        VALUES (1, 1, '{SID_A}', '2026-01-01T09:00:00+00:00', '2026-01-01T10:00:00+00:00', 3),
               (2, 1, '{SID_B}', '2026-01-02T09:00:00+00:00', '2026-01-02T10:00:00+00:00', 2),
               (3, 1, '{SID_C}', '2026-01-03T09:00:00+00:00', '2026-01-03T10:00:00+00:00', 2);

        INSERT INTO session_mart (session_id, project_id, provider, first_ts, last_ts,
                                  message_count, cost_usd)
        VALUES ('{SID_A}', 1, 'claude', '2026-01-01T09:00:00+00:00',
                '2026-01-01T10:00:00+00:00', 3, 1.0);

        INSERT INTO messages_202601
          (id, session_fk, seq, timestamp, role, content_text, tools_json)
        VALUES
          (1, 1, 1, '2026-01-01T09:10:00+00:00', 'user',
           'we should cache the watermark', '[]'),
          (2, 1, 2, '2026-01-01T09:20:00+00:00', 'assistant', '',
           '[{{"name": "Edit", "input": {{"file_path": "/home/dev/alpha/main.py"}}}}]'),
          (3, 1, 3, '2026-01-01T09:30:00+00:00', 'user', 'that worked, thanks', '[]'),

          (4, 2, 1, '2026-01-02T09:10:00+00:00', 'user',
           'cache cache cache everywhere', '[]'),
          (5, 2, 2, '2026-01-02T09:20:00+00:00', 'user', 'the cache again', '[]'),

          (6, 3, 1, '2026-01-03T09:10:00+00:00', 'user',
           'the cache is consulted on every lookup in /home/dev/alpha/main.py', '[]'),
          (7, 3, 2, '2026-01-03T09:20:00+00:00', 'user', 'that worked, thanks', '[]');
        "#
    ))
    .expect("seed");
    let prefix = queries::paths::resolve_input_path("/home/dev/alpha");
    conn.execute("UPDATE projects SET path=?1 WHERE id=1", [&prefix])
        .expect("canonical project path");
    conn.execute(
        "UPDATE messages_202601 SET tools_json=replace(tools_json,'/home/dev/alpha',?1),
         content_text=replace(content_text,'/home/dev/alpha',?1)",
        [&prefix],
    )
    .expect("canonical fixture paths");
    conn
}

/// `search_index.db` with the reference schema. `rows` is
/// `(session_id, project, content, timestamp)`.
fn index(scratch: &Scratch, rows: &[(&str, &str, &str, &str)]) -> LexicalIndex {
    let path = scratch.join("search_index.db");
    let conn = Connection::open(&path).expect("index");
    conn.execute_batch(
        "CREATE TABLE messages (
             id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
             project TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL,
             timestamp TEXT, model TEXT, tokens_input INTEGER DEFAULT 0,
             tokens_output INTEGER DEFAULT 0);
         CREATE VIRTUAL TABLE messages_fts USING fts5(
             content, content='messages', content_rowid='id',
             tokenize='porter unicode61');
         CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
         END;",
    )
    .expect("index schema");
    for (session_id, project, content, timestamp) in rows {
        let content = content.replace(
            "/home/dev/alpha",
            &queries::paths::resolve_input_path("/home/dev/alpha"),
        );
        conn.execute(
            "INSERT INTO messages (session_id, project, role, content, timestamp) \
             VALUES (?, ?, 'user', ?, ?)",
            rusqlite::params![session_id, project, content, timestamp],
        )
        .expect("index row");
    }
    drop(conn);
    LexicalIndex::at(path)
}

/// The index mirroring the store's content — what a real `reindex` produces.
fn mirrored_index(scratch: &Scratch) -> LexicalIndex {
    index(
        scratch,
        &[
            (
                SID_A,
                "-home-dev-alpha",
                "we should cache the watermark",
                "2026-01-01T09:10:00",
            ),
            (
                SID_B,
                "-home-dev-alpha",
                "cache cache cache everywhere",
                "2026-01-02T09:10:00",
            ),
            (
                SID_B,
                "-home-dev-alpha",
                "the cache again",
                "2026-01-02T09:20:00",
            ),
            (
                SID_C,
                "-home-dev-alpha",
                "the cache is consulted on every lookup in /home/dev/alpha/main.py",
                "2026-01-03T09:10:00",
            ),
        ],
    )
}

fn budget() -> rank::Budget {
    // A fixed clock: the recency term must not depend on when the suite runs.
    rank::Budget::at(2000, rank::DEFAULT_RANK_WEIGHTS, 1_800_000_000.0)
}

fn ids(matches: &[queries::SessionMatch]) -> Vec<&str> {
    matches.iter().map(|m| m.session_id.as_str()).collect()
}

// ── the fallback contract ────────────────────────────────────────────────────

#[test]
fn no_index_configured_is_the_like_path() {
    let scratch = Scratch::new("noindex");
    let conn = store(&scratch);

    let with_none =
        queries::search_past_decisions_indexed(&conn, None, "cache", None, None, 20, &budget())
            .expect("query");
    let direct =
        queries::search_past_decisions(&conn, "cache", None, None, 20, &budget()).expect("query");
    assert_eq!(ids(&with_none.sessions), ids(&direct.sessions));
    // The LIKE path's snippet and clustering count travel with it.
    assert_eq!(
        with_none
            .sessions
            .iter()
            .map(|m| m.snippet.clone())
            .collect::<Vec<_>>(),
        direct
            .sessions
            .iter()
            .map(|m| m.snippet.clone())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn a_missing_index_file_falls_back_to_the_like_path() {
    let scratch = Scratch::new("missing");
    let conn = store(&scratch);
    let absent = LexicalIndex::at(scratch.join("search_index.db"));

    let routed = queries::search_past_decisions_indexed(
        &conn,
        Some(&absent),
        "cache",
        None,
        None,
        20,
        &budget(),
    )
    .expect("query");
    let direct =
        queries::search_past_decisions(&conn, "cache", None, None, 20, &budget()).expect("query");
    assert_eq!(ids(&routed.sessions), ids(&direct.sessions));
}

#[test]
fn an_empty_index_falls_back_but_a_populated_no_match_does_not() {
    let scratch = Scratch::new("emptyidx");
    let conn = store(&scratch);

    // Empty index → unpopulated → LIKE scan, which finds all three sessions.
    let empty = index(&scratch, &[]);
    let fell_back = queries::search_past_decisions_indexed(
        &conn,
        Some(&empty),
        "cache",
        None,
        None,
        20,
        &budget(),
    )
    .expect("query");
    assert_eq!(fell_back.sessions.len(), 3);

    // Populated index, query matches nothing in it → an honest empty result.
    // Falling back here would silently reintroduce the full scan.
    let scratch2 = Scratch::new("nomatch");
    let conn2 = store(&scratch2);
    let populated = index(
        &scratch2,
        &[(
            SID_A,
            "-home-dev-alpha",
            "unrelated prose",
            "2026-01-01T09:10:00",
        )],
    );
    let honest = queries::search_past_decisions_indexed(
        &conn2,
        Some(&populated),
        "cache",
        None,
        None,
        20,
        &budget(),
    )
    .expect("query");
    assert!(
        honest.sessions.is_empty(),
        "a populated index that matched nothing must NOT re-run the LIKE scan"
    );
}

// ── decisions ────────────────────────────────────────────────────────────────

#[test]
fn decisions_on_the_fts_path_carries_bm25_snippets_and_clustering() {
    let scratch = Scratch::new("dec");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);

    let result = queries::search_past_decisions_indexed(
        &conn,
        Some(&idx),
        "cache",
        None,
        None,
        20,
        &budget(),
    )
    .expect("query");
    assert_eq!(result.sessions.len(), 3);

    // The clustering count is FTS-native: B had two indexed messages, so one
    // *further* hit beyond its representative. This is NOT what the LIKE path
    // reports for the same session (messages-minus-one over the store), and
    // the two paths disagreeing is the recorded contract.
    let b = result
        .sessions
        .iter()
        .find(|m| m.session_id == SID_B)
        .expect("B");
    assert_eq!(b.more_matches_in_session, Some(1));
    let a = result
        .sessions
        .iter()
        .find(|m| m.session_id == SID_A)
        .expect("A");
    assert_eq!(
        a.more_matches_in_session, None,
        "a single hit clusters to None"
    );

    // The snippet is built by the same Python builder the LIKE path uses, from
    // the bm25-best message.
    assert!(b.snippet.as_deref().unwrap_or("").contains("cache"));
}

#[test]
fn the_bm25_signal_rides_in_the_rank_function_not_the_list_order() {
    // Two runs over the same candidates with the weights swapped. Pure
    // relevance orders by bm25 (B is the densest); pure recency orders by
    // `last_ts DESC`, which is the order the plain list is built in. Both
    // orders coming out of the same query is the proof that the FTS score
    // reaches the packer instead of being baked into the list.
    let scratch = Scratch::new("decrank");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);

    let by_bm25 = queries::search_past_decisions_indexed(
        &conn,
        Some(&idx),
        "cache",
        None,
        None,
        20,
        &rank::Budget::at(2000, (0.0, 0.0, 1.0), 1_800_000_000.0),
    )
    .expect("query");
    assert_eq!(
        ids(&by_bm25.sessions)[0],
        SID_B,
        "densest session ranks first"
    );

    let by_recency = queries::search_past_decisions_indexed(
        &conn,
        Some(&idx),
        "cache",
        None,
        None,
        20,
        &rank::Budget::at(2000, (1.0, 0.0, 0.0), 1_800_000_000.0),
    )
    .expect("query");
    assert_eq!(ids(&by_recency.sessions), vec![SID_C, SID_B, SID_A]);
}

#[test]
fn the_phrase_that_zeroes_on_like_finds_a_session_on_fts() {
    // Findings-ledger #3, and the reason this branch is not cosmetic: on the
    // LIKE path `%cache lookup%` is a literal substring test and matches
    // nothing; the same query on the FTS path finds session C.
    let scratch = Scratch::new("phrase");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);

    let like = queries::search_past_decisions(&conn, "cache lookup", None, None, 20, &budget())
        .expect("query");
    assert!(
        like.sessions.is_empty(),
        "the LIKE path is blind to the phrase"
    );

    let fts = queries::search_past_decisions_indexed(
        &conn,
        Some(&idx),
        "cache lookup",
        None,
        None,
        20,
        &budget(),
    )
    .expect("query");
    assert_eq!(ids(&fts.sessions), vec![SID_C]);
}

#[test]
fn decisions_scopes_the_index_query_to_the_project_slug() {
    let scratch = Scratch::new("decproj");
    let conn = store(&scratch);
    let idx = index(
        &scratch,
        &[
            (
                SID_A,
                "-home-dev-alpha",
                "cache notes",
                "2026-01-01T09:10:00",
            ),
            (
                SID_B,
                "-somewhere-else",
                "cache notes",
                "2026-01-02T09:10:00",
            ),
        ],
    );
    let scoped = queries::search_past_decisions_indexed(
        &conn,
        Some(&idx),
        "cache",
        Some("-home-dev-alpha"),
        None,
        20,
        &budget(),
    )
    .expect("query");
    assert_eq!(ids(&scoped.sessions), vec![SID_A]);

    // `--project ''` is falsy in Python: no filter at all, not "the empty slug".
    let unscoped = queries::search_past_decisions_indexed(
        &conn,
        Some(&idx),
        "cache",
        Some(""),
        None,
        20,
        &budget(),
    )
    .expect("query");
    assert_eq!(unscoped.sessions.len(), 2, "an empty slug is every project");
}

#[test]
fn a_malformed_since_still_raises_before_the_index_is_consulted() {
    let scratch = Scratch::new("since");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);
    let error = queries::search_past_decisions_indexed(
        &conn,
        Some(&idx),
        "cache",
        None,
        Some("notadate"),
        20,
        &budget(),
    )
    .expect_err("a malformed --since is a ValueError on both paths");
    assert!(error.to_string().contains("since"), "{error}");
}

#[test]
fn an_fts_hit_with_no_store_provenance_is_skipped() {
    let scratch = Scratch::new("drift");
    let conn = store(&scratch);
    let idx = index(
        &scratch,
        &[
            (
                "orphan-session-id",
                "-home-dev-alpha",
                "cache drifted",
                "2026-01-04T09:10:00",
            ),
            (
                SID_A,
                "-home-dev-alpha",
                "we should cache the watermark",
                "2026-01-01T09:10:00",
            ),
        ],
    );
    let result = queries::search_past_decisions_indexed(
        &conn,
        Some(&idx),
        "cache",
        None,
        None,
        20,
        &budget(),
    )
    .expect("query");
    assert_eq!(ids(&result.sessions), vec![SID_A]);
}

// ── touching a file ──────────────────────────────────────────────────────────

#[test]
fn touching_file_ranks_the_exact_tool_half_ahead_of_content_mentions() {
    let scratch = Scratch::new("touch");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);

    let matches = queries::find_sessions_touching_file_indexed(
        &conn,
        Some(&idx),
        "/home/dev/alpha/main.py",
        20_i64,
    )
    .expect("query");

    // A edited the file (tool half); C only mentions it (FTS content half).
    assert_eq!(ids(&matches), vec![SID_A, SID_C]);
}

#[test]
fn the_content_half_is_skipped_once_the_exact_half_fills_the_page() {
    // The perf gate: with `limit = 1` the single tool hit already fills the
    // page, so the second database is never opened and the content mention
    // does not appear — even though it would have on an unbounded query.
    let scratch = Scratch::new("thin");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);

    let capped = queries::find_sessions_touching_file_indexed(
        &conn,
        Some(&idx),
        "/home/dev/alpha/main.py",
        1_i64,
    )
    .expect("query");
    assert_eq!(ids(&capped), vec![SID_A]);

    let unbounded = queries::find_sessions_touching_file_indexed(
        &conn,
        Some(&idx),
        "/home/dev/alpha/main.py",
        0_i64,
    )
    .expect("query");
    assert_eq!(unbounded.len(), 2, "an unbounded limit always consults FTS");
}

#[test]
fn touching_file_budgeted_packs_the_merged_set() {
    let scratch = Scratch::new("touchbudget");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);

    let packed = queries::find_sessions_touching_file_budgeted_indexed(
        &conn,
        Some(&idx),
        "/home/dev/alpha/main.py",
        20_i64,
        &budget(),
    )
    .expect("query");
    assert_eq!(packed.sessions.len(), 2);
    assert!(!packed.truncated);

    // A budget of one row's worth drops the rest and says so.
    let tiny = rank::Budget::at(40, rank::DEFAULT_RANK_WEIGHTS, 1_800_000_000.0);
    let squeezed = queries::find_sessions_touching_file_budgeted_indexed(
        &conn,
        Some(&idx),
        "/home/dev/alpha/main.py",
        20_i64,
        &tiny,
    )
    .expect("query");
    assert!(squeezed.truncated);
    assert!(squeezed.more_available > 0);
}

// ── worked ───────────────────────────────────────────────────────────────────

#[test]
fn worked_unions_the_tool_and_content_halves_and_carries_clustering() {
    let scratch = Scratch::new("worked");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);

    let matches = queries::find_sessions_where_action_worked_indexed(
        &conn,
        Some(&idx),
        "cache",
        &queries::ActionWorked::new(None, None, 20, 0.5),
    )
    .expect("query");

    // A and C both end with an explicit "that worked"; B has no follow-up turn
    // after its anchor, so it classifies as something else and drops out.
    assert_eq!(ids(&matches), vec![SID_C, SID_A]);
    for session_match in &matches {
        let outcome = session_match.outcome.as_ref().expect("an outcome");
        assert_eq!(outcome.outcome, "worked");
    }
}

#[test]
fn worked_falls_back_when_the_index_is_unpopulated() {
    let scratch = Scratch::new("workedfall");
    let conn = store(&scratch);
    let empty = index(&scratch, &[]);

    let routed = queries::find_sessions_where_action_worked_indexed(
        &conn,
        Some(&empty),
        "cache",
        &queries::ActionWorked::new(None, None, 20, 0.5),
    )
    .expect("query");
    let direct = queries::find_sessions_where_action_worked(
        &conn,
        "cache",
        &queries::ActionWorked::new(None, None, 20, 0.5),
    )
    .expect("query");
    assert_eq!(ids(&routed), ids(&direct));
}

#[test]
fn an_empty_action_is_empty_before_the_index_is_touched() {
    let scratch = Scratch::new("workedempty");
    let conn = store(&scratch);
    let idx = mirrored_index(&scratch);
    let matches = queries::find_sessions_where_action_worked_indexed(
        &conn,
        Some(&idx),
        "   ",
        &queries::ActionWorked::new(None, None, 20, 0.5),
    )
    .expect("query");
    assert!(matches.is_empty());
}

// ── the self-contradiction, kept on purpose ──────────────────────────────────

#[test]
fn the_risk_summary_still_counts_with_like_while_the_list_comes_from_bm25() {
    // `memory file` prints `risk.total_sessions` above the session list. The
    // count is a LIKE scan over the *store*; the list is bm25 over the *index*.
    // The two databases hold different text — `content_text` is empty on
    // tool-call turns while the index was built from a fuller extraction — so
    // the report contradicts itself: the verifier measured Python printing
    // "sessions touching the file: 0" directly above twenty of them.
    // Reproduced rather than fixed; it is the current contract (B-1, ledger
    // finding 3) and it is what an agent reading the report sees today.
    let scratch = Scratch::new("contradiction");
    let conn = store(&scratch);
    let path = "/home/dev/alpha/ghost.py";
    let idx = index(
        &scratch,
        &[
            // Indexed prose mentioning a path the store's own text never does.
            (
                SID_B,
                "-home-dev-alpha",
                "poked at /home/dev/alpha/ghost.py by hand",
                "2026-01-02T09:10:00",
            ),
        ],
    );

    let risk = queries::file_risk_summary(&conn, path, None, 5).expect("risk");
    assert_eq!(
        risk.total_sessions, 0,
        "the store's own text never names it"
    );

    let listed = queries::find_sessions_touching_file_indexed(&conn, Some(&idx), path, 20_i64)
        .expect("query");
    assert_eq!(
        ids(&listed),
        vec![SID_B],
        "…and yet the list beneath that zero is not empty"
    );
}

// ── the index handle ─────────────────────────────────────────────────────────

#[test]
fn the_index_path_is_derived_beside_the_store() {
    let derived = LexicalIndex::beside_store(Path::new("/data/su/store.db")).expect("a parent");
    assert_eq!(derived.path(), Path::new("/data/su/search_index.db"));
}
