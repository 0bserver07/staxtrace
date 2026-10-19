//! The service layer — `stackunderflow/services/*.py` and `reports/*.py`.
//!
//! # Why this directory exists
//!
//! Wave 5's first two endpoint batches ported the routes that *are* their
//! handlers: `routes/cost.py` and `routes/projects.py` carry their own SQL and
//! their own arithmetic, so `routes/cost.rs` carries it too and the file map is
//! one-to-one. The seven modules batch C inherited are the opposite shape —
//! each is a thin HTTP wrapper (55–405 lines) over a large unported service
//! (262–871 lines) that the CLI calls through the *same* function. `/api/compare`
//! and `stax compare` share `services/compare.py`; `/api/export` and
//! `stax export` share `reports/export.py::run_export`.
//!
//! Transliterating that logic into the route module would fork it: wave 8 ports
//! the CLI verbs, finds no shared home, and writes a second copy that drifts.
//! So the split Python already has is the split reproduced here — the route
//! module parses query parameters, validates them, calls one function in this
//! directory, and stamps currency. Everything else lives here.
//!
//! # Ownership
//!
//! Batch C owns this directory and the single `mod services;` line in
//! [`crate`]'s root. Nothing else in `lib.rs` was touched, and `routes/mod.rs`
//! was not touched at all — the 34 slots were already wired by the wave-5 lead.
//!
//! # The module map
//!
//! | Module | Python source | Consumed by |
//! |---|---|---|
//! | [`aggregate`] | `reports/aggregate.py` | `routes/plan.rs`, [`export`] |
//! | [`anomaly`] | `reports/anomaly.py` | `routes/optimize.rs` |
//! | [`burn`] | `services/burn.py` | `routes/plan.rs` |
//! | [`compare`] | `services/compare.py` | `routes/compare.rs` |
//! | [`context_budget`] | `services/context_budget.py` | `routes/context_budget.rs`, [`prescribe`] |
//! | [`context_replay`] | `services/context_replay.py` | `routes/context_replay.rs` |
//! | [`export`] | `reports/export.py` | `routes/export.rs` |
//! | [`json_error`] | `Lib/json/decoder.py` (error path) | `routes/data.rs` |
//! | [`mart_queries`] | `store/mart_queries.py` | `routes/data.rs`, [`optimize`] |
//! | [`messages`] | `api/messages.py` | `routes/data.rs` |
//! | [`mode_recommender`] | `services/mode_recommender.py` | [`prescribe`] |
//! | [`optimize`] | `reports/optimize.py` | `routes/optimize.rs` |
//! | [`outcome_attribution`] | `services/outcome_attribution.py` | `routes/yield_route.rs` |
//! | [`plans`] | `services/plans.py` | `routes/plan.rs` |
//! | [`prescribe`] | `reports/prescribe.py` | `routes/optimize.rs` |
//! | [`scope`] | `reports/scope.py` | [`optimize`], [`prescribe`], [`export`], `routes/plan.rs` |
//! | [`yield_tracker`] | `services/yield_tracker.py` | `routes/yield_route.rs` |

pub mod json_error;
pub mod messages;

// ── batch E: the deferred-endpoint remainder ─────────────────────────────────
//
// | Module | Python source | Consumed by |
// |---|---|---|
// | [`agent_teams`] | `services/agent_teams.py` | `routes/agent_teams.rs` |
// | [`benchmark`] | `reports/benchmark.py` | `routes/benchmark.rs` |
// | [`benchmark_stats`] | `services/benchmark_stats.py` | [`benchmark`] |
// | [`etl_backfill`] | `etl/backfill_jobs.py`, plus re-exports of `etl/backfill.py` | `routes/etl.rs` |
// | [`forks`] | `reports/forks.py` | `routes/forks.rs` |
// | [`grading`] | `services/grading.py` | `routes/quality.rs` |
// | [`live`] | `services/live.py` | `routes/live.rs` |
// | [`ollama_proxy`] | `routes/misc.py::ollama_proxy` | `routes/misc.rs` |
// | [`patterns`] | `reports/patterns.py` | `routes/patterns.rs` |
// | [`playback`] | `services/playback.py` | `routes/playback.rs` |
// | [`playback_fs`] | `services/playback_fs.py` | `routes/playback.rs` |
// | [`pricing_refresh`] | `routes/misc.py::refresh_pricing` | `routes/misc.rs` |
// | [`risk`] | `services/risk.py` | [`playback`] |
// | [`session_compare`] | `routes/sessions.py::compare_sessions` | `routes/sessions.rs` |
// | [`worktrees`] | `services/worktrees.py` | `routes/worktrees.rs` |
//
// The module list is declared by the batch-E integrator up front so that the
// parallel members never contend on this file — the wave-5 lesson that a shared
// registration point is a merge conflict waiting to happen (`routes/mod.rs` was
// pre-wired for exactly this reason).
pub mod agent_teams;
pub mod etl_backfill;
pub mod live;
pub mod ollama_proxy;
pub mod playback;
pub mod playback_fs;
pub mod pricing_refresh;
pub mod session_compare;
pub mod static_analysis;

// ── the rulings pass: `routes/cost.py`'s cross-request memo ───────────────────
//
// | Module | Python source | Consumed by |
// |---|---|---|
// | [`stats_memo`] | `routes/cost.py::_project_stats_cached` | `routes/data.rs`, `routes/cost.rs`, `routes/commands.rs` |
//
// It lives here rather than in `routes/cost.rs` for the reason Python's
// docstring gives for its own placement being awkward: three route modules share
// it, and `routes/data.py` already has to import it back out of `cost.py`
// (`data.py` line 29) because putting it the other way round would cycle. A
// service module has no such problem.
pub mod stats_memo;

// ── wave 8 tranche 3: the report layer moved out, and the paths did not ───────
//
// Twenty-two of the modules above are now `stax-reports`'s. The reason is in
// that crate's `lib.rs`: this directory was the shared home for logic `/api/…`
// and `stackunderflow …` both call, and it was the RIGHT split at the wrong
// address — living inside the HTTP crate meant `stax-cli` had to link `axum`
// and `tokio` to print a spend line (tranche 1 filed exactly that deviation).
//
// They are re-exported here, one name per line, rather than being reached for
// as `stax_reports::…` at each call site. That is deliberate and it is the
// whole reason the move was cheap: `crate::services::optimize::round_half_even`,
// `super::scope::Scope`, `crate::services::mart_queries::table_exists` — every
// path already written in `routes/` and in the eleven modules that stayed
// resolves to the same item it did before, so the split touched **no route
// module** and no handler. A `pub use` list is also honest about the ownership:
// these are consumed here, not owned here.
//
// | Module | Python source | Consumed by |
// |---|---|---|
// | [`aggregate`] | `reports/aggregate.py` | `routes/plan.rs`, [`export`], `stax-cli` |
// | [`anomaly`] | `reports/anomaly.py` | `routes/optimize.rs`, [`benchmark_stats`] |
// | [`benchmark`] | `reports/benchmark.py` | `routes/benchmark.rs`, `stax-cli` |
// | [`benchmark_stats`] | `services/benchmark_stats.py` | [`benchmark`] |
// | [`burn`] | `services/burn.py` | `routes/plan.rs`, `stax-cli` |
// | [`compare`] | `services/compare.py` | `routes/compare.rs`, `stax-cli` |
// | [`context_budget`] | `services/context_budget.py` | `routes/context_budget.rs`, [`prescribe`], `stax-cli` |
// | [`context_replay`] | `services/context_replay.py` | `routes/context_replay.rs`, `stax-cli` |
// | [`export`] | `reports/export.py` | `routes/export.rs`, `stax-cli` |
// | [`forks`] | `reports/forks.py` | `routes/forks.rs` |
// | [`grading`] | `services/grading.py` | `routes/quality.rs`, `stax-cli` |
// | [`mart_queries`] | `store/mart_queries.py` | `routes/data.rs`, [`optimize`], [`live`], [`playback`] |
// | [`mode_recommender`] | `services/mode_recommender.py` | [`prescribe`] |
// | [`optimize`] | `reports/optimize.py` | `routes/optimize.rs`, `stax-cli` |
// | [`outcome_attribution`] | `services/outcome_attribution.py` | `routes/yield_route.rs` |
// | [`patterns`] | `reports/patterns.py` | `routes/patterns.rs`, `stax-cli` |
// | [`plans`] | `services/plans.py` | `routes/plan.rs`, [`live`], `stax-cli` |
// | [`prescribe`] | `reports/prescribe.py` | `routes/optimize.rs` |
// | [`risk`] | `services/risk.py` | [`playback`], `stax-cli` |
// | [`scope`] | `reports/scope.py` | [`optimize`], [`prescribe`], [`export`], `routes/plan.rs`, `stax-cli` |
// | [`worktrees`] | `services/worktrees.py` | `routes/worktrees.rs`, `stax-cli` |
// | [`yield_tracker`] | `services/yield_tracker.py` | `routes/yield_route.rs`, `stax-cli` |
pub use stax_reports::{
    aggregate, anomaly, benchmark, benchmark_stats, burn, compare, context_budget, context_replay,
    export, forks, grading, mart_queries, mode_recommender, optimize, outcome_attribution,
    patterns, plans, prescribe, risk, scope, worktrees, yield_tracker,
};

// ── T2v3: `etl/status.py` joined them, for the same reason ───────────────────
//
// `cli.py`'s `etl status` verb calls `etl.status.assemble_status`, so the
// assembler needed a home `stax-cli` can reach. It is `stax_reports::etl_status`
// now, and this line keeps `crate::services::etl_status::assemble_status`
// resolving in `routes/etl.rs`. One behavioural change travelled with it and is
// visible at the call site: the two process-local job slots are PARAMETERS,
// because a CLI process has no slots to read.
pub use stax_reports::etl_status;

#[cfg(feature = "rigwright-durable-work")]
pub mod durable_backfill;
