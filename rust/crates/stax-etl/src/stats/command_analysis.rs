//! Port of `python-legacy: stats/aggregator.py::_command_analysis` — the three
//! numerators `project_mart` materialises.
//!
//! The Python function returns a 21-key dict, of which `_count_interaction_dims`
//! reads exactly three: `commands_followed_by_interruption`, `total_tools_used`
//! and `total_assistant_steps` (v023's interruption-rate and avg
//! tools/steps-per-command numerators). The loop that produces them is ported
//! whole — every accumulator that gates one of the three is present, in the
//! same order, with the same guards — while the 18 keys with no mart reader
//! (`command_details`, the per-tool-count interruption table, the model
//! distribution, the estimated-token averages, the search-tool tally) are not
//! computed. That is a scope boundary, not a shortcut: each is a pure function
//! of the same loop, and `_count_search_tools` / `_is_search_invocation` are
//! the only helpers they would additionally need.
//!
//! # Why the `if tc:` guard matters
//!
//! `total_tools_used` accumulates only inside `if not is_int:` **and** `if tc:`.
//! A non-interrupt command with zero tools contributes to `total_steps` and to
//! the distribution but not to `total_tools`; an interrupt command contributes
//! to neither. Getting either guard wrong moves `total_command_tools` on every
//! project with interruptions — 57 of 243 events-backed rows already carry a
//! zero from the classifier fall-through (DIV-002), which is exactly the kind
//! of number that looks like a porting bug and is not.

use super::classifier::{INTERRUPT_API, INTERRUPT_PREFIX};
use super::enricher::{EnrichedDataset, Interaction, Record, interaction_key};

/// The three numerators `_count_interaction_dims` reads off `_command_analysis`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CommandAnalysis {
    /// `commands_followed_by_interruption` — the interruption-rate numerator.
    pub commands_followed_by_interruption: i64,
    /// `total_tools_used` — the avg-tools-per-command numerator.
    pub total_tools_used: i64,
    /// `total_assistant_steps` — the avg-steps-per-command numerator.
    pub total_assistant_steps: i64,
}

/// `aggregator._is_interrupt_text`.
#[must_use]
pub fn is_interrupt_text(text: &str) -> bool {
    text.starts_with(INTERRUPT_PREFIX) || text.starts_with(INTERRUPT_API)
}

/// `aggregator._command_analysis(records, interactions)`, reduced to the three
/// numerators the mart materialises.
#[must_use]
pub fn command_analysis(ds: &EnrichedDataset) -> CommandAnalysis {
    let records = &ds.records;

    // `ordered = sorted(records, key=lambda r: r.timestamp or "")` — the same
    // stable sort `group_interactions` ran over the same list, so the same
    // permutation.
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| records[a].timestamp.cmp(&records[b].timestamp));
    let ordered: Vec<&Record> = order.into_iter().map(|i| &records[i]).collect();

    // `ix_lut[f"{ix.command.timestamp}|{ix.command.content[:64]}"] = ix`, which
    // is the interaction key itself — last-wins, and post-dedup the keys are
    // unique, so the map is a straight index.
    let mut ix_lut: std::collections::HashMap<&str, &Interaction> =
        std::collections::HashMap::with_capacity(ds.interactions.len());
    for ix in &ds.interactions {
        ix_lut.insert(ix.key.as_str(), ix);
    }

    let mut out = CommandAnalysis::default();

    for (i, r) in ordered.iter().enumerate() {
        if r.kind != "user" || r.has_tool_result {
            continue;
        }
        // Precomputed from the full text at parse time — under `Detail::Lean`
        // `content` is only the key's 64-char prefix.
        let is_int = r.content_is_interrupt;

        let key = interaction_key(r);
        let (tc, steps) = match ix_lut.get(key.as_str()) {
            Some(ix) => (ix.tool_count as i64, ix.assistant_steps as i64),
            None => scan_forward(&ordered, i),
        };

        let followed = next_is_interrupt(&ordered, i);

        if !is_int {
            out.total_assistant_steps += steps;
            if tc != 0 {
                out.total_tools_used += tc;
            }
            if followed {
                out.commands_followed_by_interruption += 1;
            }
        }
    }

    out
}

/// `aggregator._scan_forward` — `(tool_count, assistant_steps)`.
///
/// The model / tool-name / search-tool returns feed `command_details` only.
fn scan_forward(ordered: &[&Record], idx: usize) -> (i64, i64) {
    let mut tc = 0_i64;
    let mut steps = 0_i64;
    for nxt in &ordered[idx + 1..] {
        if nxt.kind == "user" && !nxt.has_tool_result {
            break;
        }
        if nxt.kind == "assistant" {
            steps += 1;
            tc += nxt.tools.len() as i64;
        }
    }
    (tc, steps)
}

/// `aggregator._next_is_interrupt`.
fn next_is_interrupt(ordered: &[&Record], idx: usize) -> bool {
    for nxt in &ordered[idx + 1..] {
        if nxt.kind == "assistant" && nxt.content_is_api_abort {
            return true;
        }
        if nxt.kind == "user" && !nxt.has_tool_result {
            return nxt.content_is_interrupt;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::classifier::{RawEntry, tag};
    use crate::stats::enricher::build;
    use serde_json::{Value, json};

    fn entry(payload: Value) -> RawEntry {
        RawEntry {
            payload,
            session_id: "s".into(),
            provider: "anthropic".into(),
        }
    }

    fn analyse(payloads: Vec<Value>) -> CommandAnalysis {
        let ds = build(tag(payloads.into_iter().map(entry).collect()));
        command_analysis(&ds)
    }

    #[test]
    fn steps_and_tools_accumulate_per_non_interrupt_command() {
        let out = analyse(vec![
            json!({"type": "human", "timestamp": "t1", "message": {"content": "do it"}}),
            json!({"type": "assistant", "timestamp": "t2", "message": {"content": [
                {"type": "tool_use", "id": "a", "name": "Read"},
                {"type": "tool_use", "id": "b", "name": "Edit"},
            ]}}),
            json!({"type": "assistant", "timestamp": "t3", "message": {"content": "done"}}),
        ]);
        assert_eq!(out.total_assistant_steps, 2);
        assert_eq!(out.total_tools_used, 2);
        assert_eq!(out.commands_followed_by_interruption, 0);
    }

    #[test]
    fn interrupt_commands_contribute_to_nothing() {
        let out = analyse(vec![
            json!({"type": "human", "timestamp": "t1",
                   "message": {"content": format!("{INTERRUPT_PREFIX} trailing")}}),
            json!({"type": "assistant", "timestamp": "t2", "message": {"content": [
                {"type": "tool_use", "id": "a", "name": "Read"}]}}),
        ]);
        assert_eq!(out.total_assistant_steps, 0);
        assert_eq!(out.total_tools_used, 0);
    }

    #[test]
    fn a_command_followed_by_an_interrupt_user_turn_counts() {
        let out = analyse(vec![
            json!({"type": "human", "timestamp": "t1", "message": {"content": "go"}}),
            json!({"type": "assistant", "timestamp": "t2", "message": {"content": "working"}}),
            json!({"type": "human", "timestamp": "t3", "message": {"content": INTERRUPT_PREFIX}}),
        ]);
        assert_eq!(out.commands_followed_by_interruption, 1);
        // The interrupt turn itself is a command that is NOT counted.
        assert_eq!(out.total_assistant_steps, 1);
    }

    #[test]
    fn an_abort_signal_assistant_turn_also_counts_as_following() {
        let out = analyse(vec![
            json!({"type": "human", "timestamp": "t1", "message": {"content": "go"}}),
            json!({"type": "assistant", "timestamp": "t2",
                   "message": {"content": format!("  {INTERRUPT_API}  ")}}),
        ]);
        assert_eq!(out.commands_followed_by_interruption, 1);
    }

    #[test]
    fn zero_tool_commands_skip_the_tools_accumulator_but_not_steps() {
        let out = analyse(vec![
            json!({"type": "human", "timestamp": "t1", "message": {"content": "hi"}}),
            json!({"type": "assistant", "timestamp": "t2", "message": {"content": "hello"}}),
        ]);
        assert_eq!(out.total_assistant_steps, 1);
        assert_eq!(out.total_tools_used, 0);
    }

    #[test]
    fn div_002_fall_through_turns_are_assistants_and_never_commands() {
        // A legacy-history entry with no `type` and no `message.role`: Python
        // calls it an assistant, so it becomes a *step* of the preceding
        // command instead of a command of its own.
        let out = analyse(vec![
            json!({"type": "human", "timestamp": "t1", "message": {"content": "go"}}),
            json!({"timestamp": "t2", "message": {"content": "legacy user text"}}),
        ]);
        assert_eq!(out.total_assistant_steps, 1);
    }
}
