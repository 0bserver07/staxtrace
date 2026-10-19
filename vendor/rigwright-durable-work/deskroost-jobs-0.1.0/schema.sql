-- The one table the durable job queue of `deskroost-jobs` needs, with its indexes. Names are unqualified: the
-- table is created in the first schema of the connection's search path. It is the `job_queue` table of the
-- Deskroost schema (crates/deskroost-db/migrations/0001_upstream_fresh.sql), which a test of this crate compares
-- with this file, so the two cannot drift. See README.md for the contract.

CREATE TABLE job_queue (
    id text NOT NULL,
    org_id text,
    queue text NOT NULL,
    name text NOT NULL,
    data jsonb NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    max_attempts integer DEFAULT 3 NOT NULL,
    run_at timestamp without time zone DEFAULT now() NOT NULL,
    started_at timestamp without time zone,
    completed_at timestamp without time zone,
    error text,
    cron_key text,
    dedupe_key text,
    locked_by text,
    lock_token text,
    lock_expires_at timestamp without time zone,
    created_at timestamp without time zone DEFAULT now() NOT NULL
);

ALTER TABLE ONLY job_queue
    ADD CONSTRAINT job_queue_pkey PRIMARY KEY (id);

CREATE UNIQUE INDEX job_queue_active_cron_unique ON job_queue USING btree (cron_key) WHERE ((cron_key IS NOT NULL) AND (status = ANY (ARRAY['pending'::text, 'running'::text])));

CREATE UNIQUE INDEX job_queue_dedupe_unique ON job_queue USING btree (COALESCE(org_id, ''::text), dedupe_key) WHERE (dedupe_key IS NOT NULL);

CREATE INDEX job_queue_lease_idx ON job_queue USING btree (status, lock_expires_at);

CREATE INDEX job_queue_org_idx ON job_queue USING btree (org_id);

CREATE INDEX job_queue_poll_idx ON job_queue USING btree (status, queue, run_at);
