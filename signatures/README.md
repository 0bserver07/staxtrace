# Egress signatures

Declarative D1 signatures (Spec 28 §6), one JSON file per agent, following the
`adapters/capabilities.json` pattern: the catalog is data the engine interprets
— new agents are catalog entries, not code. The copies here are compiled into
the `stax` binary; a checkout or `stax audit --signatures <dir>` overrides them.

## The rules

1. **Verified, not guessed.** Every check cites reality: a documented setting,
   the agent's source, or observed behavior. A key nobody verified does not
   ship — mark the agent `pending` instead, and it audits as *unknown, not
   safe* (§8.3).
2. **No signature without its negative.** Every check has a positive fixture
   (fires) and a negative fixture (stays silent) in
   `rust/crates/stax-audit/tests/catalog_fixtures.rs`, written in the file
   shape the vendor really produces — dotted TOML keys, quoted sections,
   arrays of tables, JSONC, the nested Gemini schema.
   `every_shipped_check_has_a_fixture_pair` fails the build for a check that
   ships without one: the rule is enforced, not promised.
3. **Every finding carries its veto, and its evidence.** A warning without
   the exact fix line is noise; the loader rejects vetoless checks. The table
   prints the basis under every row (`file not present`, `key = value`,
   `VAR=1 (exported in your shell)`), because a row nobody can check is a
   row nobody should believe.
4. **Postures are honest.** `at_risk` / `protected` / `unknown` — there is no
   `safe`. Unreadable artifacts and unmodeled values degrade to `unknown`.
5. **Confined to the home.** Every path is `~/`-relative with plain
   components; the loader refuses anything else and the scanner drops root
   and `..` components again. A third-party signature pack cannot read the
   machine.

## Shipped signatures

| Agent | Status | Verified against |
|---|---|---|
| grok | 3 checks | Spec 28 §0/§3 — the 2026 upload incident, verified on the maintainer's machine (local `[telemetry]` vetoes outrank the remote flag). The negative fixture is that machine's real config shape, `[[marketplace.sources]]` included |
| claude | 5 checks | Claude Code's data-usage and monitoring docs (2026-09): usage metrics **default on** (`DISABLE_TELEMETRY`; never code, prompts or paths), error reports on for Pro/Max sign-ins (`DISABLE_ERROR_REPORTING`), the transcript-share survey (`CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY`; nothing is sent unless you answer Yes), `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` as the umbrella, and the opt-in OpenTelemetry export (`CLAUDE_CODE_ENABLE_TELEMETRY`, `OTEL_LOG_USER_PROMPTS`). The `DISABLE_*` family is presence-based ("any non-empty value including `0`"), and every one of them is read from the shell environment as well as `settings.json` |
| gemini | 1 check | Gemini CLI configuration docs: `privacy.usageStatisticsEnabled`, **default true**, nested since v0.3.0 with automatic migration from the flat key (2025-09); both spellings are read |
| codex | 3 checks | Codex config reference: `analytics.enabled` (dotted keys are the documented form; "when unset, the client default applies"), `otel.log_user_prompt` ("opt in to exporting raw user prompts"), `check_for_update_on_startup`. Note `disable_response_storage` was REMOVED in Sep 2025 — `store:false` is now unconditional, so there is no key to audit |
| copilot | 3 checks | Copilot CLI configuration-directory reference: `settings.json` is JSONC, `remoteExport` (**default true** — "export sessions remotely when session sync is available") and `remote` (**default "on"** — "controls session syncing and remote access"), plus the deprecated gh-copilot `optional_analytics` that survives uninstall |
| cursor | **pending, permanently** | Verified conclusion, not a gap: Privacy Mode is account-side and server-enforced ("tied to your account, not to a specific app"), so no local file answers the question. `telemetry.telemetryLevel` exists via the VS Code lineage but Cursor documents no commitment to honor it — claiming it as a veto would be a lie |

**Locally auditable ≠ everything.** Training opt-outs and public-code-match
policies for Copilot, and Privacy Mode for Cursor, live in web dashboards. D1
reports what a local file can prove and says `unknown` for the rest.

## Check fields

```jsonc
{
  "id": "agent.check_name",        // stable dedupe key, must start with "<agent>."
  "file": "~/.agent/config.toml",  // artifact; ~/ only, plain components
  "format": "json | toml | env | yaml-flat",
  "key": "section.key",             // dotted path (or VAR name for env files)
  "alt_keys": ["old.spelling"],     // the same setting where a schema moved it
  "env_var": "VAR",                 // the same setting exported in the shell
  "uploading_when": [true],         // values that mean "configured to upload" → at_risk
  "safe_when": [false],             // values that mean the veto is present → protected
  "safe_when_set": true,            // OR: any non-empty value is the veto (the DISABLE_* family)
  "at_risk_when_unset": true,       // unset key/file = absent veto (the Grok shape)
  "alt_vetoes": [                   // umbrella switches that veto this check outright
    {"key": "env.DISABLE_ALL", "env_var": "DISABLE_ALL", "safe_when_set": true}
  ],
  "title": "what fires, in one line",
  "veto": "the exact fix line",     // mandatory
  "severity": "info|low|medium|high|critical"
}
```

Formats: `json` accepts the comment lines and trailing commas Copilot writes;
`toml` is the full language (`toml-lite` is accepted as an alias of the same
reader); `env` is `KEY=VALUE`; `yaml-flat` is `key: value` at column 0 and
refuses anything nested. An artifact beyond its reader degrades the check to
`unknown`, never to silence.

Precedence when evaluating a check: an umbrella veto (artifact, then
environment) → the key or an alternate spelling in the artifact → the
environment variable → unset. The artifact wins over the shell when both
answer; the evidence names whichever did.

The audit header counts an agent as able to upload your data only on
findings of severity medium or higher. Low rows — usage metrics that carry
no code — still print with their veto, but they do not put an agent in the
headline.

## Transcript rules (D3)

`transcript-rules.json` is the same idea for the transcript detector: rule
families as data, a secret-path list, and an allow-list. "Remote" means a
host that is not the machine, not a private network (RFC 1918, link-local,
CGNAT — tailnets), not an IPv6 ULA/link-local address, not a `.local` /
`.internal` / `.lan` / `.home.arpa` / `.ts.net` name, and not allow-listed
(`stax audit --allow-host`, `STAXTRACE_AUDIT_ALLOW_HOSTS`, or
`audit_allow_hosts` in config.json; exact or `*.suffix`). Findings aggregate
per (provider, rule) and name the session the evidence came from. Every
regression the 2026-09-01 review reproduced is a test in
`rust/crates/stax-audit/tests/d3.rs`.
