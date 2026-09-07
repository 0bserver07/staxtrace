//! Port of `python-legacy: stats/enricher.py`.
//!
//! Two consumers with very different appetites share this builder:
//!
//! * `project_mart`'s second pass (wave 3) reads eight `Record` fields and one
//!   predicate over `model`, across every message on the store.
//! * `aggregator.summarise` (wave 5, RS-3-062) reads all nineteen, plus the
//!   whole `Interaction` chain, for one project at a time.
//!
//! Carrying the aggregator's needs on the mart path would mean cloning every
//! tool block's `input` (which holds `Write` file bodies) and holding every
//! `raw_json` blob of a 383K-message store alive for the length of a mart
//! refresh. Rather than fork the builder — two copies of `_parse_entry` is
//! exactly how the two ports drift — the two heavy fields are gated behind
//! [`Detail`]: [`build`] is the mart path, unchanged, and [`build_detailed`] is
//! the aggregator's. Everything else is populated on both paths, because
//! everything else is a small string.
//!
//! # What is still not ported
//!
//! Step 5 (`scan_sessions`) builds `EnrichedDataset.sessions`, a
//! `dict[str, SessionMeta]`. Neither `summarise` nor `formatter.to_dicts` nor
//! the mart path reads it — `_SessionsCollector` recomputes the same fold from
//! `records` — so it is not built here. It is a pure fold and can be added
//! without touching anything else.
//!
//! # Ordering is the contract
//!
//! `group_interactions` sorts by `timestamp or ""` with Python's *stable*
//! `sorted`, and the interaction chain it builds depends on the order of
//! equal-timestamp records. `slice::sort_by` is stable too, and the input order
//! is the SQL row order — which is why both `_refresh_message_dims`' query and
//! `build_enriched_dataset`'s keep their `ORDER BY m.timestamp` exactly as
//! Python wrote them.

use serde_json::Value;

use super::classifier::{INTERRUPT_API, INTERRUPT_PREFIX, TaggedEntry};
use super::pytext::{py_char_prefix, py_str, py_strip, py_truthy};
use super::sha256;

/// How much of each entry to materialise.
///
/// The variants differ in exactly two fields — [`Record::raw_data`] and
/// [`ToolRef::block`] — and in nothing else. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// The mart path: no raw payload, tool blocks reduced to their dedup key.
    Lean,
    /// The aggregator path: every field Python's `Record` carries.
    Full,
}

/// `enricher._usage_from`'s result, plus the optional `reasoning` overlay.
///
/// Python's `tokens` is a `dict[str, int]` whose **key order is observable**:
/// it reaches the wire in `overview.total_tokens`, `session_costs[].tokens`,
/// `command_costs[].tokens`, `token_composition.*` and `daily_stats[].tokens`,
/// and `json.dumps` writes a dict in insertion order. `_usage_from` always
/// inserts the same four keys in `_TOKEN_FIELDS` order and `_parse_entry`
/// appends `reasoning` after them, so the order is fixed and a struct models it
/// exactly — see [`TokenBag::to_json`].
///
/// `touched` is not decoration: `Counter()` that never saw a record serialises
/// as `{}`, not as four zeros, and `_CommandCostCollector` hits that case for
/// every interaction with no responses and no tool results.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenBag {
    /// Whether any record has contributed. A fresh `Counter()` has not.
    pub touched: bool,
    /// `usage.input_tokens`.
    pub input: i64,
    /// `usage.output_tokens`.
    pub output: i64,
    /// `usage.cache_creation_input_tokens`.
    pub cache_creation: i64,
    /// `usage.cache_read_input_tokens`.
    pub cache_read: i64,
    /// The attribution-only reasoning subset of `output`, present only when a
    /// contributing record carried a separable count.
    pub reasoning: Option<i64>,
}

impl TokenBag {
    /// The four-key shape `_usage_from` always returns.
    #[must_use]
    pub fn quad(input: i64, output: i64, cache_creation: i64, cache_read: i64) -> Self {
        Self {
            touched: true,
            input,
            output,
            cache_creation,
            cache_read,
            reasoning: None,
        }
    }

    /// `for k, v in other.items(): self[k] += v`.
    pub fn add(&mut self, other: &Self) {
        if !other.touched {
            return;
        }
        self.touched = true;
        self.input += other.input;
        self.output += other.output;
        self.cache_creation += other.cache_creation;
        self.cache_read += other.cache_read;
        // A key once present in a Counter stays present, so `reasoning` is
        // `Some` if EITHER side had it.
        if self.reasoning.is_some() || other.reasoning.is_some() {
            self.reasoning = Some(self.reasoning.unwrap_or(0) + other.reasoning.unwrap_or(0));
        }
    }

    /// `dict(counter)` — the four keys in `_TOKEN_FIELDS` order, `reasoning`
    /// last, and `{}` when nothing was ever added.
    #[must_use]
    pub fn to_json(self) -> Value {
        let mut map = serde_json::Map::new();
        if !self.touched {
            return Value::Object(map);
        }
        map.insert("input".into(), self.input.into());
        map.insert("output".into(), self.output.into());
        map.insert("cache_creation".into(), self.cache_creation.into());
        map.insert("cache_read".into(), self.cache_read.into());
        if let Some(reasoning) = self.reasoning {
            map.insert("reasoning".into(), reasoning.into());
        }
        Value::Object(map)
    }

    /// The canonical token shape `compute_cost` prices.
    ///
    /// `reasoning` is deliberately not passed through: Python hands the whole
    /// `tokens` dict to `compute_cost`, and no pricer reads a key called
    /// `reasoning` (the OpenAI normalizer looks for `reasoning_output_tokens`,
    /// a different key on a different shape), so the value cannot move a dollar.
    #[must_use]
    pub fn raw(self) -> crate::pricing::RawTokens {
        crate::pricing::RawTokens::canonical(
            self.input,
            self.output,
            self.cache_creation,
            self.cache_read,
        )
    }
}

/// The `{"name", "id", "input"}` dict `_tools_from` builds, kept whole.
///
/// Present only on [`Detail::Full`] builds. `input` is the expensive field —
/// on a `Write` call it is the entire file body — and the mart path reads
/// nothing from any of the three.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBlock {
    /// `blk.get("name", "Unknown")`.
    pub name: Value,
    /// `blk.get("id", "")`.
    pub id: Value,
    /// `blk.get("input", {})`.
    pub input: Value,
}

/// One tool-use block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRef {
    /// A key that compares equal exactly when Python's set membership would.
    ///
    /// Tagged by JSON type so a string `"5"` and a number `5` stay distinct the
    /// way two Python dict keys would. `None` when Python's `t.get("id", "")`
    /// would be falsy — those blocks are never deduplicated and every
    /// occurrence counts.
    pub id_key: Option<String>,
    /// The full block, on [`Detail::Full`] builds only.
    pub block: Option<ToolBlock>,
}

impl ToolRef {
    /// `t.get("name", "?")` as a dict key / display string.
    ///
    /// `_tools_from` always writes a `name` (defaulting to `"Unknown"`), so the
    /// `"?"` fallback in `_ToolCostCollector` / `_RetryCollector` /
    /// `_SessionEfficiencyCollector` is unreachable for records built here —
    /// it is reachable only through `recompute_tz_stats`' `_DictProxy`, which
    /// is not on this path. The fallback is kept anyway, for lean builds.
    #[must_use]
    pub fn name_key(&self) -> String {
        match &self.block {
            Some(b) => super::aggregator::py_dict_key(&b.name),
            None => "?".to_string(),
        }
    }

    /// `t.get("name")` under Python truthiness — the `if nm:` guard in
    /// `_ErrorCostCollector.result`.
    #[must_use]
    pub fn name_if_truthy(&self) -> Option<String> {
        let b = self.block.as_ref()?;
        py_truthy(&b.name).then(|| super::aggregator::py_dict_key(&b.name))
    }
}

/// One fully-parsed log entry (`enricher.Record`).
#[derive(Debug, Clone)]
pub struct Record {
    /// `te.session_id`.
    pub session_id: String,
    /// `te.kind`.
    pub kind: String,
    /// `raw.get("timestamp", "")`.
    pub timestamp: String,
    /// `msg.get("model", "N/A") if msg else "N/A"` — kept as the wire value,
    /// because `_ModelsCollector` uses it as a dict KEY and the formatter
    /// echoes it, and neither coerces.
    pub model: Value,
    /// `_text_from(raw)` — the flattening extraction, not the classifier's.
    ///
    /// Under [`Detail::Lean`] this is only the first 64 chars: the interaction
    /// key's material, which is the one thing the mart path reads from it. The
    /// two predicates the mart path also needs are computed from the full text
    /// before it is dropped — [`Record::content_is_interrupt`] and
    /// [`Record::content_is_api_abort`]. A project's worth of full texts was the
    /// bulk of the resident's per-tick peak.
    pub content: String,
    /// `_is_interrupt_text(content)` over the FULL text — an interruption
    /// marker opens it.
    pub content_is_interrupt: bool,
    /// `content.strip() == INTERRUPT_API` over the FULL text — an aborted-request
    /// turn, which `_next_is_interrupt` looks for after a command.
    pub content_is_api_abort: bool,
    /// `_usage_from(msg)` plus the `reasoning` overlay.
    pub tokens: TokenBag,
    /// `_tools_from(msg)`.
    pub tools: Vec<ToolRef>,
    /// `te.is_error`.
    pub is_error: bool,
    /// `te.error_category`.
    pub error_category: Option<String>,
    /// `te.is_interruption`.
    pub is_interruption: bool,
    /// `_has_result_block(msg)`.
    pub has_tool_result: bool,
    /// `raw.get("uuid", "")`.
    pub uuid: Value,
    /// `raw.get("parentUuid")` — `null` when absent, which is what Python's
    /// bare `.get` returns and what the formatter emits.
    pub parent_uuid: Value,
    /// `raw.get("isSidechain", False)`.
    pub is_sidechain: Value,
    /// `msg.get("id", "") if msg else ""`.
    pub message_id: Value,
    /// `raw.get("cwd", "")`.
    pub cwd: Value,
    /// The whole decoded log line. [`Value::Null`] on [`Detail::Lean`] builds.
    pub raw_data: Value,
    /// `te.provider`.
    pub provider: String,
    /// `_speed_from(msg)` — `"fast"` or `"standard"`.
    pub speed: String,
    /// Whether `tokens["cache_read"]` is truthy — the `cache.hit_rate` numerator.
    pub cache_read_truthy: bool,
    /// Python's `rec.model and rec.model != "N/A"`, precomputed.
    pub model_named: bool,
}

impl Record {
    /// The `(model, speed)` tuple every cost collector groups on, as a key.
    ///
    /// Only meaningful when [`Record::model_named`]; the collectors all check
    /// that first.
    #[must_use]
    pub fn model_speed_key(&self) -> String {
        format!(
            "{}\u{0}{}",
            super::aggregator::py_dict_key(&self.model),
            self.speed
        )
    }
}

/// A user prompt and everything that followed until the next prompt
/// (`enricher.Interaction`).
///
/// `command`, `responses` and `tool_results` are indices into
/// [`EnrichedDataset::records`] rather than owned `Record`s: Python holds
/// references into the same list, and cloning a `Record` here would duplicate
/// every `raw_data` blob a `Full` build carries.
#[derive(Debug, Clone)]
pub struct Interaction {
    /// `sha256(f"{timestamp}|{content[:64]}").hexdigest()[:16]`.
    pub interaction_id: String,
    /// The unhashed material behind [`Interaction::interaction_id`] — the
    /// literal key `_command_analysis` builds its lookup table on, and the
    /// identity this port deduplicates by. See [`build_with`].
    pub key: String,
    /// Index of the command record.
    pub command: usize,
    /// Indices of the assistant records in this chain.
    pub responses: Vec<usize>,
    /// Indices of the tool-result records in this chain.
    pub tool_results: Vec<usize>,
    /// `rec.session_id` of the command.
    pub session_id: String,
    /// `rec.timestamp` of the command.
    pub start_time: String,
    /// The latest assistant timestamp seen, seeded from the command's.
    pub end_time: String,
    /// The last named model any response carried, else `"N/A"`.
    pub model: Value,
    /// `len(tools_used)` after dedup.
    pub tool_count: usize,
    /// `len(responses)`.
    pub assistant_steps: usize,
    /// Never set by the builder — Python's field defaults to `False` and
    /// nothing assigns it. Carried so the shape is not silently narrower.
    pub is_continuation: bool,
    /// Accumulated `tools_used`, deduplicated by step 4.
    pub tools_used: Vec<ToolRef>,
    /// `any(t.get("name") == "Task" for t in deduped)`.
    pub has_task_tool: bool,
}

/// `enricher.EnrichedDataset` (without `sessions` — see the module docs).
#[derive(Debug, Default)]
pub struct EnrichedDataset {
    /// Every record, in input order.
    pub records: Vec<Record>,
    /// The deduplicated interaction chains.
    pub interactions: Vec<Interaction>,
}

/// `enricher.build`, mart-path detail. See [`build_with`].
#[must_use]
pub fn build<I: IntoIterator<Item = TaggedEntry>>(tagged: I) -> EnrichedDataset {
    build_with(tagged, Detail::Lean)
}

/// `enricher.build`, aggregator detail. See [`build_with`].
#[must_use]
pub fn build_detailed<I: IntoIterator<Item = TaggedEntry>>(tagged: I) -> EnrichedDataset {
    build_with(tagged, Detail::Full)
}

/// `enricher.build` — steps 1, 2, 3 and 4 (see the module docs for 5).
///
/// # The one deliberate substitution
///
/// Python identifies an interaction by the truncated SHA-256 in
/// [`Interaction::interaction_id`] and *deduplicates* on it, while
/// `_command_analysis` looks interactions up by the *unhashed* material. This
/// port computes the id (it reaches the wire in five places) but keys the
/// dedup map on the material, which is the same partition of the input unless
/// two distinct materials collide in 64 bits of SHA-256 — at which point Python
/// merges two unrelated interactions and this port does not. The wave-3
/// full-row diff over the live store is the evidence that no such collision
/// exists there.
///
/// `tagged` may be a lazy iterator: step 1 consumes it one entry at a time and
/// keeps only the [`Record`], so a caller that streams rows from the store
/// never holds more than one payload. A `Vec` still works as before.
#[must_use]
pub fn build_with<I: IntoIterator<Item = TaggedEntry>>(
    tagged: I,
    detail: Detail,
) -> EnrichedDataset {
    // step 1 — extract_records
    let records: Vec<Record> = tagged
        .into_iter()
        .map(|te| parse_entry(te, detail))
        .collect();

    // step 2 — group_interactions
    let mut order: Vec<usize> = (0..records.len()).collect();
    // Python: `sorted(self.records, key=lambda r: r.timestamp or "")`, stable.
    order.sort_by(|&a, &b| records[a].timestamp.cmp(&records[b].timestamp));

    let mut interactions: Vec<Interaction> = Vec::new();
    let mut active: Option<Interaction> = None;
    for &i in &order {
        let rec = &records[i];
        if rec.kind == "summary" || rec.kind == "compact_summary" || rec.kind == "task" {
            continue;
        }
        let is_user_command = rec.kind == "user" && !rec.has_tool_result;
        if is_user_command {
            if let Some(prev) = active.take() {
                interactions.push(prev);
            }
            let key = interaction_key(rec);
            active = Some(Interaction {
                interaction_id: sha256::hexdigest(key.as_bytes())[..16].to_string(),
                key,
                command: i,
                responses: Vec::new(),
                tool_results: Vec::new(),
                session_id: rec.session_id.clone(),
                start_time: rec.timestamp.clone(),
                end_time: rec.timestamp.clone(),
                model: Value::String("N/A".to_string()),
                tool_count: 0,
                assistant_steps: 0,
                is_continuation: false,
                tools_used: Vec::new(),
                has_task_tool: false,
            });
            continue;
        }
        let Some(act) = active.as_mut() else { continue };
        if rec.kind == "assistant" {
            act.responses.push(i);
            if rec.model_named {
                act.model = rec.model.clone();
            }
            act.tools_used.extend(rec.tools.iter().cloned());
            if !rec.timestamp.is_empty() && rec.timestamp > act.end_time {
                act.end_time = rec.timestamp.clone();
            }
        } else if rec.has_tool_result {
            act.tool_results.push(i);
        }
    }
    if let Some(last) = active.take() {
        interactions.push(last);
    }

    // step 3 — deduplicate_interactions (insertion-ordered, first-wins on ties)
    let mut keys: Vec<String> = Vec::new();
    let mut best: std::collections::HashMap<String, Interaction> = std::collections::HashMap::new();
    for ix in interactions {
        match best.remove(&ix.key) {
            None => {
                keys.push(ix.key.clone());
                best.insert(ix.key.clone(), ix);
            }
            Some(prev) => {
                // `(ix, prev) if len(ix.responses) > len(prev.responses) else (prev, ix)`
                let (mut winner, loser) = if ix.responses.len() > prev.responses.len() {
                    (ix, prev)
                } else {
                    (prev, ix)
                };
                absorb_tools(&mut winner, &loser);
                best.insert(winner.key.clone(), winner);
            }
        }
    }
    let mut interactions: Vec<Interaction> =
        keys.into_iter().filter_map(|k| best.remove(&k)).collect();

    // step 4 — finalise_tools
    for ix in &mut interactions {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut deduped: Vec<ToolRef> = Vec::with_capacity(ix.tools_used.len());
        for t in ix.tools_used.drain(..) {
            if let Some(id) = &t.id_key {
                if seen.contains(id) {
                    continue;
                }
                seen.insert(id.clone());
            }
            deduped.push(t);
        }
        ix.tool_count = deduped.len();
        ix.assistant_steps = ix.responses.len();
        ix.has_task_tool = deduped.iter().any(|t| {
            t.block
                .as_ref()
                .is_some_and(|b| b.name.as_str() == Some("Task"))
        });
        ix.tools_used = deduped;
    }

    EnrichedDataset {
        records,
        interactions,
    }
}

/// `f"{rec.timestamp}|{rec.content[:64]}"` — the material behind `_make_id`
/// and the literal key `_command_analysis` builds its lookup table on.
#[must_use]
pub fn interaction_key(rec: &Record) -> String {
    format!("{}|{}", rec.timestamp, py_char_prefix(&rec.content, 64))
}

/// `enricher._absorb_tools`.
fn absorb_tools(winner: &mut Interaction, loser: &Interaction) {
    let mut existing: std::collections::HashSet<String> = winner
        .tools_used
        .iter()
        .filter_map(|t| t.id_key.clone())
        .collect();
    for t in &loser.tools_used {
        if let Some(id) = &t.id_key
            && !existing.contains(id)
        {
            winner.tools_used.push(t.clone());
            existing.insert(id.clone());
        }
    }
}

/// `enricher._parse_entry`.
fn parse_entry(te: TaggedEntry, detail: Detail) -> Record {
    let raw = te.payload;
    // `msg = raw.get("message") if isinstance(raw.get("message"), dict) else {}`
    let msg = raw.get("message").and_then(Value::as_object);

    let mut tokens = usage_from(msg);
    let reasoning = reasoning_from(&raw, msg);
    if reasoning > 0 {
        tokens.reasoning = Some(reasoning);
    }

    // `msg.get("model", "N/A") if msg else "N/A"`. `if msg` is truthiness: an
    // empty dict takes the "N/A" branch, as does a non-dict `message`.
    let model = match msg {
        Some(m) if !m.is_empty() => m
            .get("model")
            .cloned()
            .unwrap_or_else(|| Value::String("N/A".to_string())),
        _ => Value::String("N/A".to_string()),
    };
    let model_named = py_truthy(&model) && model.as_str() != Some("N/A");

    let content = text_from(&raw);
    let content_is_interrupt =
        content.starts_with(INTERRUPT_PREFIX) || content.starts_with(INTERRUPT_API);
    let content_is_api_abort = py_strip(&content) == INTERRUPT_API;
    let content = match detail {
        Detail::Lean => py_char_prefix(&content, 64).to_string(),
        Detail::Full => content,
    };
    let tools = tools_from(msg, detail);
    let has_tool_result = has_result_block(msg);
    let speed = speed_from(msg);
    let uuid = raw
        .get("uuid")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    let parent_uuid = raw.get("parentUuid").cloned().unwrap_or(Value::Null);
    let is_sidechain = raw
        .get("isSidechain")
        .cloned()
        .unwrap_or(Value::Bool(false));
    let message_id = match msg {
        Some(m) if !m.is_empty() => m
            .get("id")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
        _ => Value::String(String::new()),
    };
    let cwd = raw
        .get("cwd")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    let timestamp = match raw.get("timestamp") {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        // Python keeps the raw value; a non-string one makes the
        // `sorted(..., key=lambda r: r.timestamp or "")` in step 2 raise
        // `TypeError` as soon as a string timestamp is present too, so Python
        // cannot complete a pass that reaches this branch on a mixed project.
        // Coercing keeps the port total; the gate counts occurrences (zero on
        // the live store — `build_enriched_dataset` overwrites `timestamp`
        // from the authoritative column before we ever see the payload).
        Some(other) => py_str(other),
    };

    Record {
        session_id: te.session_id,
        kind: te.kind,
        timestamp,
        model,
        content,
        content_is_interrupt,
        content_is_api_abort,
        cache_read_truthy: tokens.cache_read != 0,
        tokens,
        tools,
        is_error: te.is_error,
        error_category: te.error_category,
        is_interruption: te.is_interruption,
        has_tool_result,
        uuid,
        parent_uuid,
        is_sidechain,
        message_id,
        cwd,
        raw_data: match detail {
            Detail::Lean => Value::Null,
            Detail::Full => raw,
        },
        provider: te.provider,
        speed,
        model_named,
    }
}

/// `enricher._usage_from` — `usage.get(api_key, 0) or 0` for the four fields.
///
/// # The one coercion
///
/// Python keeps whatever the wire held: a float `1.5` stays `1.5` and rides
/// through every `Counter` sum into the payload as a float. Every token field
/// on the maintainer's store is an integer, and modelling the alternative would
/// mean an int-or-float sum type threaded through nine collectors. A
/// non-integral value is truncated toward zero here and counted — see
/// [`non_integer_token_count`], which the parity binary reports so the
/// assumption is a measurement rather than a hope.
fn usage_from(msg: Option<&serde_json::Map<String, Value>>) -> TokenBag {
    let Some(usage) = msg.and_then(|m| m.get("usage")).and_then(Value::as_object) else {
        return TokenBag::quad(0, 0, 0, 0);
    };
    let field = |key: &str| -> i64 {
        match usage.get(key) {
            // `or 0` — a falsy value (0, 0.0, null, "", false) becomes int 0.
            Some(v) if py_truthy(v) => py_int_lossy(v),
            _ => 0,
        }
    };
    TokenBag::quad(
        field("input_tokens"),
        field("output_tokens"),
        field("cache_creation_input_tokens"),
        field("cache_read_input_tokens"),
    )
}

/// How many non-integer token values have been seen this process. See
/// [`usage_from`].
pub fn non_integer_token_count() -> u64 {
    NON_INTEGER_TOKENS.load(std::sync::atomic::Ordering::Relaxed)
}

static NON_INTEGER_TOKENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn py_int_lossy(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| {
            NON_INTEGER_TOKENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            #[allow(clippy::cast_possible_truncation)]
            {
                n.as_f64().unwrap_or(0.0) as i64
            }
        }),
        _ => {
            NON_INTEGER_TOKENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            0
        }
    }
}

/// `enricher._reasoning_from` — the first wire field carrying a separable
/// reasoning count, else 0.
fn reasoning_from(raw: &Value, msg: Option<&serde_json::Map<String, Value>>) -> i64 {
    if let Some(usage) = msg.and_then(|m| m.get("usage")).and_then(Value::as_object) {
        for key in ["reasoning_output_tokens", "reasoning_tokens"] {
            if let Some(v) = usage.get(key)
                && py_truthy(v)
            {
                return py_int_lossy(v).max(0);
            }
        }
    }
    if let Some(v) = raw
        .get("info")
        .and_then(|i| i.get("last_token_usage"))
        .and_then(|l| l.get("reasoning_output_tokens"))
        && py_truthy(v)
    {
        return py_int_lossy(v).max(0);
    }
    if let Some(v) = raw.get("tokenUsage").and_then(|t| t.get("thinkingTokens"))
        && py_truthy(v)
    {
        return py_int_lossy(v).max(0);
    }
    0
}

/// `enricher._speed_from`.
fn speed_from(msg: Option<&serde_json::Map<String, Value>>) -> String {
    let priority = msg
        .and_then(|m| m.get("usage"))
        .and_then(Value::as_object)
        .and_then(|u| u.get("service_tier"))
        .and_then(Value::as_str)
        == Some("priority");
    if priority { "fast" } else { "standard" }.to_string()
}

/// `enricher._text_from` — readable text from a JSONL entry.
///
/// Distinct from `classifier._surface_text`: this one renders `tool_use` as
/// `[Tool: name]` and recurses into `tool_result` content. Both exist in
/// Python; this is the one that becomes `Record.content`, and therefore the one
/// the interruption tally and the interaction identity are computed from.
#[must_use]
pub fn text_from(raw: &Value) -> String {
    if let Some(s) = raw.get("summary").and_then(Value::as_str) {
        return s.to_string();
    }
    let Some(msg) = raw.get("message").and_then(Value::as_object) else {
        return String::new();
    };
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => flatten_content_blocks(items).join("\n"),
        _ => String::new(),
    }
}

/// `enricher._flatten_content_blocks`.
fn flatten_content_blocks(blocks: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for blk in blocks {
        match blk {
            Value::String(s) => {
                out.push(s.clone());
                continue;
            }
            Value::Object(o) => {
                let bt = o.get("type").and_then(Value::as_str).unwrap_or("");
                match bt {
                    "text" => out.push(
                        o.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    "tool_use" => {
                        // f"[Tool: {blk.get('name', '?')}]" — `name` need not be
                        // a string, and Python's f-string calls `str()` on it.
                        let name = match o.get("name") {
                            None => "?".to_string(),
                            Some(v) => py_str(v),
                        };
                        out.push(format!("[Tool: {name}]"));
                    }
                    "tool_result" => match o.get("content") {
                        Some(Value::String(s)) => out.push(s.clone()),
                        Some(Value::Array(inner)) => out.extend(flatten_content_blocks(inner)),
                        _ => {}
                    },
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

/// `enricher._tools_from`.
fn tools_from(msg: Option<&serde_json::Map<String, Value>>, detail: Detail) -> Vec<ToolRef> {
    let Some(msg) = msg else { return Vec::new() };
    let Some(body) = msg.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    body.iter()
        .filter_map(Value::as_object)
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|o| ToolRef {
            // `blk.get("id", "")` then `if tid:` — falsy ids never dedup.
            id_key: match o.get("id") {
                Some(v) if py_truthy(v) => Some(match v {
                    Value::String(s) => format!("s:{s}"),
                    other => format!("v:{other}"),
                }),
                _ => None,
            },
            block: match detail {
                Detail::Lean => None,
                Detail::Full => Some(ToolBlock {
                    name: o
                        .get("name")
                        .cloned()
                        .unwrap_or_else(|| Value::String("Unknown".to_string())),
                    id: o
                        .get("id")
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new())),
                    input: o
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
                }),
            },
        })
        .collect()
}

/// `enricher._has_result_block`.
#[must_use]
pub fn has_result_block(msg: Option<&serde_json::Map<String, Value>>) -> bool {
    msg.and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|body| {
            body.iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// `_has_result_block` over a raw payload's `message`, the shape both
/// `_count_message_dims` and `command.py::_is_user_command` use.
#[must_use]
pub fn has_result_block_of(raw: &Value) -> bool {
    has_result_block(raw.get("message").and_then(Value::as_object))
}

/// `_tools_from(msg)` over a raw payload — non-empty iff the turn used tools.
#[must_use]
pub fn tools_from_raw(raw: &Value) -> Vec<ToolRef> {
    tools_from(raw.get("message").and_then(Value::as_object), Detail::Lean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::classifier::{RawEntry, tag};
    use serde_json::json;

    fn entry(payload: Value) -> RawEntry {
        RawEntry {
            payload,
            session_id: "s".into(),
            provider: "anthropic".into(),
        }
    }

    #[test]
    fn text_from_flattens_tool_blocks_and_joins_with_newlines() {
        let raw = json!({"message": {"content": [
            {"type": "text", "text": "one"},
            {"type": "tool_use", "name": "Read"},
            {"type": "tool_result", "content": [{"type": "text", "text": "two"}]},
            "bare",
        ]}});
        assert_eq!(text_from(&raw), "one\n[Tool: Read]\ntwo\nbare");
        assert_eq!(text_from(&json!({"summary": "s"})), "s");
        assert_eq!(text_from(&json!({"message": {"content": "flat"}})), "flat");
        assert_eq!(text_from(&json!({})), "");
        // missing name renders the literal '?'
        assert_eq!(
            text_from(&json!({"message": {"content": [{"type": "tool_use"}]}})),
            "[Tool: ?]"
        );
    }

    #[test]
    fn interactions_chain_from_user_turns_and_count_assistant_steps() {
        let tagged = tag(vec![
            entry(json!({"type": "human", "timestamp": "t1", "message": {"content": "ask"}})),
            entry(json!({"type": "assistant", "timestamp": "t2", "message": {
                "content": [{"type": "tool_use", "id": "a", "name": "Read"}]}})),
            entry(json!({"type": "assistant", "timestamp": "t3", "message": {
                "content": [{"type": "tool_use", "id": "b", "name": "Edit"}]}})),
            entry(json!({"type": "human", "timestamp": "t4", "message": {"content": "again"}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.records.len(), 4);
        assert_eq!(ds.interactions.len(), 2);
        assert_eq!(ds.interactions[0].responses.len(), 2);
        assert_eq!(ds.interactions[0].assistant_steps, 2);
        assert_eq!(ds.interactions[0].tool_count, 2);
        assert_eq!(ds.interactions[1].responses.len(), 0);
        // The command index points back into `records`, in INPUT order.
        assert_eq!(ds.interactions[0].command, 0);
        assert_eq!(ds.interactions[1].command, 3);
    }

    #[test]
    fn tool_result_turns_do_not_start_an_interaction() {
        let tagged = tag(vec![
            entry(json!({"type": "human", "timestamp": "t1", "message": {"content": "ask"}})),
            entry(json!({"type": "human", "timestamp": "t2", "message": {
                "content": [{"type": "tool_result", "content": "out"}]}})),
            entry(json!({"type": "assistant", "timestamp": "t3", "message": {"content": "ok"}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions.len(), 1);
        assert_eq!(ds.interactions[0].responses.len(), 1);
        assert_eq!(ds.interactions[0].tool_results, vec![1]);
    }

    #[test]
    fn duplicate_interactions_merge_with_the_longer_response_chain_winning() {
        // Same timestamp + same first 64 chars = same identity. Every record
        // shares a timestamp so the stable sort leaves them in input order and
        // both chains are non-empty — the shape the merge actually has to
        // decide between.
        let dup = json!({"type": "human", "timestamp": "t", "message": {"content": "ask"}});
        let tagged = tag(vec![
            entry(dup.clone()),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {
                "content": [{"type": "tool_use", "id": "a", "name": "Read"}]}})),
            entry(dup),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {
                "content": [{"type": "tool_use", "id": "b", "name": "Edit"}]}})),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {"content": "more"}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions.len(), 1);
        // Chain 1 has 1 response, chain 2 has 2 — strictly greater, so chain 2
        // wins. (`>` not `>=`: a tie keeps the FIRST.)
        assert_eq!(ds.interactions[0].responses.len(), 2);
        // …and the loser's tool is absorbed rather than dropped.
        assert_eq!(ds.interactions[0].tool_count, 2);
    }

    #[test]
    fn a_tie_on_response_count_keeps_the_first_chain() {
        let dup = json!({"type": "human", "timestamp": "t", "message": {"content": "ask"}});
        let tagged = tag(vec![
            entry(dup.clone()),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {
                "content": [{"type": "tool_use", "id": "a", "name": "Read"}]}})),
            entry(dup),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {
                "content": [{"type": "tool_use", "id": "b", "name": "Edit"}]}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions.len(), 1);
        assert_eq!(ds.interactions[0].responses.len(), 1);
        assert_eq!(ds.interactions[0].tool_count, 2);
    }

    #[test]
    fn tools_without_an_id_are_never_deduplicated() {
        let tagged = tag(vec![
            entry(json!({"type": "human", "timestamp": "t1", "message": {"content": "ask"}})),
            entry(
                json!({"type": "assistant", "timestamp": "t2", "message": {"content": [
                    {"type": "tool_use", "name": "Read"},
                    {"type": "tool_use", "name": "Read"},
                    {"type": "tool_use", "id": "x", "name": "Edit"},
                    {"type": "tool_use", "id": "x", "name": "Edit"},
                ]}}),
            ),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions[0].tool_count, 3);
    }

    #[test]
    fn cache_read_truthiness_follows_the_wire_value() {
        let mk = |usage: Value| {
            let ds = build(tag(vec![entry(
                json!({"type": "assistant", "message": {"usage": usage}}),
            )]));
            ds.records[0].cache_read_truthy
        };
        assert!(mk(json!({"cache_read_input_tokens": 12})));
        assert!(!mk(json!({"cache_read_input_tokens": 0})));
        assert!(!mk(json!({"cache_read_input_tokens": null})));
        assert!(!mk(json!({})));
    }

    #[test]
    fn stable_sort_keeps_equal_timestamps_in_input_order() {
        // Two user turns sharing a timestamp: the chain that forms depends on
        // which one the sort leaves first, and Python's `sorted` is stable.
        let tagged = tag(vec![
            entry(json!({"type": "human", "timestamp": "t", "message": {"content": "first"}})),
            entry(json!({"type": "assistant", "timestamp": "t", "message": {"content": "reply"}})),
            entry(json!({"type": "human", "timestamp": "t", "message": {"content": "second"}})),
        ]);
        let ds = build(tagged);
        assert_eq!(ds.interactions.len(), 2);
        assert_eq!(ds.interactions[0].key, "t|first");
        assert_eq!(ds.interactions[0].responses.len(), 1);
        assert_eq!(ds.interactions[1].responses.len(), 0);
    }

    #[test]
    fn detail_gates_exactly_two_fields() {
        let payload = json!({"type": "assistant", "timestamp": "t", "message": {
            "model": "claude-opus-4-8",
            "usage": {"input_tokens": 3, "output_tokens": 4,
                      "cache_creation_input_tokens": 5, "cache_read_input_tokens": 6},
            "content": [{"type": "tool_use", "id": "x", "name": "Read",
                         "input": {"file_path": "/a"}}]}});
        let lean = build(tag(vec![entry(payload.clone())]));
        let full = build_detailed(tag(vec![entry(payload)]));
        assert_eq!(lean.records[0].raw_data, Value::Null);
        assert!(full.records[0].raw_data.is_object());
        assert!(lean.records[0].tools[0].block.is_none());
        assert_eq!(full.records[0].tools[0].name_key(), "Read");
        // Everything else is identical.
        for r in [&lean.records[0], &full.records[0]] {
            assert_eq!(r.tokens, TokenBag::quad(3, 4, 5, 6));
            assert_eq!(r.model.as_str(), Some("claude-opus-4-8"));
            assert!(r.model_named);
            assert_eq!(r.speed, "standard");
        }
    }

    #[test]
    fn the_interaction_id_is_the_truncated_sha256_of_the_key() {
        let ds = build(tag(vec![entry(
            json!({"type": "human", "timestamp": "2026-01-01T00:00:00+00:00",
                   "message": {"content": "hello"}}),
        )]));
        assert_eq!(ds.interactions[0].key, "2026-01-01T00:00:00+00:00|hello");
        assert_eq!(ds.interactions[0].interaction_id, "85e40597f50c27d6");
    }

    #[test]
    fn token_bag_merges_reasoning_only_once_a_record_carries_it() {
        let mut acc = TokenBag::default();
        assert_eq!(acc.to_json(), json!({}));
        acc.add(&TokenBag::quad(1, 2, 3, 4));
        assert_eq!(
            acc.to_json(),
            json!({"input": 1, "output": 2, "cache_creation": 3, "cache_read": 4})
        );
        let mut with_reasoning = TokenBag::quad(0, 10, 0, 0);
        with_reasoning.reasoning = Some(7);
        acc.add(&with_reasoning);
        assert_eq!(
            acc.to_json(),
            json!({"input": 1, "output": 12, "cache_creation": 3,
                   "cache_read": 4, "reasoning": 7})
        );
        // Once present, the key stays present.
        acc.add(&TokenBag::quad(1, 1, 1, 1));
        assert_eq!(acc.reasoning, Some(7));
    }

    #[test]
    fn speed_is_fast_only_for_the_priority_tier() {
        let mk = |tier: Value| {
            let ds = build(tag(vec![entry(
                json!({"type": "assistant", "message": {"usage": {"service_tier": tier}}}),
            )]));
            ds.records[0].speed.clone()
        };
        assert_eq!(mk(json!("priority")), "fast");
        assert_eq!(mk(json!("standard")), "standard");
        assert_eq!(mk(Value::Null), "standard");
    }

    #[test]
    fn reasoning_takes_the_first_wire_field_that_has_one() {
        let mk = |payload: Value| {
            let ds = build(tag(vec![entry(payload)]));
            ds.records[0].tokens.reasoning
        };
        assert_eq!(
            mk(json!({"type": "assistant",
                      "message": {"usage": {"reasoning_output_tokens": 9}}})),
            Some(9)
        );
        assert_eq!(
            mk(json!({"type": "assistant", "message": {"usage": {"reasoning_tokens": 4}}})),
            Some(4)
        );
        assert_eq!(
            mk(json!({"type": "assistant",
                      "info": {"last_token_usage": {"reasoning_output_tokens": 11}}})),
            Some(11)
        );
        assert_eq!(
            mk(json!({"type": "assistant", "tokenUsage": {"thinkingTokens": 3}})),
            Some(3)
        );
        // Zero is falsy, so the key is never added.
        assert_eq!(
            mk(json!({"type": "assistant",
                      "message": {"usage": {"reasoning_output_tokens": 0}}})),
            None
        );
    }
}
