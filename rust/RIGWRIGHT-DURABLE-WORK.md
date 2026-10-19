# Rigwright task RP-139: optional incremental ETL backfill

Branch trial only. The existing incremental ETL backfill converts messages to
usage events and refreshes the eight reporting marts. The new executable calls
that same implementation and pricing engine through a durable queue. The
existing HTTP handlers, CLI commands and their process-local status stay as they
are. No merge or publication is authorized by this trial.

## Package identity

`stax-server` optionally depends on `deskroost-jobs = "=0.1.0"`, interface
`durable-work-core.v1`. This is an **unreleased pinned candidate**, not a registry
release. [The archive, extracted source, pin and checksum](../vendor/rigwright-durable-work/)
are committed together. Archive SHA-256:

`c780ff030ee306da7fd8bc674aa543cbec01851076ce4991f6d3d5510c538e1c`

Every extracted byte matches the owner-approved 10-member candidate. The candidate
selects no licence: it contains no LICENSE, NOTICE, `license` or `license-file`
metadata. Its implementation, schema and README bytes are unchanged by this
metadata refresh. There are no other Rigwright crates, checkout paths or runtime
services in the dependency graph. The archive's other APIs (product dead letters,
outbound HTTP and reserved product job names) are outside this integration.

The default feature set stays empty. The added executable has
`required-features = ["rigwright-durable-work"]`. Under the qualification's
no-debug build settings, all 14 original executable artifacts were compared to
the original checkout, byte for byte. Existing locked package identities stay
unchanged; the lockfile gains the optional dependency graph.

## Using the branch

Run commands from `rust/`. Supply a dedicated PostgreSQL database through
`STAX_DURABLE_WORK_DATABASE_URL` in the environment; never pass a URL as an
argument. Staxtrace's application store is **SQLite**. PostgreSQL is an additional
service needed only for this opt-in scheduler.

```sh
cargo build --workspace --locked
cargo build --workspace --locked --features rigwright-durable-work
cargo run --locked -p stax-server --features rigwright-durable-work --bin stax-durable-backfill -- init
cargo run --locked -p stax-server --features rigwright-durable-work --bin stax-durable-backfill -- enqueue --store-id example-store --request-id refresh-1
cargo run --locked -p stax-server --features rigwright-durable-work --bin stax-durable-backfill -- work --store-id example-store --data-dir "$STAXTRACE_HOME" --once
```

`init` is a one-time, transactional installation of the archive's exact queue
DDL in the dedicated `staxtrace_durable_work` schema. It does not install any
Rigwright application tables. All pool sessions use that schema and UTC.
Omit `--once` to keep polling; Ctrl-C lets the active attempt finish before exit.
Select the local SQLite store explicitly. All workers sharing a `store-id` must
point to the same logical store; that identity is not a store synchronization
mechanism across hosts.

A request identity deduplicates resubmission even after completion. Use a new
request identity for each new refresh. Keys are consumer-prefixed JSON tuples,
because the primitive's dedupe namespace spans queues. Payloads are JSON objects.

## Concurrency and retry boundary

Claims use the primitive's 60-second token-fenced lease, renewed every 15 seconds.
Failure retries up to three attempts with the primitive's exponential backoff.
An OS file lock beside the selected SQLite store is acquired before claiming and
held by the blocking task until ETL ends. Cooperating local workers cannot
overlap two requests, even if the PostgreSQL lease is lost or the async caller
is cancelled. Original HTTP/CLI backfills and other SQLite writers do not take
this lock and must not be run concurrently with this worker.

SQLite effects and PostgreSQL settlement cannot share a transaction. Delivery is
at least once. The existing unique-event constraint and mart watermarks make
incremental reruns safe; the crash-gap test verifies no duplicate event or daily
mart row after SQLite commits but settlement is lost. `force=true` is unsupported
because its destructive wipe needs a separate retry/concurrency policy.

The primitive's stale-job sweep touches the entire visible queue table, so this
integration isolates it in its own schema. The existing HTTP `202`/`409` and
30-second status slots are not mapped onto durable status in this trial. A future
HTTP adoption needs that mapping, durable result/error inspection, and an explicit
policy for other writers, forced rebuilds and cross-host store identity.
Stale recovery schedules a fixed five-second cooldown; a `work --once` immediately
after expiry can recover the row and return `Idle` before its retry becomes due.

## Build adaptations found in this real consumer

Cargo resolves SQLx 0.8's optional SQLite driver even when only PostgreSQL is
enabled. Its native SQLite library conflicts with staxtrace's existing rusqlite.
[Two manifest-only SQLx patches](../vendor/sqlx-postgres-only/) remove those unused
SQLite resolution edges and upstream test metadata; source and license bytes and
all versions remain unchanged. Upstream author names are retained without contact
addresses in the copied manifests. Source hashes and original registry archive
hashes are recorded in `upstream.json`. The path dependency also declares the two
upstream custom compiler cfg names. SQLite support in these SQLx copies is outside
the supported feature set. A reusable Postgres-only facade that avoids native
SQLite resolution is an extension need for the queue package.

The vendor folder is outside the Rust workspace: the candidate declares its own
workspace, and nesting it inside `rust/` produced Cargo's multiple-workspace-root
error. On macOS 27, the stripped SQLx proc-macro dylib also hit the
[LLVM LINKEDIT padding defect](https://github.com/rust-lang/rust/issues/157750).
A package-specific dev/test override retains line tables and disables stripping
only for `sqlx-macros`; it leaves the candidate and default compiled graph intact.

Existing guide/hooks test fixtures needed local `.git` markers when temporary
directories live under a checkout. Existing query fixtures also needed canonical
paths on macOS, where `/home` resolves through another filesystem path. These
changes are test-only.
FTS and WASM path fixtures use the same canonical spelling too, and the fake
HTTP peer reads complete TCP requests. The lowercase `skill.md` fixture and
Ollama closed-port test were restored exactly from main after both original
tests passed on a case-sensitive Linux filesystem with the Ollama port closed.
The default suite again checks filename casing and includes the closed-port
test without an ignore annotation. Point
`CLAUDE_CONFIG_DIR` at empty scratch when running the suite to keep ingest tests
away from the machine's session history.

## Checks

```sh
python3 -I -B scripts/test_rigwright_durable_work_vendor.py -v  # from repository root
cargo fmt --all -- --check
cargo test --workspace --locked --no-fail-fast -- --test-threads=1
cargo test --workspace --locked --features rigwright-durable-work --no-fail-fast -- --test-threads=1
cargo clippy --workspace --all-targets --locked --features rigwright-durable-work -- -D warnings
```

The five PostgreSQL tests visibly skip when
`STAX_DURABLE_WORK_TEST_DATABASE_URL` is absent. To execute them, set that variable
and `STAX_DURABLE_WORK_TEST_DISPOSABLE=1` in the environment. They require a private
loopback PostgreSQL instance, create an independent schema per test with only the
queue table, and run real ETL against disposable SQLite fixtures. Normal exits
drop the schemas and files; the test runner owns database cleanup after failure.
The test URL is separate from the optional executable's runtime URL.

Earlier qualification on the Mac, 2026-10-19, Rust 1.97.1, before this
candidate refresh and restoration of the original tests:

| Check | Executed result |
| --- | --- |
| Default workspace build | Passed; `Finished dev profile` in 46.16s. |
| Default artifact comparison against the pristine main checkout | `default artifact bytes: 14 unchanged; 0 changed`. |
| Default workspace tests | Aggregate: **2742 passed; 0 failed; 2 ignored**. |
| Feature workspace build | Passed; `Finished dev profile` in 1m 43s. |
| Feature workspace tests, private PostgreSQL available | Aggregate: **2747 passed; 0 failed; 2 ignored**. |
| Five real backfill PostgreSQL tests (included above) | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 20.16s`. |
| Vendor byte/dependency tests | `Ran 3 tests`; `OK`. |
| Formatting and all-target workspace Clippy with `-D warnings` | Passed; Clippy finished in 2m 17s. |

That earlier run ignored the deliberately invoked golden regeneration and the
host-specific Ollama qualification. The PostgreSQL tests also ran separately
without a test URL and emitted all five explicit `SKIP` messages; that run is
skip-behavior evidence only. Browser checks were not run, as required.

All Cargo runs used the same task-owned target directory, `CARGO_INCREMENTAL=0`
and `CARGO_PROFILE_DEV_DEBUG=0`. Temporary files, `STAXTRACE_HOME` and
`CLAUDE_CONFIG_DIR` pointed at task-owned scratch. The byte comparison applies
to these build settings. The private scheduling database used an owned loopback
forward to a disposable PostgreSQL instance; it was dropped and the instance,
volume and forward were removed after execution. No live application store or
production service was used. Disposable Cargo output was removed after the
checks to recover disk space; logs and the original artifact hashes remain in
the qualification's scratch directory.

### Linux qualification after the candidate refresh, 2026-10-19

All Rust builds and tests for this refresh ran on case-sensitive Linux with the
pinned Rust 1.97.1, using the same no-debug, non-incremental settings described
above. The archive hash and every extracted byte were checked again. Both
original test files were tested before restoring them in the adopter clone and
then verified byte-for-byte against main:

```text
test context_budget::tests::skills_need_a_skill_md_and_come_out_in_sorted_order ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 428 filtered out; finished in 0.00s
test routes::misc::tests::every_verb_answers_the_502_because_the_port_is_closed ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 639 filtered out; finished in 0.00s
```

| Check | Executed result |
| --- | --- |
| Default workspace build | `Finished dev profile [unoptimized]` in 32.06s. |
| Default workspace tests | 57 result lines: **2743 passed; 0 failed; 1 ignored**. |
| Feature workspace build | `Finished dev profile [unoptimized]` in 36.27s. |
| Feature workspace tests with private PostgreSQL | 58 result lines: **2748 passed; 0 failed; 1 ignored**. |
| Five PostgreSQL backfill contracts, included above | `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 14.90s`. |
| Vendor byte/dependency tests on the Mac | `Ran 3 tests`; `OK`. The build host's Python 3.8 lacks `tomllib`. |
| Formatting | `cargo fmt --all -- --check`: exit 0. |
| Default all-target workspace Clippy with `-D warnings` | Exit 0; `Finished dev profile [unoptimized]` in 31.86s. |
| Feature all-target workspace Clippy with `-D warnings` | Exit 0; `Finished dev profile [unoptimized]` in 27.45s. |

The feature suite executed all five database contracts with the test URL present
only in the runner's environment; none skipped. The only remaining ignored test
is deliberate golden regeneration. The disposable test databases, private
PostgreSQL container and its anonymous volume were removed after the suite.
Browser checks were not run.
