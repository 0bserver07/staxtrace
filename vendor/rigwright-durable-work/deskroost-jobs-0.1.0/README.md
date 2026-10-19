# deskroost-jobs: the durable job queue

A job queue kept in one PostgreSQL table. Producers enqueue jobs, workers claim them under a lease, and a worker
that lost its lease can no longer settle the job or commit a result with it. Deskroost's workers and the factory
engine run on it; the crate also works by itself, with only its own table.

This page is the contract: what a producer and a worker may rely on. `tests/reliability.rs` and
`tests/standalone.rs` test it.

## What it needs

- PostgreSQL with `jsonb` and `FOR UPDATE SKIP LOCKED`. The tests run on PostgreSQL 17.
- The table `job_queue` and its five indexes: [`schema.sql`](schema.sql). In a Deskroost database the table is
  already there (it is part of the schema that `deskroost-db` installs; a test keeps `schema.sql` identical to
  it). Anywhere else, run `schema.sql` once. Its names are unqualified, so the table lands in the first schema of
  the connection's search path, and the crate's statements find it there.
- `sqlx` 0.8 with Tokio. Every function takes a pool, a connection or a transaction.

Nothing else. Two parts of the crate belong to Deskroost and do not concern another consumer:

- the queue named `agent-jobs` orders a few of Deskroost's job names before the rest (every other queue is first
  come, first served, by `created_at`);
- a job named `observe-chat-message` also updates Deskroost's `message_observations` table when it fails. Do not
  use that name outside Deskroost.

## A job

| Column | Meaning |
|---|---|
| `id` | A random UUID, as text. |
| `queue`, `name` | Where the job waits and what it is. A worker claims from one queue. |
| `data` | The payload, any JSON. |
| `org_id` | Optional owner. `enqueue` takes it from `data.orgId` or `data.org_id` unless the options name it. |
| `status` | `pending`, `running`, `completed` or `failed`. |
| `attempts`, `max_attempts` | Claims so far, and how many are allowed (default 3). |
| `run_at` | The job is not claimed before this time. |
| `locked_by`, `lock_token`, `lock_expires_at` | The claim: who holds it, the fencing token and when the lease ends. Empty unless `running`. |
| `dedupe_key` | Optional. At most one job per owner and key exists at a time. |
| `cron_key` | Optional. At most one `pending` or `running` job per key. |
| `error` | The last failure. |

## Enqueue

`enqueue(executor, queue, name, &data, &options)` inserts a `pending` job. Pass a transaction as the executor to
queue the job together with your own write: it becomes visible to workers when you commit, and not at all if you
roll back.

- `delay_ms` sets `run_at` later.
- `max_attempts` defaults to 3.
- `dedupe_key`: a second job with the same owner and key is not inserted, whatever the state of the first, until
  the first is pruned. `enqueue_or_rearm_failed` queues such a job or, when the earlier one has failed for good,
  makes that one `pending` again with fresh attempts.
- `ensure_cron_job` registers a recurring job: it inserts nothing while a job with that `cron_key` is `pending`
  or `running`. The consumer registers the next occurrence when it finishes one.

## Claim

`dequeue_job(pool, queue, &options)` takes the oldest `pending` job of the queue whose `run_at` has passed and
makes it `running`: `attempts` goes up by one, and the job gets the worker's name (`locked_by`), a new random
`lock_token` and a lease that ends `lease_ms` from now (default 60 seconds). Two workers never get the same job:
the claim is one statement with `FOR UPDATE SKIP LOCKED`. The options can narrow the claim to an owner, a job
name, a list of names or one field of the payload.

The **lock token is a fencing token**. Every later call names the job and the token, and acts only if the job is
still `running` with that token and its lease has not ended. A call that returns `false` means the lease is gone:
the worker must stop and throw its result away.

- `renew_job_lease(pool, id, token, lease_ms)` extends the lease. Renew well before it ends (the factory worker
  renews every third of the lease).
- `complete_job(pool, id, token)` marks the job `completed`.
- `complete_job_on(executor, id, token)` does the same on your connection or transaction. Write your result and
  call it in one transaction, and commit only if it returned `true`: the result is then stored exactly when the
  job is settled, and a worker that lost its lease stores nothing.
- `fail_job(pool, id, token, error, &options)` records the error. While attempts remain, the job is `pending`
  again after a delay of 1 second doubling per attempt, at most 60 seconds; after `max_attempts`, or with
  `terminal: true`, it is `failed`.
- `defer_job(pool, id, token, error, delay_ms)` puts the job back to `pending` without using up an attempt (at
  least 1 second later).

## Recovery

A worker that dies or stalls keeps its job `running` until the lease ends. `cleanup_stale_jobs(pool)` then gives
every such job back: `pending` again 5 seconds later if attempts remain, else `failed`, with the error
`stale: worker lease expired before settlement`. Run it on a timer (Deskroost's workers do).

So delivery is **at least once**. A job can run again after its first worker lost the lease, and the first worker
may still be running when the second starts. What the queue guarantees is that the first worker cannot settle the
job or commit through `complete_job_on` any more. Effects outside the database (a request sent, a file written)
are not undone: make them idempotent, or check the lease (`renew_job_lease`) before each one.

## Clocks

Leases, due times and retry delays are compared with the database's `now()`, in the statement that acts on them.
Workers' clocks do not matter. The columns are `timestamp without time zone`: keep every session of the database
in one time zone. The one exception is `prune_finished_jobs(pool, retention_ms)`, which removes `completed` and
`failed` jobs older than the retention (default 7 days) by the worker's clock.

## Waking workers

A worker polls. For Deskroost's jobs that a person waits for, the insert also sends a notification on the
channel `deskroost_job_wake` (payload: the queue), delivered when the insert commits. A consumer may `LISTEN` on
it; other jobs are found at the next poll.

## Health

`health::get_queue_health_snapshot` counts jobs by status and reports the age of the oldest job that is due, leases that
ended without settlement and recent failures, from `job_queue` alone.

## Tests

```sh
# The schema file equals the product's table (no database needed).
cargo test -p deskroost-jobs --test standalone the_schema_file

# The queue with only its own table, in a schema of its own that the test removes.
DESKROOST_TEST_DISPOSABLE=1 DESKROOST_TEST_DATABASE_URL=postgres://... \
  cargo test -p deskroost-jobs --test standalone -- --ignored

# The reliability contracts on a Deskroost database (see tests/reliability.rs).
DESKROOST_TEST_DISPOSABLE=1 DESKROOST_TEST_DATABASE_URL=postgres://... \
  cargo test -p deskroost-jobs --test reliability -- --ignored --test-threads=1
```

`standalone` enqueues with a dedupe key, claims, renews, lets the lease run out, shows that the stale holder can
neither renew, settle nor commit a result, recovers the job, settles it from a second worker together with that
worker's own write, retries a failing job to its last attempt, registers a recurring job once and prunes.
