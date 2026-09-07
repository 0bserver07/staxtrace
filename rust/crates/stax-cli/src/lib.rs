//! The command surface: clap, and eventually the 79 commands Python exposes.
//!
//! Charter (`docs/specs/rust-port.md` §3): port `cli.py` — the same command
//! names, the same flags, the same output shapes, so a script written against
//! the Python CLI keeps working when `stax` replaces it. Wave 8 owns the long
//! tail and the `--help`-tree diff; wave 0 ships exactly one command, `store` (né `status` — DIV-025 ruling),
//! whose entire job is to be checkable against Python on the real store.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// `stax ingest` — the PR/CI receiver group (`cli.py`'s `ingest`). Appended for
/// the same lib.rs-law reason as `msg` above.
mod advanced;
mod analyze;
mod anchor;
mod ask;
mod audit;
mod backup;
mod benchmark;
mod cache;
mod cfg;
mod click;
mod clickx;
mod compare;
mod context_replay;
mod discovery;
mod docs;
mod doctor;
mod embeddings;
mod export;
mod guide;
mod hooks;
mod ingest;
mod init;
mod memory;
pub mod mode_rec;
/// `stax msg` — the agent telephone (`cli.py`'s `msg` group).
///
/// Appended rather than filed alphabetically: three agents edit this file
/// concurrently, and the lib.rs law is add-only lines, so a new module goes
/// where no other agent's insertion point can collide with it.
mod msg;
mod optimize;
mod plan;
mod pyclock;
mod recommend;
mod reports;
mod resume;
mod risk;
pub mod settings;
pub mod skill_rec;
pub mod skill_synth;
mod skills;
mod spend;
mod start;
mod status;
mod store;
mod sync;
mod worktrees;
// ── T2v3 (this leg) — appended, never interleaved: the LIB.RS LAW ────────────
mod discovery_telemetry;
mod etl;
mod memory_embed;
mod pricing;
mod reindex;
// ── RS-8-101 (the import leg) — appended at the tail, never interleaved ──────
/// `stax import` — the history-plugin verb. Named `import_history` rather than
/// `import` so the module name matches `cli.py`'s function
/// (`import_history_cmd`) and reads unambiguously beside `use`.
mod import_history;
// ── agent-remotes Phases 1+2 — appended at the tail, per the lib.rs law ──────
mod observe;
mod remote;
// ── track1-reposition — appended at the tail, per the lib.rs law ─────────────
mod audit_transcripts;
mod doctor_install;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

pub use anchor::{AnchorArgs, AnchorCommand, run_anchor};
pub use ask::{hybrid_env_from_process, run_ask};
pub use backup::{
    BackupArgs, BackupVerb, create_rsync_argv, cron_line, darwin_plist, launchctl_argv,
    restore_rsync_argv, run_backup, sanitise_label,
};
pub use benchmark::{BenchmarkArgs, BenchmarkVerb, run_benchmark};
pub use cache::{ClearCacheArgs, run_clear_cache};
pub use cfg::{CfgArgs, CfgVerb, ConfigArgs, ConfigVerb, ModelAliasVerb, run_cfg, run_config};
pub use click::{Output, UsageError};
pub use compare::{CompareArgs, render_compare_table, run_compare, sort_keys};
pub use context_replay::{ContextReplayArgs, run_context_replay};
pub use discovery::{
    ActionWorkedArgs, FailureModesArgs, InPathArgs, PastDecisionsArgs, TouchingFileArgs,
    run_action_worked, run_failure_modes, run_in_path, run_past_decisions, run_touching_file,
};
pub use docs::{DocsArgs, DocsVerb, render_markdown, run_docs, run_docs_with};
// `DoctorArgs` is aliased because `pricing doctor` (T2v3, appended below)
// exports a struct of the same name for a different command — the
// `ListArgs as WorktreesListArgs` precedent. Renaming theirs would be an edit
// to a line this leg does not own.
pub use doctor::{
    Delivery, DoctorArgs as StoreDoctorArgs, Finding, Health, ProviderRow, enumerate_disk,
    exempt_providers, render_doctor_json, render_doctor_text, run_delivery_checks, run_doctor,
    run_store_health_checks,
};
pub use export::{ExportArgs, run_export_cmd};
pub use guide::{GuideArgs, GuideVerb, run_guide};
pub use hooks::{HooksArgs, HooksVerb, run_hooks};
pub use ingest::{
    GITHUB_API_BASE, IngestArgs, IngestVerb, MAX_PAGES_RANGE, MAX_PER_PAGE, STATE_CHOICES,
    ServeArgs, WebhookArgs, WebhookVerb, auth_headers, ci_url, is_last_page, page_params,
    pr_extra_params, pr_url, rate_limit_message, run_ingest, serve_banner,
};
pub use init::{
    InitArgs, SkillsReport, default_skills_dest, install_static_skills, render_report, run_init,
    shipped_skills_source_dir,
};
pub use memory::{MemoryArgs, MemoryVerb, run_memory};
pub use msg::{
    MsgArgs, MsgInboxArgs, MsgSendArgs, MsgVerb, message_payload_now, run_inbox, run_msg, run_send,
    strftime_local,
};
pub use optimize::{OptimizeArgs, qa_db_path, run_optimize};
pub use plan::{PlanArgs, PlanSetArgs, PlanVerb, ThresholdsVerb, format_money, run_plan};
pub use recommend::{ModeArgs, RecommendArgs, RecommendSkillsArgs, RecommendVerb, run_recommend};
pub use reports::{IngestFlags, PeriodArgs, ReportArgs, run_month, run_report, run_today};
pub use resume::{ResumeArgs, ResumeEnv, run_resume};
pub use risk::{RiskArgs, RiskFileArgs, RiskVerb, render_risk_text, run_risk};
pub use skills::{
    CleanArgs, GenerateArgs, ListArgs, SkillsArgs, SkillsEnv, SkillsVerb, run_skills,
    run_skills_with,
};
pub use spend::{ContextBudgetArgs, YieldArgs, run_context_budget, run_yield};
pub use start::{
    StartArgs, dashboard_url, exposure_warning, is_loopback, resolve_host, resolve_port, run_start,
    run_start_with,
};
pub use status::{StatusArgs, run_status};
pub use store::{StoreArgs, render_store, run_store};
pub use sync::{SyncArgs, SyncInitArgs, SyncJsonArgs, SyncVerb, run_sync};
pub use worktrees::{
    ListArgs as WorktreesListArgs, WorktreesArgs, WorktreesVerb, render_worktrees_text,
    run_worktrees, short_worktree_path,
};
// ── T2v3 (this leg) ─────────────────────────────────────────────────────────
pub use discovery_telemetry::{
    DemoteArgs, DiscoveryArgs, DiscoveryVerb, TelemetryArgs, TelemetryRow, demote_candidates,
    iter_telemetry, mark_demoted, render_demote_text, render_telemetry_text, run_discovery,
};
pub use etl::{
    BackfillArgs as EtlBackfillArgs, EtlArgs, EtlVerb, StatusArgs as EtlStatusArgs,
    render_etl_status_text, run_etl,
};
pub use memory_embed::{EmbedArgs, embed_new_messages, pack_vector, run_memory_embed};
pub use pricing::{DoctorArgs, PricingArgs, PricingVerb, render_pricing_doctor_text, run_pricing};
pub use reindex::{ReindexArgs, render_counts, run_reindex};
// ── RS-8-101 (the import leg) ────────────────────────────────────────────────
pub use import_history::{
    HISTORY_PLUGIN_DIRNAME, ImportArgs, render_text as render_import_text, result_payload,
    run_import, search_roots,
};

/// `stax` — the Rust port of StackUnderflow.
///
/// `about` is set explicitly rather than taken from this doc comment: the
/// `--help`-tree differ compares the root summary against Click's, and Click
/// prints `cli.__doc__`. Matching it is drop-in parity (the P0 directive); the
/// string is the maintainer's to change, in `cli.py` first.
#[derive(Debug, Parser)]
#[command(
    name = "stax",
    version,
    about = "staxtrace — a local-first knowledge base for your AI coding sessions.",
    long_about = None
)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Every command `stax` understands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Keyed, append-only campaign state that survives a context rotation.
    #[command(hide = true)]
    Anchor(AnchorArgs),
    /// Which coding agents can send your code or telemetry off-box — and the
    /// exact line that stops each one (Spec 28, config audit).
    Audit(crate::audit::AuditArgs),
    /// What your coding agents cost you — dashboard-style summary over a date range.
    Cost(ReportArgs),
    /// Reconstruct what the model "saw" in SESSION_ID up to a --at seq (session replay).
    Replay(ContextReplayArgs),
    /// Back up and restore session data from every registered coding agent.
    #[command(hide = true)]
    Backup(BackupArgs),
    /// Which model wins for the kind of work you actually do.
    ///
    /// An observational benchmark over your own history — a natural experiment you
    /// already ran, not live replay. Every verdict carries n, coverage, confidence
    /// intervals and a ``confidence`` label, and says "insufficient evidence"
    /// rather than guess. Run any subcommand with ``--json`` for the stable,
    /// token-bounded agent-output envelope.
    #[command(hide = true)]
    Benchmark(BenchmarkArgs),
    /// View or change persistent settings.
    #[command(hide = true)]
    Cfg(CfgArgs),
    /// Clear cached data.  Use ``start --fresh`` for a clean boot.
    #[command(name = "clear-cache")]
    #[command(hide = true)]
    ClearCache(ClearCacheArgs),
    /// Compare per-model metrics side-by-side over a window.
    ///
    /// Renders one row per model with sessions, calls, one-shot %, retry
    /// rate, cache hit %, $/call, $/session, and total $.
    #[command(hide = true)]
    Compare(CompareArgs),
    /// The pre-`cfg` spelling, kept working and kept out of every listing.
    ///
    /// `about = ""` is deliberate and is a parity fact, not an oversight:
    /// `config_compat` and its three subcommands have **no docstring** in
    /// `cli.py`, so Click prints no summary at all. A helpful Rust summary here
    /// would be the only text in the tree the reference does not have.
    #[command(hide = true, about = "", long_about = None)]
    Config(ConfigArgs),
    /// Read staxtrace's own docs, offline from the installed package.
    #[command(hide = true)]
    Docs(DocsArgs),
    /// Read-only health + delivery check of the local store.
    ///
    /// Health: SQLite integrity + foreign-key checks plus watermark/orphan
    /// sanity, opening the store read-only (never migrates or writes).
    ///
    /// Delivery: the per-provider scoreboard (disk sessions → base messages →
    /// usage_events → marts) that catches data loading but never reaching the
    /// dashboard. Exit is non-zero on health findings always, and on delivery
    /// gaps only with ``--fail-on-gap``.
    Doctor(StoreDoctorArgs),
    /// Export aggregated usage data to a CSV or JSON file.
    ///
    /// With ``--period`` set, exports a single window. Without it, exports
    /// a multi-period rollup (today / last 7 days / last 30 days) so a JSON
    /// consumer never has to make three CLI calls. CSV always lays out
    /// one section per period in the same file, separated by a blank line.
    #[command(hide = true)]
    Export(ExportArgs),
    /// List sessions where editing FILE led to a follow-up correction.
    ///
    /// Surfaces the sessions where a past edit to FILE was followed by the
    /// user reporting it broke, the agent reverting it (``git revert`` /
    /// ``git reset --hard`` / ``git checkout --``), or a complaint — each
    /// with the evidence (the triggering message) plus an
    /// ``outcome_confidence`` in [0.0, 1.0]. Rows below ``--min-confidence``
    /// (default 0.5) are filtered out. The companion of
    /// ``find-sessions-where-action-worked``: use this to learn why an edit
    /// went wrong, that one to learn how a successful change was done.
    #[command(hide = true)]
    FindFailureModesForFile(FailureModesArgs),
    /// List sessions whose project root is PATH or any ancestor of PATH.
    ///
    /// Useful when an agent is working in /a/b/c and wants to know what
    /// has happened in the project rooted at /a/b. The match is
    /// ancestor-only — projects rooted *below* PATH do not match.
    #[command(hide = true)]
    FindSessionsInPath(InPathArgs),
    /// List sessions where FILE shows up in tool calls or message text.
    #[command(hide = true)]
    FindSessionsTouchingFile(TouchingFileArgs),
    /// List sessions where ACTION was performed and the next user turn confirmed it worked.
    ///
    /// ACTION is matched as a substring against tool calls and message text,
    /// so it can be a tool name ("Edit"), a file fragment ("cost.py"), or a
    /// phrase from the conversation ("add caching"). For each session the
    /// *last* matching message is the anchor; the outcome is inferred from
    /// the following user turns (an explicit "thanks"/"that worked", an
    /// agent revert command, or — at lower confidence — no signal at all
    /// before the session ended). Each row carries an ``outcome_confidence``
    /// in [0.0, 1.0]; rows below ``--min-confidence`` (default 0.5) are
    /// filtered out. Pair with ``find-failure-modes-for-file`` to see where
    /// an edit went wrong.
    #[command(hide = true)]
    FindSessionsWhereActionWorked(ActionWorkedArgs),
    /// Manage the staxtrace agent-discovery snippet in CLAUDE.md / AGENTS.md.
    #[command(hide = true)]
    Guide(GuideArgs),
    /// Manage opt-in Claude Code lifecycle hooks (hybrid capture).
    #[command(hide = true)]
    Hooks(HooksArgs),
    /// Start the dashboard (alias for ``start``).
    ///
    /// With ``--install-skills``, copies the three shipped Claude Code
    /// ``SKILL.md`` files into ``~/.claude/skills/`` (or ``--skills-dest``)
    /// before starting the dashboard. See ``docs/skills.md``.
    #[command(hide = true)]
    Init(InitArgs),
    /// Ask the local store what past sessions already know.
    ///
    /// ``memory`` is the agent-facing namespace: one set of commands, one
    /// output contract. Run any subcommand with ``--json`` to get the stable,
    /// token-bounded agent-output envelope an agent can splice straight into
    /// its context window; without it you get a human-readable summary.
    ///
    /// Every subcommand shares ``--format`` / ``--json``, ``--project``,
    /// ``--since``, ``--limit`` and ``--context-budget``. ``--project``
    /// defaults to the current directory's project when staxtrace
    /// recognises it, so these commands Just Work when run inside a repo.
    #[command(hide = true)]
    Memory(MemoryArgs),
    /// Estimate the per-session context tax (system prompt + MCP + skills + memory).
    #[command(name = "context-budget")]
    #[command(hide = true)]
    ContextBudget(ContextBudgetArgs),
    /// Reconstruct what the model "saw" in SESSION_ID up to a --at seq.
    ///
    /// Returns the ordered message sequence (role, preview, tool calls, per-turn
    /// token estimate) with a running token total, so you can watch the context
    /// grow. Read-only and advisory: an unknown session yields an empty result,
    /// never an error. MVP semantics = the session's message sequence up to --at
    /// (harness-side context eviction is a future refinement).
    #[command(name = "context-replay")]
    #[command(hide = true)]
    ContextReplay(ContextReplayArgs),
    /// This month's usage.
    #[command(hide = true)]
    Month(PeriodArgs),
    /// Find wasted spend: looped Q&A pairs plus seven structural waste patterns.
    ///
    /// The legacy ``waste`` block lists projects where the assistant had to
    /// retry repeatedly. The ``patterns`` block surfaces structural waste
    /// detected from filesystem state and tool-call history (bloated
    /// CLAUDE.md, unused MCP servers, ghost agents, junk reads, cache
    /// thrash, oversized bash output, exploration-only sessions).
    #[command(hide = true)]
    Optimize(OptimizeArgs),
    /// Manage and inspect a monthly plan budget (Claude Pro, Cursor Pro, custom).
    #[command(hide = true)]
    Plan(PlanArgs),
    /// Proactive recommendations mined from your local session store.
    ///
    /// Recommendations are read-only — accepting one is always a separate
    /// explicit step (e.g. ``stax skills generate --pattern <id>``).
    #[command(hide = true)]
    Recommend(RecommendArgs),
    /// Dashboard-style summary over a date range.
    #[command(hide = true)]
    Report(ReportArgs),
    /// Session/resume ids for every coding agent under PATH (default: cwd).
    ///
    /// Groups recent sessions by provider and renders each agent's real resume
    /// invocation (templates are data in ``adapters/capabilities.json``, verified
    /// against the actual CLIs — e.g. ``claude --resume <id>``, ``codex resume
    /// <id>``). Matching is bidirectional: standing inside a project finds it,
    /// and giving a workspace folder lists every project underneath. Read-only;
    /// agents whose CLI has no known resume command still list their session
    /// ids.
    #[command(hide = true)]
    Resume(ResumeArgs),
    /// Surface "this file has caused N reverts in M days" before editing it.
    ///
    /// Read-only aggregator over the v0.7.2 outcome heuristic. No new
    /// schema; counts are computed from existing ``messages`` / ``sessions``
    /// rows on each call.
    #[command(hide = true)]
    Risk(RiskArgs),
    /// Substring-search QUERY across past message content; return matching
    /// sessions.
    #[command(hide = true)]
    SearchPastDecisions(PastDecisionsArgs),
    /// Generate / list / clean project-specific Claude Code skills.
    ///
    /// These are mined from your local session store — never from CLAUDE.md
    /// or memory — and are always project-scoped unless you ask otherwise.
    #[command(hide = true)]
    Skills(SkillsArgs),
    /// Launch the staxtrace dashboard.
    Start(StartArgs),
    /// Compact one-liner: today + month cost and message counts.
    #[command(hide = true)]
    Status(StatusArgs),
    /// Open the store read-only and print its schema version and row counts.
    #[command(hide = true)]
    Store(StoreArgs),
    /// Encrypted, bring-your-own-bucket backup of your analytics aggregates
    /// (opt-in).
    #[command(hide = true)]
    Sync(SyncArgs),
    /// Today's usage.
    #[command(hide = true)]
    Today(PeriodArgs),
    /// Yield analysis: productive vs reverted vs abandoned sessions.
    #[command(name = "yield")]
    #[command(hide = true)]
    Yield(YieldArgs),
    /// Inspect git worktrees: owner project, cost, prune safety (read-only).
    #[command(hide = true)]
    Worktrees(WorktreesArgs),
    // ── T2v3 (this leg) — appended at the tail, never interleaved ────────────
    /// Inspect / maintain the discovery citation-feedback telemetry.
    #[command(hide = true)]
    Discovery(DiscoveryArgs),
    /// Run the ETL pipeline (raw messages → events → marts).
    #[command(hide = true)]
    Etl(EtlArgs),
    /// Inspect model pricing health (read-only).
    #[command(hide = true)]
    Pricing(PricingArgs),
    /// Rebuild the session store from scratch.
    #[command(hide = true)]
    Reindex(ReindexArgs),
    // ── TELEPHONE (this leg) — appended at the tail, never interleaved ───────
    /// Agent telephone — leave word for another machine's agents (and read
    /// yours).
    ///
    /// Store-and-forward, not chat: `msg send` writes one small JSON file into
    /// the RECIPIENT's data dir over ssh (same transport as `sync`); the
    /// recipient's injection hooks surface unseen messages into the next live
    /// agent turn (UserPromptSubmit / PreToolUse), exactly once. No broker, no
    /// daemon.
    #[command(hide = true)]
    Msg(MsgArgs),
    /// Pull PR / CI data into the local store (REST backfill + webhook
    /// receiver).
    ///
    /// The `about` attribute below WINS over this doc comment for clap, and
    /// that is the point: this text is for `cargo doc`, the attribute is for
    /// `--help`. Same split the `Config` variant uses.
    ///
    // Only `webhook serve` is registered. `ingest github` needs a TLS client
    // (DIV-199, an open architect manifest decision) and this campaign's brief
    // forbids live network, so it is ABSENT rather than stubbed — see
    // `ingest.rs`'s module docs for what of it is ported and differed.
    //
    // `about` is set explicitly, and the note above is a `//` comment rather
    // than a doc one, for the reason the root `Cli` gives: Click prints the
    // FIRST LINE of the docstring as the group summary and `help-tree.sh`
    // compares it. A doc comment explaining the port's own gap would put text
    // in the tree the reference does not have — measured, on the first run.
    /// Per-session static-analysis pass.
    #[command(
        about = "Per-session static-analysis pass — complexity / lint / type-completeness deltas.",
        long_about = None
    )]
    #[command(hide = true)]
    Analyze(crate::analyze::AnalyzeArgs),
    /// PR / CI ingest group.
    #[command(
        about = "Pull PR / CI data into the local store (REST backfill + webhook receiver).",
        long_about = None
    )]
    #[command(hide = true)]
    Ingest(IngestArgs),
    // ── RS-8-101 (the import leg) — appended at the tail ────────────────────
    /// Import external agent history via a user-supplied export command.
    ///
    /// For sources with no local transcript (cloud-gated tools), you supply an
    /// export command in a ``stackunderflow-history-plugin.json`` manifest; we own
    /// only the ``stackunderflow-history-jsonl-v1`` stream format. The command is
    /// run with **no shell**, a cleared + allowlisted environment, and byte + time
    /// caps; its stream is validated whole and upserted under the ``custom``
    /// provider (namespaced by the manifest's ``source_id``). Resumption uses an
    /// opaque cursor we store and replay but never interpret.
    ///
    /// Fail-closed: a non-zero exit, a timeout, or a malformed line aborts the
    /// whole import and leaves the stored cursor un-advanced. Re-running an
    /// unchanged export is an idempotent no-op (content-addressed ids).
    ///
    /// Also available as ``stax import``.
    #[command(hide = true)]
    Import(ImportArgs),
    // ── agent-remotes Phases 1+2 — appended at the tail ─────────────────────
    /// Manage the remote address book: other machines' datasets, by name.
    ///
    /// A remote is `NAME -> ssh://[user@]host[:port]/ABS_DATA_DIR` in
    /// config.json. `stax memory … --at NAME` and `stax resume --at NAME` run
    /// the same read-only verb where that data lives; `stax observe NAME`
    /// tails its most recent session. Auth is ssh's (keys, agent, tailnet);
    /// nothing here stores a credential.
    #[command(hide = true)]
    Remote(remote::RemoteArgs),
    /// Watch another machine's most recent agent session, live.
    ///
    /// Polls the remote's store over ssh (`store tail` on their side, the
    /// versioned `staxtrace.observe/1` envelope on the wire) and renders
    /// a log tail. `--once` fetches a single batch; `--json` passes envelopes
    /// through verbatim.
    #[command(hide = true)]
    Observe(observe::ObserveArgs),
    /// Every verb, including the ones this help hides.
    Advanced(crate::advanced::AdvancedArgs),
}

/// Parse this process's arguments and run the requested command.
///
/// # Errors
/// Whatever the command returns. Argument-parsing failures exit the process
/// through clap, as they do for every clap program.
pub fn run() -> Result<ExitCode> {
    dispatch(&Cli::parse())
}

/// Run an already-parsed [`Cli`].
///
/// Wave-1 commands print as they go and signal failure through `Err` (or, for
/// `memory`, through `std::process::exit`); wave-8 commands return their bytes
/// and their exit code as an [`Output`] so they are testable without a
/// subprocess. Both shapes end up as an [`ExitCode`] here.
///
/// # Errors
/// Whatever the command returns.
pub fn dispatch(cli: &Cli) -> Result<ExitCode> {
    let code = match &cli.command {
        Command::Anchor(args) => run_anchor(args).map(|()| ExitCode::SUCCESS)?,
        Command::Audit(args) => crate::audit::run_audit(args)?.emit(),
        Command::Cost(args) => run_report(args)?.emit(),
        Command::Replay(args) => run_context_replay(args)?.emit(),
        Command::Advanced(args) => crate::advanced::run_advanced(args)?,
        Command::Backup(args) => run_backup(args)?.emit(),
        Command::Benchmark(args) => run_benchmark(args)?.emit(),
        Command::Cfg(args) => run_cfg(args)?.emit(),
        Command::ClearCache(args) => run_clear_cache(args)?.emit(),
        Command::Compare(args) => run_compare(args)?.emit(),
        Command::Config(args) => run_config(args)?.emit(),
        Command::Docs(args) => run_docs(args)?.emit(),
        Command::Doctor(args) => run_doctor(args)?.emit(),
        Command::Export(args) => run_export_cmd(args)?.emit(),
        Command::FindFailureModesForFile(args) => {
            run_failure_modes(args).map(|()| ExitCode::SUCCESS)?
        }
        Command::FindSessionsInPath(args) => run_in_path(args).map(|()| ExitCode::SUCCESS)?,
        Command::FindSessionsTouchingFile(args) => {
            run_touching_file(args).map(|()| ExitCode::SUCCESS)?
        }
        Command::FindSessionsWhereActionWorked(args) => {
            run_action_worked(args).map(|()| ExitCode::SUCCESS)?
        }
        Command::Guide(args) => run_guide(args)?.emit(),
        Command::Hooks(args) => run_hooks(args)?.emit(),
        Command::Init(args) => run_init(args)?.emit(),
        // `--at NAME` re-runs the user's own argv on a registered remote —
        // read-only by construction (only memory/resume parse the flag).
        Command::Memory(args) if args.at.is_some() => {
            let tail: Vec<String> = std::env::args().skip(1).collect();
            remote::run_at(args.at.as_deref().unwrap_or_default(), &tail)?
        }
        Command::Memory(args) => run_memory(args).map(|()| ExitCode::SUCCESS)?,
        Command::ContextBudget(args) => run_context_budget(args)?.emit(),
        Command::ContextReplay(args) => run_context_replay(args)?.emit(),
        Command::Month(args) => run_month(args)?.emit(),
        Command::Optimize(args) => run_optimize(args)?.emit(),
        Command::Plan(args) => run_plan(args)?.emit(),
        Command::Recommend(args) => run_recommend(args)?.emit(),
        Command::Report(args) => run_report(args)?.emit(),
        Command::Resume(args) if args.at.is_some() => {
            let tail: Vec<String> = std::env::args().skip(1).collect();
            remote::run_at(args.at.as_deref().unwrap_or_default(), &tail)?
        }
        Command::Resume(args) => run_resume(args).map(|()| ExitCode::SUCCESS)?,
        Command::Risk(args) => run_risk(args)?.emit(),
        Command::SearchPastDecisions(args) => {
            run_past_decisions(args).map(|()| ExitCode::SUCCESS)?
        }
        Command::Skills(args) => run_skills(args)?.emit(),
        Command::Start(args) => run_start(args)?.emit(),
        Command::Status(args) => run_status(args)?.emit(),
        Command::Store(args) => run_store(args).map(|()| ExitCode::SUCCESS)?,
        Command::Sync(args) => run_sync(args).map(|()| ExitCode::SUCCESS)?,
        Command::Today(args) => run_today(args)?.emit(),
        Command::Yield(args) => run_yield(args)?.emit(),
        Command::Worktrees(args) => run_worktrees(args)?.emit(),
        // ── T2v3 (this leg) — appended at the tail, never interleaved ───────
        Command::Discovery(args) => run_discovery(args)?.emit(),
        Command::Etl(args) => run_etl(args)?.emit(),
        Command::Pricing(args) => run_pricing(args)?.emit(),
        Command::Reindex(args) => run_reindex(args)?.emit(),
        // ── TELEPHONE (this leg) — appended at the tail, never interleaved ──
        Command::Msg(args) => run_msg(args)?.emit(),
        Command::Analyze(args) => crate::analyze::run_analyze(args)?.emit(),
        Command::Ingest(args) => run_ingest(args)?.emit(),
        // ── RS-8-101 (the import leg) — appended at the tail ────────────────
        Command::Import(args) => run_import(args)?.emit(),
        // ── agent-remotes Phases 1+2 — appended at the tail ─────────────────
        Command::Remote(args) => remote::run_remote(args)?.emit(),
        Command::Observe(args) => observe::run_observe(args)?,
    };
    Ok(code)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_clap_definition_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn store_takes_an_optional_store_path() {
        let cli = Cli::try_parse_from(["stax", "store"]).expect("bare store parses");
        let Command::Store(args) = &cli.command else {
            panic!("expected store");
        };
        assert!(args.store.is_none());

        let cli = Cli::try_parse_from(["stax", "store", "--store", "/data/su/store.db"])
            .expect("--store parses");
        let Command::Store(args) = &cli.command else {
            panic!("expected store");
        };
        assert_eq!(
            args.store.as_deref(),
            Some(std::path::Path::new("/data/su/store.db"))
        );
    }

    #[test]
    fn the_binary_is_named_stax() {
        assert_eq!(Cli::command().get_name(), "stax");
    }
}
