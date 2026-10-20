//! One region several runs on a project share: each run's record of what it
//! means to do, readable by every other run working on the same project.
//!
//! Runs that work on one project at once (each in its own copy of it, say)
//! cannot see each other until their changes meet, and by then two of them
//! may have rewritten the same file. A region named [`SHARED_REGION`] lets
//! each run say what it plans to change while it works, and lets the others
//! read that before they write.
//!
//! Sharing needs both halves:
//!
//! - the run's graph declares a keyed region named [`SHARED_REGION`], in any
//!   layout a stage that should see it uses, and
//! - the run is spawned with a scope: its delivery metadata names one under
//!   [`SCOPE_KEY`]. Runs given the same scope share; a run with none shares
//!   nothing, and its region holds only what it writes itself.
//!
//! Each run owns one entry, keyed by its run id. `context_write` and
//! `context_append` on the region write that entry and no other, and
//! `context_delete` clears it. `sync_shared_regions` copies every live
//! member's entry into every other member's region, once per tick, and drops
//! the entry of a run that has finished. The daemon drives every run from one
//! world on one thread, so the copies never race; a restart rebuilds them
//! from what each run's own window says it wrote.

use std::collections::HashMap;

use bevy_ecs::prelude::*;
use leviath_core::{Region, RegionKind};
use serde_json::Value;

use crate::components::{AgentState, ContextWindow, Pushed};
use crate::insert::RunSpecC;
use crate::spec::run_spec::RunSpec;

/// The region runs in one scope share. It has to be keyed: it holds one
/// entry per run.
pub const SHARED_REGION: &str = "shared_intents";

/// The delivery metadata key that names a run's scope. Runs spawned with the
/// same value share [`SHARED_REGION`].
pub const SCOPE_KEY: &str = "leviath.context_scope";

/// The scope `spec` was spawned into, when it was given one. A value of only
/// whitespace names none.
pub fn scope_of(spec: &RunSpec) -> Option<&str> {
    spec.delivery
        .metadata
        .get(SCOPE_KEY)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
}

/// Apply one `context_*` tool call for the run `own`.
///
/// A call on any region but [`SHARED_REGION`] is handled as it always is. On
/// that region a write lands under the run's own key whatever key it named,
/// and a call that would write, append to or delete another run's entry is
/// refused, as is a record bigger than the whole region.
pub(crate) fn handle_context_tool(
    name: &str,
    args: &Value,
    own: &str,
    window: &mut ContextWindow,
) -> String {
    if args.get("region").and_then(Value::as_str) != Some(SHARED_REGION) {
        return crate::context_tools::handle_context_tool(name, args, window);
    }
    match own_record_call(name, args, own, window) {
        Ok(args) => crate::context_tools::handle_context_tool(name, &args, window),
        Err(refusal) => refusal,
    }
}

/// The reply to a call that reached for another run's entry.
fn not_yours(own: &str) -> String {
    format!(
        "[error] '{SHARED_REGION}' holds one record per run. Yours is the one under key \
         '{own}'; the others belong to other runs working on the same project and are \
         read-only"
    )
}

/// `args` as a call on the run's own entry, or the refusal when the call
/// reaches past it.
fn own_record_call(
    name: &str,
    args: &Value,
    own: &str,
    window: &ContextWindow,
) -> Result<Value, String> {
    if !matches!(name, "context_write" | "context_append" | "context_delete") {
        return Ok(args.clone());
    }
    let named = args.get("key").and_then(Value::as_str);
    if named.is_some_and(|k| k != own) {
        return Err(not_yours(own));
    }
    // An index or a count names entries by position, and every position but
    // one holds somebody else's record.
    if name == "context_delete" && (args.get("index").is_some() || args.get("oldest").is_some()) {
        return Err(not_yours(own));
    }
    if name != "context_delete" {
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
        let region = window.get_region(SHARED_REGION);
        let record = match (name, region.and_then(|r| r.get_by_key(own))) {
            ("context_append", Some(existing)) => format!("{}\n{content}", existing.content),
            _ => content.to_string(),
        };
        let tokens = leviath_core::estimate_tokens(&record);
        let budget = region.map_or(usize::MAX, |r| r.max_tokens);
        if tokens > budget {
            return Err(format!(
                "[error] your record would be {tokens} tokens, and '{SHARED_REGION}' holds at \
                 most {budget} for every run sharing it. Keep it to the files you plan to \
                 change and what for"
            ));
        }
    }
    let mut args = args.clone();
    // `args` named a region, so it is an object and indexing it cannot fail.
    args["key"] = Value::String(own.to_string());
    Ok(args)
}

/// One run's entry, as the other runs in its scope get it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Record {
    /// The run that wrote it, which is also its key.
    pub(crate) run: String,
    /// What it says.
    pub(crate) content: String,
    /// What it costs.
    pub(crate) tokens: usize,
}

/// The other runs' entries `region` should hold, when it does not already
/// hold exactly those: `others` in order, as many as fit beside the run's own
/// entry and anything unkeyed, by tokens and by the region's entry cap.
///
/// `None` when the region is already right, so a tick where nothing moved
/// writes nothing.
pub(crate) fn plan(region: &Region, own: &str, others: &[&Record]) -> Option<Vec<Record>> {
    let foreign = |e: &&leviath_core::RegionEntry| e.key.as_deref().is_some_and(|k| k != own);
    let mine: Vec<&leviath_core::RegionEntry> =
        region.content.iter().filter(|e| !foreign(e)).collect();
    let mut used: usize = mine.iter().map(|e| e.tokens).sum();
    let mut count = mine.len();
    let cap = match &region.kind {
        RegionKind::HashMap { max_entries } => *max_entries,
        _ => None,
    };
    let mut keep = Vec::new();
    for r in others {
        if used + r.tokens <= region.max_tokens && cap.is_none_or(|m| count < m) {
            used += r.tokens;
            count += 1;
            keep.push((*r).clone());
        }
    }
    let now: Vec<(&str, &str)> = region
        .content
        .iter()
        .filter(foreign)
        .map(|e| (e.key.as_deref().unwrap_or(""), e.content.as_str()))
        .collect();
    let want: Vec<(&str, &str)> = keep
        .iter()
        .map(|r| (r.run.as_str(), r.content.as_str()))
        .collect();
    (now != want).then_some(keep)
}

/// Replace the other runs' entries in `window`'s shared region with `keep`,
/// which [`plan`] sized to fit, leaving the run's own entry where it stands.
pub(crate) fn apply(window: &mut ContextWindow, own: &str, keep: Vec<Record>) {
    let before = window.begin_change(SHARED_REGION);
    let region = window
        .get_region_mut(SHARED_REGION)
        .expect("planned against this region");
    let stale: Vec<String> = region
        .content
        .iter()
        .filter_map(|e| e.key.clone().filter(|k| k != own))
        .collect();
    for key in stale {
        region.remove_by_key(&key);
    }
    for r in keep {
        // Planned to fit, so this neither fails nor evicts.
        let _ = region.upsert_by_key(&r.run, r.content, r.tokens);
    }
    window.current_tokens = window.calculate_tokens();
    window.commit_change(
        leviath_core::ContextCause::Framework,
        before,
        Pushed::Upsert,
    );
}

/// The scope and shared region of a run that takes part in sharing: one
/// still running, spawned into a scope, whose window has the region.
fn member<'a>(
    state: &AgentState,
    spec: &'a RunSpecC,
    window: &'a ContextWindow,
) -> Option<(&'a str, &'a Region)> {
    if crate::pipeline::is_terminal_status(&state.status) {
        return None;
    }
    let scope = scope_of(&spec.0)?;
    let region = window.get_region(SHARED_REGION)?;
    Some((scope, region))
}

/// What `sync_shared_regions` selects.
type SharedQuery = (
    Entity,
    &'static AgentState,
    &'static RunSpecC,
    &'static mut ContextWindow,
);

/// Copy every member's own entry into the shared region of every other
/// member of its scope, and drop the entries of runs that have left it.
///
/// A run leaves its scope when it finishes, however it finishes, and when it
/// is unloaded from the world. Its own window keeps what it wrote, as the
/// record of what it said.
pub(crate) fn sync_shared_regions(mut agents: Query<SharedQuery>) {
    crate::tick_scope::clear();
    let mut scopes: HashMap<String, Vec<Record>> = HashMap::new();
    for (_, state, spec, window) in agents.iter() {
        let Some((scope, region)) = member(state, spec, window) else {
            continue;
        };
        let records = scopes.entry(scope.to_string()).or_default();
        if let Some(entry) = region.get_by_key(&state.agent_id) {
            records.push(Record {
                run: state.agent_id.clone(),
                content: entry.content.as_str().to_string(),
                tokens: entry.tokens,
            });
        }
    }
    for records in scopes.values_mut() {
        records.sort_by(|a, b| a.run.cmp(&b.run));
    }
    for (entity, state, spec, mut window) in agents.iter_mut() {
        let Some((scope, region)) = member(state, spec, &window) else {
            continue;
        };
        crate::tick_scope::enter(entity);
        let others: Vec<&Record> = scopes[scope]
            .iter()
            .filter(|r| r.run != state.agent_id)
            .collect();
        if let Some(keep) = plan(region, &state.agent_id, &others) {
            apply(&mut window, &state.agent_id, keep);
        }
    }
}

#[cfg(test)]
#[path = "shared_region_tests.rs"]
mod tests;
