# staxtrace

**Staxtrace watches the coding agents on your machine and tells you what they're reading, spending, and uploading — before it ends up on someone's server.**

Local-first from the first commit (2026-03-31): no account, no telemetry, nothing leaves `~/.staxtrace/`. Auditing your agents by uploading your data somewhere would be a joke we're not telling.

## Install

```bash
brew install 0bserver07/tap/staxtrace        # macOS (Apple Silicon) + Linux
# or:
curl -fsSL https://raw.githubusercontent.com/0bserver07/staxtrace/main/install.sh | sh

stax audit
```

## The audit

In mid-2026, Grok Build CLI v0.2.93 tarballed users' repositories and uploaded them to
xAI's trace bucket — on one of our machines that was 14 codebases and 99 upload events over
two weeks, and the obvious opt-out toggle didn't govern it. That incident is why this exists.

`stax audit` reads the configs of every coding agent it detects and reports which are
configured to send your code or telemetry off-box — with the exact line that stops each one:

```
EGRESS AUDIT — 4 of your 6 coding agents can upload your data

  AGENT    FINDING                                  SEVERITY  VETO
  copilot  Copilot CLI syncs your prompts, its re…  high      set "remoteExport": false in ~/.copilot/setting…
           ↳ ~/.copilot/settings.json: file not present — remoteExport is unset (no local veto)
  claude   An agent copied files to a remote host…  high      review the session; if the host is yours, allow…
           ↳ session a23e445d: rsync -az --delete ./dist/ deploy.example.com:/opt/app/
  codex    Codex CLI sends usage and health analy…  medium    set [analytics] enabled = false in ~/.codex/con…
           ↳ ~/.codex/config.toml: analytics.enabled is unset in ~/.codex/config.toml (no local veto)
  copilot  Copilot CLI session data leaves the ma…  medium    set "remote": "off" in ~/.copilot/settings.json…
           ↳ ~/.copilot/settings.json: file not present — remote is unset (no local veto)
  gemini   Gemini CLI usage-statistics collection…  medium    set "privacy": {"usageStatisticsEnabled": false…
           ↳ ~/.gemini/settings.json: privacy.usageStatisticsEnabled is unset in ~/.gemini/settings.json (no …
  claude   Claude Code sends usage metrics to Ant…  low       set "DISABLE_TELEMETRY": "1" in the env block o…
           ↳ ~/.claude/settings.json: env.DISABLE_TELEMETRY is unset in ~/.claude/settings.json (no local vet…
  cursor   cursor detected — Privacy Mode is acco…  info      —

  transcripts: 327 sessions scanned (20000 tool calls, newest 20000 messages)

  7 findings · unknown ≠ safe (1 unknown, 1 artifacts not read) · vetoes are suggestions — staxtrace never edits your configs
```

Every row shows its basis on the line beneath it — the file that is missing, the key
that is set, the session and the command. Transcript rows come from the sessions
staxtrace already ingested, for every agent at once; when the host is yours,
`stax audit --allow-host deploy.example.com` (or an `audit_allow_hosts` list in
config.json) retires the row. Localhost, private networks and your tailnet never need
listing. The headline counts agents with medium-or-higher findings; low rows — usage
metrics that carry no code — still print with their veto.

<!-- launch assets: replace the block above with docs/assets/audit.png + GIF from the Mac run -->

Signatures are [declarative JSON](signatures/), one file per agent — **verified or
explicitly pending, never guessed**. An agent we can't verify audits as *unknown, not
safe*. Every check ships a positive and a negative fixture in CI, and every finding
carries its veto. `--json` for machines; `--strict` exits 2 for CI. Detection is honest
about its limits: configs and transcripts are self-reported sources — see
[Spec 28](docs/specs/agent-egress-audit.md) for the full threat model.

## Five verbs

| verb | what it does |
|---|---|
| `stax audit` | which agents can upload your data, and the line that stops each |
| `stax cost` | what your agents cost you — spend, tokens, yield, by project |
| `stax replay` | reconstruct what the model saw in a session, step by step |
| `stax start` | the local dashboard (server + hooks ship in the same install) |
| `stax doctor` | store health; `--install` verifies the installation itself |

Everything else — 40+ verbs: memory, ingest, benchmarks, sync, skills — still works and
lives under [`stax advanced`](docs/cli-reference.md).

<p align="center">
  <kbd><img src="https://www.google.com/s2/favicons?domain=anthropic.com&sz=64" width="16" valign="middle" /> Claude Code</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=openai.com&sz=64" width="16" valign="middle" /> OpenAI Codex</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=cursor.com&sz=64" width="16" valign="middle" /> Cursor</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=cline.bot&sz=64" width="16" valign="middle" /> Cline</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=github.com&sz=64" width="16" valign="middle" /> Copilot</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=gemini.google.com&sz=64" width="16" valign="middle" /> Gemini / Antigravity</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=continue.dev&sz=64" width="16" valign="middle" /> Continue</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=codeium.com&sz=64" width="16" valign="middle" /> Codeium</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=qwen.ai&sz=64" width="16" valign="middle" /> Qwen</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=roocode.com&sz=64" width="16" valign="middle" /> Roo Code</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=hermes-agent.org&sz=64" width="16" valign="middle" /> Hermes</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=openclaw.ai&sz=64" width="16" valign="middle" /> OpenClaw</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=pi.ai&sz=64" width="16" valign="middle" /> Pi</kbd>
</p>

The audit is the front door; underneath is the observability engine it rides on: 20
provider adapters into one local SQLite store, cost marts, time-travel playback, an
agent-memory layer with a versioned contract, and an offline chat sidebar — all local.

[What it does](#what-it-does) · [Architecture](#architecture) · [Library API](#library-api) · [Configuration](#configuration) · [Privacy](#privacy)

![staxtrace — the projects overview across every coding agent the local store has indexed](assets/overview.png)

*Writeup: [Building staxtrace](https://yad.codes/posts/building-stackunderflow/).*

---

> **Formerly published as StackUnderflow.** The engine is the Rust workspace
> under [`rust/`](rust/) — same store, same schema, no migration. The Python
> implementation lives on the [`python-legacy`](../../tree/python-legacy)
> branch in maintenance mode.

## Quickstart

The first run picks up whatever local sessions you already have under `~/.claude/`, `~/.codex/`, etc.

**Prebuilt binaries** — every [release](../../releases) carries a tarball and a
`.sha256` per platform (darwin arm64, linux x86_64/arm64 — glibc ≥ 2.39 until the
static musl builds land; Intel macOS pending):

```bash
TAG=v1.0.0; TRIPLE=aarch64-apple-darwin       # or your platform
curl -LO https://github.com/0bserver07/staxtrace/releases/download/$TAG/staxtrace-$TAG-$TRIPLE.tar.gz
curl -LO https://github.com/0bserver07/staxtrace/releases/download/$TAG/staxtrace-$TAG-$TRIPLE.tar.gz.sha256
shasum -a 256 -c staxtrace-$TAG-$TRIPLE.tar.gz.sha256
tar -xzf staxtrace-$TAG-$TRIPLE.tar.gz
sudo mv staxtrace-$TAG-$TRIPLE/{stax,stax-server,stax-hooks} /usr/local/bin/
```

**From source** — Rust 1.89+:

```bash
git clone https://github.com/0bserver07/staxtrace
cd staxtrace/rust && cargo build --release
cargo install --path crates/stax-cli --path crates/stax-server --path crates/stax-hooks
```

Then, whichever route you took:

```bash
stax init                                  # dashboard at localhost:8081
stax hooks install --scope user --inject   # opt-in: past context into live agent turns
stax guide install                         # opt-in: teach your agents the commands exist
```

**The build produces three binaries, and you need all three.** `stax` is the
command you type; `stax-server` is the dashboard; `stax-hooks` is the injection
fast path. `stax` locates the other two *next to its own executable*, so
installing only `stax` leaves `stax start` failing with
`No such file or directory` — it is looking for `stax-server`. Install them
together, or copy all three into one directory on your `PATH`; they carry every
data file they read, so they run from anywhere.

Do **not** symlink a single binary out of `target/release/` — earlier revisions
of this README suggested it. It installs one third of the product, and it pins
your `PATH` to a build directory that the next `cargo clean` or branch switch
empties.

`stax hooks install` is separate from installing the binaries and is what makes
the tool proactive: without it, nothing surfaces prior context into a session
and cross-machine messages sit unread in the inbox. Note that **context
injection is a second opt-in** — plain `hooks install` registers capture only,
and you want `--inject` for the memory and inbox delivery described above:

```bash
stax hooks install --scope user --inject --dry-run   # inspect first
stax hooks install --scope user --inject
```

It is idempotent, backs the file up first, and touches only its own entries.
Check any time with `stax hooks status`.

> **Upgrading from a pre-split install:** older installs wrote hook commands
> that invoked the Python entry point (`stackunderflow hooks run …`) — a
> program a Rust-only install no longer has. Re-running `stax hooks install`
> (or `stax hooks repair`) rewrites those entries in place to the native form,
> `stax-hooks run …`; the hook ids and every other tool's entries are
> untouched.

**Python (maintenance mode)** lives on the
[`python-legacy`](../../tree/python-legacy) branch — same store, same schema;
the old PyPI packages have been removed — install from this repo.

`stax` is the native binary; `stackunderflow` survives as its long-form alias on the [`python-legacy`](../../tree/python-legacy) branch. Where this README uses the long form, substitute `stax` — same commands, same flags, same output.

> **Known issue — installing the python-legacy package overwrites the Rust
> `stax`.** The legacy branch's `pyproject.toml` declares two console scripts,
> `stackunderflow` **and** `stax`, both pointing at the Python CLI. `stax` is
> also the native binary's name, so a `pip`/`pipx` install from that branch
> drops a Python `stax` onto your `PATH` and the native one is silently
> shadowed — `stax --version` starts answering `stackunderflow, version 0.9.x`
> instead of `stax 1.0.0`. Re-run the `cargo install` above afterwards, and
> check `stax --version` whenever you touch the Python package.

Browser opens to `http://localhost:8081` with every project the local store knows about, indexed and ready. Background ingest + watcher start immediately; the dashboard is interactive while ingest runs.

If port 8081 is taken: `stax cfg set port 8090` then re-run.

```bash
# common knobs
stax cfg set port 8090            # change the port
stax cfg set currency GBP         # display costs in another currency
stax plan set claude-pro          # track against a monthly budget
stax init --no-browser            # don't auto-open the browser
stax --help                       # full CLI  (or: stax --help)
```

### From source

```bash
git clone https://github.com/0bserver07/staxtrace.git
cd staxtrace/rust
cargo build --release
cargo install --path crates/stax-cli --path crates/stax-server --path crates/stax-hooks
stax init
```

(The Nix flake retired with the in-tree Python implementation; it lives on
[`python-legacy`](../../tree/python-legacy).)

---

## CLI Tour (Live Terminal Demo)

staxtrace features a robust, colorful terminal interface powered by `rich`. Here is a direct look at the CLI in action, showing how you can query cost, audit waste, and query past sessions:

### 1. Cost & Ingest Status (`stax status`)
Get a quick, one-line summary of your active token spending and message counts for the day and the current billing cycle:
```bash
$ stax status
today: $35.63 (75 msg) | month: $7974.71 (31728 msg)
```

### 2. Multi-project reports (`stax report`)
Generate high-fidelity, ASCII table summaries of your spending across all active agent workspaces over a custom date range (e.g., the last 7 days):
```ansi
$ stax report
staxtrace — last 7 days
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓
┃ Project                                     ┃     Cost ┃ Messages ┃ Sessions ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩
│ -Users-yadkonrad-dev-dev-year26-jan26-Stac… │ $1081.59 │    3,514 │       20 │
│ -Users-yadkonrad-dev-dev-year26-jan26-new-… │  $635.22 │      998 │        2 │
│ -Users-yadkonrad-dev-dev-year26-jan26-bour… │  $289.22 │      905 │        2 │
│ -Users-yadkonrad-dev-dev-year26-feb26-chim… │  $239.58 │    1,254 │       11 │
│ -Users-yadkonrad-dev-dev-year26-feb26-clau… │  $203.06 │      593 │        4 │
│ -Users-yadkonrad-dev-dev-year26-may26-Stud… │  $157.24 │      176 │        2 │
└─────────────────────────────────────────────┴──────────┴──────────┴──────────┘
Total: $2894.57  8,315 messages  59 sessions
```

### 3. Waste audit & cost optimization (`stax optimize`)
Run automated, offline waste detectors (looped Q&A pairs, cache thrashing, excessive file re-reads, and unused MCP servers) to cut down your active developer billing:
```ansi
$ stax optimize
Waste report — last 30 days

Q&A loops:
  -Users-yadkonrad-dev-dev-year26-feb26-claude-sessions: 6 looped pair(s)
    - "if u were to review our entire conversations, whats is the oscillation like?"

Structural patterns:
  [HIGH] cache_overhead: 241 session(s) with cache thrash
      241 session(s) where cache_create_tokens exceed 50% of total input
      ~289,497,821 wasted tokens
      fix: Bundle related questions into one session so cache writes amortise.
  [HIGH] junk_reads: 61 file(s) re-read excessively
      61 file(s) Read 5+ times in a single session — assistant likely forgot prior reads.
      fix: Cache file contents in working memory or use Grep to search.
```

### 4. Query the memory layer (`stax memory decisions "<term>"`)
Active agents (or developers) query the local store straight from the CLI — `stax memory decisions/file/worked/sessions/ask` — to reuse past decisions and avoid redoing work. Add `--json` to any subcommand for the stable, token-bounded `staxtrace.memory/1` envelope:
```ansi
$ stax memory decisions "cache"
Past decisions matching 'cache' (14 session(s))

  [claude] 18d87ee4-b01…  2026-05-20T03:21:26  msgs=445  $115.0498
      -Users-yadkonrad-dev-dev-year26-jan26-staxtrace  /Users/yadkonrad/dev/dev/year26/jan26/staxtrace
      … remove a leaked email and force-pushed. Please garbage-collect the dangling/unreachable commits so cached SHAs stop resolving.

  [claude] 5be67015-9a4…  2026-05-20T01:56:58  msgs=198  $22.2723
      … memory-and-latency's "no in-process cache" claim was false — `/api/dashboard-data` has a memo cache plus a `project_mart` fast-path.
```

---

## Every command

The full surface, grouped by the question it answers. `stax <verb> --help` for
flags; every read verb takes `--json`.

**What did this cost?**

| | |
|---|---|
| `status` | today + this month, one line |
| `today` · `month` · `report` | usage by project over a window |
| `compare` | per-model metrics side by side |
| `export` | aggregates out to CSV/JSON |
| `plan` | track against a monthly budget (Claude Pro, Cursor Pro, custom) |
| `pricing doctor` | rate-card health: unpriced models, stale rates, dollar exposure |

**What is being wasted?**

| | |
|---|---|
| `optimize` | looped Q&A pairs plus seven structural waste patterns, in dollars |
| `yield` | productive vs reverted vs abandoned sessions, correlated against `git log` |
| `benchmark` | which model wins for the kind of work *you* actually do — a natural experiment over runs you already paid for, with n, coverage and confidence |
| `context-budget` | the per-session context tax: system prompt + MCP + skills + memory |
| `worktrees` | per-worktree cost and prune safety (read-only) |
| `recommend` | proactive recommendations mined from your own history |

**What do I already know?**

| | |
|---|---|
| `memory decisions` | what did I decide about this before |
| `memory file` | this file's history: past edits, failure modes |
| `memory worked` | where did this action actually succeed, with evidence |
| `memory sessions` · `memory ask` | recent work · natural language over your history |
| `risk` | "this file has caused N reverts in M days" — before you edit it |
| `search-past-decisions` | substring search across past message content |
| `find-sessions-in-path` · `find-sessions-touching-file` | by project root · by file mentioned in tool calls or prose |
| `find-sessions-where-action-worked` · `find-failure-modes-for-file` | where an action was confirmed to work · where editing a file led to a correction |
| `resume` | session + resume ids for every agent under a path |

**What actually happened?**

| | |
|---|---|
| `context-replay` | reconstruct what the model *saw* at step N |
| `analyze` | per-session static-analysis pass: complexity / lint / type deltas |
| `store` · `store tail` | schema + row counts · new messages for a session |
| `doctor` | read-only health and delivery check across every provider |

**Make it proactive**

| | |
|---|---|
| `hooks install --inject` | surface past context into a live agent turn |
| `guide install` | teach agents the commands exist, via CLAUDE.md / AGENTS.md |
| `skills` | generate project-specific Claude Code skills from your own patterns |
| `start` / `init` | dashboard + resident watcher |

**Across machines**

| | |
|---|---|
| `remote add/ls/rm` | the address book: `NAME -> ssh://host/data-dir` |
| `memory … --at NAME` · `resume --at NAME` | run a read-only query where the data lives |
| `observe NAME` | tail another machine's newest session, live |
| `msg send` · `msg inbox` | the agent telephone: store-and-forward over ssh |

**Data plumbing**

| | |
|---|---|
| `etl` | ingest → events → marts, and `etl backfill` |
| `reindex` | rebuild the store from source files |
| `import` | external agent history via a user-supplied export command |
| `ingest` | PR / CI data (REST backfill + webhook receiver) |
| `backup` · `sync` | local snapshots · encrypted bring-your-own-bucket |
| `cfg` · `clear-cache` · `docs` · `anchor` · `discovery` | settings · cache · offline docs · durable campaign state · citation telemetry |

## What it does

### Multi-provider ingest
20 coding agents have adapters in the registry — all enabled by default (a provider without data on the machine simply contributes nothing). The busiest sources:

| Provider | Source |
|---|---|
| Claude Code | `~/.claude/projects/<slug>/*.jsonl` (+ legacy `~/.claude/history.jsonl`) |
| Codex | `~/.codex/sessions/{YYYY}/{MM}/{DD}/rollout-*.jsonl` |
| Cursor | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` |
| Cline | `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/` |
| OpenClaw | `~/.openclaw/agents/` (+ clawdbot / moltbot / moldbot variants) |
| Pi + OMP | `~/.pi/agent/sessions/`, `~/.omp/agent/sessions/` |
| Hermes | `~/.hermes/sessions/` |

Thirteen more are registered the same way — KiloCode, Roo Code, OpenCode, Cursor Agent, Qwen, Gemini, Copilot, Codeium, Continue, Droid, Kiro, Antigravity, and Grok — and load automatically wherever their source directories exist. There is no opt-in flag; per-adapter fidelity is curated in `stackunderflow/adapters/capabilities.json`.

See [docs/multi-provider.md](docs/multi-provider.md) for the per-provider source paths and the cost-source semantics each one uses (rate-card vs estimated).

### Cost analysis
- **Cost tab** — top sessions by cost, most expensive commands (click → Messages tab), tool-cost ranking, token composition (donut + stacked daily), cache ROI, outliers, retry-loop signals, week-over-week trends, error-cost estimate. Filters (range / session / tool) URL-encoded.
- **Compare** — side-by-side model metrics over a window: one-shot rate, retry rate, cache hit rate, $/call, $/session. Group by `(provider, model)` (Agent × Model) or just model.
- **Plan budgets** — set a monthly budget from a preset (Claude Pro $20, Claude Max $200, Cursor Pro/Max) or a custom amount. Shows used / remaining / projected month-end.
- **Yield analysis** — correlates sessions with `git log` per cwd: productive (commit followed within 24h) / reverted / abandoned / no-repo. Use it to find which sessions actually shipped code.
- **Optimize** — eight waste detectors: looped Q&A, bloated CLAUDE.md, unused MCP servers, ghost agents, low read-to-edit ratio, junk reads, cache overhead, bash-output limits. Each finding ships with a one-line suggested fix.
- **Context-budget estimator** — what your system prompt + MCP servers + skills + memory files cost on every turn before you type anything.
- **Multi-currency** — pick any 3-letter ISO code; FX rates from the public Frankfurter API (24h cached, ECB snapshot fallback when offline).
- **Model aliases** — for proxied model ids (OpenRouter, Replicate, internal gateways): `cfg model-alias set openrouter/claude-opus claude-opus-4-6` and the cost layer prices it at the canonical rate.
- **Fast-mode multiplier** — Claude Opus priority tier (`service_tier="priority"`) bills at 6×; detected from the JSONL and threaded through the cost layer end-to-end.

![The Cost tab: spend by agent, cache ROI, and an error-cost breakdown by tool](assets/cost.png)

![Compare: per-model sessions, retry, cache, and unit economics side by side](assets/compare.png)

![Tools ranked by cost and the token-composition donut](assets/tools.png)

### Search, Q&A, tags
- **Full-text search** across every ingested message. Filter by date / model / role.
- **Q&A pair extraction** — heuristic detection of question/answer pairs with resolution status (`resolved` / `looped` / `abandoned`).
- **Auto-tagging** — sessions get tagged by language, framework, topic, intent (`build`, `fix`, `explore`, `refactor`, `test`, `ops`).

### Meta agent (Ask staxtrace)
A **right-docked sidebar** lets you talk to an Ollama LLM about your own coding history. It calls a catalogue of read-only backend tools (search past decisions, find sessions touching a file, get a project's cost summary, replay a session's filesystem mutations, …) and answers in prose. Recommended models: `qwen2.5-coder`, `llama3.2`. The chat talks only to Ollama — local at `localhost:11434` by default, or the endpoint you set via `STACKUNDERFLOW_OLLAMA_URL` (+ `STACKUNDERFLOW_OLLAMA_API_KEY`); with no reachable endpoint the sidebar surfaces a banner. See [docs/meta-agent.md](docs/meta-agent.md).

![Ask staxtrace: a local model answering from your own session history via read-only tools](assets/agent-sidebar.png)

### Playback (time-travel)
- **Event-stream timeline** — scrub through every tool call a session made, in order, with payload excerpts.
- **Virtual-FS reconstruction** (v0.7.3+) — at any timestamp in the scrub, see the reconstructed content of every file the session touched. Replays Read / Write / Edit / MultiEdit / NotebookEdit calls; marks partial reconstructions where no initial Read was seen.

![Step-by-step playback with the reconstructed file tree at each moment](assets/playback.png)

### Local agent memory (self-referential recall)
A coding agent — Claude Code, Cursor, Codex, or anything that can run a shell command — queries its own history *before* it acts, so it stops relearning the same lessons. One command group, one versioned contract:

- **`stax memory decisions "<text>"`** — past decisions on a topic. **`stax memory file <path>`** — a file's history: prior edits, failure modes, and a risk summary. **`stax memory worked "<action>"`** — outcome-aware recall that returns sessions whose *later turns confirmed or contradicted* the action, with a confidence score so silence isn't mistaken for success. **`stax memory sessions [path]`** — sessions that touched a path. **`stax memory ask "<question>"`** — a natural-language query over the whole store.
- **Lexical + semantic ranking.** Candidates rank by FTS5 + bm25; `stax memory ask` fuses that keyword search with a local semantic vector search (reciprocal-rank fusion) over Ollama-served embeddings (default `nomic-embed-text`), and degrades cleanly to keyword-only when Ollama isn't running — so it always answers, and gets sharper when a local model is available.
- **A formal, versioned contract.** Add `--json` to any subcommand for the `staxtrace.memory/1` envelope — a stable, token-bounded shape (`schema`, `command`, `results[]`, `token_estimate`, `budget`, `truncated`) frozen by a JSON-Schema, with golden fixtures for every subcommand × {success, empty, error} and a stdlib validator (`scripts/check_memory_contract.py`) enforced in CI. Any harness, not just Python, can parse it. (The older `find-sessions-*` / `search-past-decisions` names remain as aliases, with an opt-in `--use-embeddings`.)
- **`stax skills generate`** — mines this store for project-specific workflow patterns and emits Claude Code `SKILL.md` files; the shipped skills auto-surface prior context when you open a project or name a file. Project-scoped by default.
- **Bookmarks** — pin conversations you want to find later.

### Real-time sync
A `watchfiles`-backed daemon thread watches every registered adapter's source paths. On any change → ingest the new bytes → normalize → refresh marts. Source-file write to dashboard data fresh in ~400ms. Disable with `--no-watcher`.

![The ETL pipeline panel: watcher status, event count, and per-mart watermarks](assets/etl.png)

### Export
```bash
stax export -f csv -o usage.csv -p month
stax export -f json -o usage.json   # multi-period rollup (today + 7d + 30d)
```

The dashboard's "Download" button hits the same `/api/export` endpoint.

### Backup
```bash
stax backup create               # snapshot ~/.claude/ via rsync --link-dest
stax backup auto --enable        # daily on macOS via launchd
stax backup list
stax backup restore <name>
```

Snapshots land under `~/.stackunderflow/backups/<ts>[-label]/`. Unchanged files are hard-linked from the previous snapshot, so a daily backup of a quiet `~/.claude/` is roughly zero on-disk delta. Full surface in [docs/backup.md](docs/backup.md).

### Chat sidebar
A header toggle slides in a chat drawer that streams from a **local** Ollama instance (proxied through `/api/ollama-api/*`, default upstream `http://localhost:11434`). Pick a pulled model, type, get a streamed reply — nothing leaves the machine. Empty model list = Ollama not running. See [docs/chat.md](docs/chat.md).

---

## Architecture

The pipeline is three layers tied together by a watermarked refresh loop and a filesystem watcher.

```mermaid
flowchart TD
    %% Theme Styling for Dark & Light Mode Legibility
    classDef source fill:#1A202C,stroke:#4A5568,stroke-width:1.5px,color:#EDF2F7;
    classDef pipeline fill:#2B6CB0,stroke:#3182CE,stroke-width:2px,color:#FFF;
    classDef db fill:#2C7A7B,stroke:#319795,stroke-width:2px,color:#FFF;
    classDef interface fill:#D69E2E,stroke:#ECC94B,stroke-width:2px,color:#FFF;
    classDef cli fill:#E53E3E,stroke:#F56565,stroke-width:2px,color:#FFF;
    classDef agent fill:#805AD5,stroke:#9F7AEA,stroke-width:2px,color:#FFF;

    %% 1. Log Sources
    subgraph Sources ["📁 Input Log Sources (20 Providers)"]
        Logs["Local Session Logs<br/>• Claude Code JSONL<br/>• Cursor state.vscdb<br/>• Cline tasks JSON"]
    end
    class Logs source;

    %% 2. Background Processing
    subgraph Engine ["⚡ staxtrace Core Engine"]
        Watcher["Filesystem Watcher<br/>• 200ms debounce<br/>• ~400ms fresh sync"]
        Ingest["Ingest & Normalizer<br/>• Standardizes events<br/>• Computes costs offline"]
        Store[("SQLite Store<br/>~/.stackunderflow/store.db")]
        ETL["Mart Builder (ETL)<br/>• Aggregates 8 reporting marts<br/>• Correlates Git yields"]
    end
    class Watcher,Ingest,ETL pipeline;
    class Store db;

    %% 3. Interfaces & Presentation
    subgraph Frontends ["🖥️ Interfaces & Presenters"]
        API["FastAPI REST Web Server<br/>• Serving /api/* routes"]
        CLI["Command Line Interface (CLI)<br/>• stax today / month<br/>• stax optimize / report<br/>• stax memory (agent queries)"]
    end
    class API interface;
    class CLI cli;

    %% 4. Client / End User Applications
    subgraph Clients ["👥 End Users & AI Clients"]
        Dashboard["React Web Dashboard<br/>• http://localhost:8081<br/>• Analytics, playback & virtual FS"]
        Ollama["Local Ollama Chat<br/>• Offline history Q&A sidebar"]
        Agent["Active AI Agent (Claude Code / Cursor)<br/>• Queries past runs during sessions<br/>• Learns from previous failures"]
    end
    class Dashboard,Ollama interface;
    class Agent agent;

    %% Watcher Loop
    Watcher -.->|Monitors| Logs
    Watcher -.->|Triggers Ingest| Ingest

    %% Data Pipeline Flow
    Logs --> Ingest
    Ingest -->|Raw & Normalized events| Store
    Store --> ETL
    ETL -->|Aggregated reporting marts| Store

    %% Access Points
    Store --> API
    Store --> CLI

    %% Client Delivery
    API --> Dashboard
    API --> Ollama
    CLI <-->|memory CLI queries| Agent
    CLI <-->|Developer CLI Reports| Dashboard
```

Most dashboard routes read from the marts when populated, falling back to a live aggregation pass otherwise. On a 247K-message store the cold-load went from 2.5s to <50ms warm. A new install starts on the empty-mart fallback path (still functional, just slower); the first watcher cycle or `stax etl backfill` populates the marts.

```
rust/crates/
  stax-core/        # store, schema + migrations, settings, agent inbox — the bedrock
  stax-adapters/    # 20 source-file parsers (all always-on; self-registering)
  stax-etl/         # ingest → normalize → 8 marts; watcher, backfill, watermarks, pricing
  stax-memory/      # the versioned memory/resume envelopes other harnesses parse
  stax-reports/     # report renderers, aggregation, optimize patterns — no HTTP
  stax-server/      # axum routes (one per concern) + the embedded React bundle
  stax-cli/         # every `stax` verb
  stax-hooks/       # the standalone hook binary Claude Code spawns
  stax-sync/        # ssh transport (backup, msg, --at)
  stax-wasm/        # wasm surface

ui/                 # React + TypeScript + Tailwind + Recharts (builds into stax-server)
```

The Python implementation this was ported from — `stackunderflow/` with its
`adapters/`, `etl/`, `routes/`, `services/`, `cli.py` and `server.py` — lives on
the [`python-legacy`](../../tree/python-legacy) branch. Comments throughout the
Rust tree cite those files by path; they are provenance, and that is where to
find them.

For the deeper design rationale see `docs/specs/etl-architecture.md`. For the on-disk schema as a versioned spec other tools can target: [docs/specs/session-schema-v1.md](docs/specs/session-schema-v1.md) (+ [adapter-contract.md](docs/specs/adapter-contract.md) for the source-adapter Protocol). For the state-of-the-codebase walkthrough (recent history, gotchas, real-data state, what's left) see [docs/HANDOFF.md](docs/HANDOFF.md).

---

## Library API

The engine is a binary, not a library: any harness integrates through the
**versioned JSON envelopes**, which is the contract this project actually
freezes and gates in CI.

```bash
stax memory decisions "cache invalidation" --json   # staxtrace.memory/1
stax resume --json                                  # staxtrace.resume/1
stax store tail --json                              # staxtrace.observe/1
```

Every envelope carries `schema`, is token-bounded, and has golden fixtures per
subcommand. Parse it from any language; nothing here needs a Python import.

Inside the workspace, the crates are the seams — `stax-core` (store, schema),
`stax-memory` (the envelope types), `stax-reports` (aggregation, no HTTP),
`stax-adapters` (the 20 parsers). See [`rust/README.md`](rust/README.md).

<details>
<summary><b>The Python library API (python-legacy branch)</b></summary>

The pre-split Python package exposed an importable API. It is unchanged on the
[`python-legacy`](../../tree/python-legacy) branch and documented here for
anyone still running it — it is **not** installable from `main`.

```python
import stackunderflow

# Every project the local store knows about, provider-tagged.
projects = stackunderflow.list_projects()
# [{"slug": ..., "provider": "claude" | "codex" | "cursor" | ...,
#   "display_name": ..., "path": ..., "first_seen": ..., "last_modified": ...}]

# Filter to one provider:
codex_only = stackunderflow.list_projects(provider="codex")

# Sessions for a project:
sessions = stackunderflow.list_sessions("project-slug")
# [{"session_id": ..., "first_ts": ..., "last_ts": ..., "message_count": ...}]

# Pipeline-formatted messages + statistics for one project:
messages, stats = stackunderflow.process(projects[0]["slug"])
print(f"Sessions: {stats['overview']['sessions']}")
print(f"Cost: ${stats['overview']['total_cost']:.2f}")
```

`list_projects()` returns `[]` rather than raising when the store doesn't exist yet. `process()` raises `KeyError` when the slug isn't found.

For lower-level access:

```python
from stackunderflow.store import db, queries, mart_queries
from stackunderflow.etl import backfill, watermark
from stackunderflow.etl.normalize import get as get_normalizer
from stackunderflow.infra.discovery import locate_logs
```

</details>

---

## Claude Code skills

staxtrace ships a set of [Claude Code skills](https://code.claude.com/docs/en/skills) that turn the local store into a reflex: Claude Code automatically surfaces prior session context when you start work in a project, mention a specific file, or reference a past decision. Install with `cp -r stackunderflow/skills/* ~/.claude/skills/` — see [docs/skills.md](docs/skills.md) for trigger semantics and example transcripts.

---

## ETL operations

The pipeline is incremental + idempotent. Most users never need to think about it. For when you do:

```bash
# Health check — watcher status, mart watermarks vs max event id, lag
stax etl status

# Populate marts from existing messages (one-time on first install or after a crash)
stax etl backfill          # incremental — skips converted msgs
stax etl backfill --force  # drop + rebuild from scratch

# Same backfill, kicked off in the background from HTTP (used by the
# Settings page "Backfill now" button); poll /api/etl/status to follow it
curl -X POST http://127.0.0.1:8081/api/etl/backfill

# Disable the watcher (headless / debugging)
stax start --no-watcher
# or via env var:
STACKUNDERFLOW_DISABLE_WATCHER=1 stax start

# Skip the watcher single-instance lock (multi-server, or stale lock file)
stax start --no-lock
# or via env var:
STACKUNDERFLOW_DISABLE_LOCK=1 stax start
```

Watcher state (including the PID currently holding the watcher lock),
watermarks, per-provider event counts, and any in-flight backfill job
are also at `GET /api/etl/status` and visible as a badge in the
dashboard header.

---

## Configuration

```bash
stax cfg ls                   # show current settings
stax cfg set port 8090
stax cfg rm port              # reset to default
```

Selected keys (full list in [docs/cli-reference.md](docs/cli-reference.md)):

| Key | Default | Description |
|---|---|---|
| `port` | `8081` | Server port |
| `host` | `127.0.0.1` | Bind address |
| `auto_browser` | `true` | Open browser on start |
| `currency` | `USD` | Display currency (any 3-letter ISO) |
| `model_aliases` | `{}` | Proxy id → canonical (manage via `cfg model-alias`) |
| `plan_name` | unset | Active plan preset (`claude-pro`, `claude-max`, `cursor-pro`, `cursor-max`, `custom`) |
| `plan_monthly_usd` | `0.0` | Monthly budget (USD) |
| `plan_reset_day` | `1` | Day of month the budget resets |
| `auto_reindex_on_ingest` | `true` | Refresh search/qa/tags after each ingest |

Env vars override the persisted file. The Python descriptor in `stackunderflow/settings.py` resolves env → file → default lazily on every read.

---

## Privacy

Everything runs locally. Nothing about your sessions, prompts, or code leaves the machine.

**What staxtrace reads on disk** — only the source paths the registered adapters point at. The 7 default-on roots:
- `~/.claude/projects/`, `~/.claude/history.jsonl` (legacy)
- `~/.codex/sessions/`
- `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
- `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/`
- `~/.openclaw/agents/` (+ clawdbot / moltbot / moldbot variants)
- `~/.pi/agent/sessions/`, `~/.omp/agent/sessions/`
- `~/.hermes/sessions/`

The other adapters add more source roots wherever their source directories exist. Full path list in [docs/multi-provider.md](docs/multi-provider.md).

**What it writes** — `~/.stackunderflow/` only.
- `store.db` — SQLite, WAL mode, the source of truth
- `cache/` — currency rates (24h), Cursor vscdb fingerprint cache
- `backups/` — only when you run `backup create`. Plain copy of `~/.claude/` snapshots — protect this directory.

**What leaves your machine** — only when explicitly enabled:
- Pricing snapshot from `github.com/BerriAI/litellm` (no user data sent; hardcoded fallback in `infra/costs.py`)
- FX rates from `api.frankfurter.app` when `currency != USD` (no user data sent; ECB snapshot fallback embedded in `infra/currency.py`)
- Chat and embedding requests to the Ollama endpoint you configure with `STACKUNDERFLOW_OLLAMA_URL` (these carry message text from your store). Unset, the only Ollama endpoint tried is local `localhost:11434` — or nothing at all.

No telemetry. No tracking. No crash reports. No analytics. The app is a single binary that talks to your filesystem and your browser.

---

## Development

```bash
git clone https://github.com/0bserver07/staxtrace.git
cd staxtrace/rust

cargo test --workspace                               # the suite
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check

# The React frontend (build output embeds into stax-server)
cd ../ui && npm install && npm run build

# Lint
ruff check stackunderflow/

# Frontend
cd stackunderflow-ui
npm run typecheck
npm run build                          # outputs to ../stackunderflow/static/react/
node --test tests/services/*.test.ts   # unit tests via Node 22+ built-in runner
```

For an architecture walkthrough oriented at a new contributor or agent: [docs/HANDOFF.md](docs/HANDOFF.md).

For per-component design specs: [docs/specs/](docs/specs/).

For adapters: [docs/adapters.md](docs/adapters.md) walks through writing one.

---

## License

MIT — see [LICENSE](LICENSE).
