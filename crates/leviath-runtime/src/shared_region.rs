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
//! `context_append` on the region write that entry, whatever key the model
//! named, unless the key is another run's; `context_delete` clears it.
//! `sync_shared_regions` copies every live member's entry into every other
//! member's region, once per tick, with the taint of the run that wrote it,
//! and drops the entry of a run that has finished. The daemon drives every
//! run from one world on one thread, so the copies never race; a restart
//! rebuilds them from what each run's own window says it wrote.

use std::collections::HashMap;

use bevy_ecs::prelude::*;
use leviath_core::{Region, RegionEntry, RegionKind, RegionTaint, TaintLevel};
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
/// that region a write lands under the run's own key whatever key it named:
/// a keyed region asks the model for a key, and the model cannot know its run
/// id. A call naming a key another run's entry holds is refused, as is a
/// delete by position and a record bigger than the whole region.
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
    let region = window.get_region(SHARED_REGION);
    let named = args.get("key").and_then(Value::as_str);
    if named.is_some_and(|k| k != own && region.is_some_and(|r| r.get_by_key(k).is_some())) {
        return Err(not_yours(own));
    }
    // An index or a count names entries by position, and every position but
    // one holds somebody else's record.
    if name == "context_delete" && (args.get("index").is_some() || args.get("oldest").is_some()) {
        return Err(not_yours(own));
    }
    if name != "context_delete" {
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
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
    /// The highest taint anywhere in the writer's window: the model wrote the
    /// record knowing all of it, so a run reading it is as tainted.
    pub(crate) taint: TaintLevel,
}

/// An entry the shared region should hold, beside the taint it carries.
pub(crate) type Laid = (RegionEntry, TaintLevel);

/// The taint `region` records for its entry at `index`, or `Public` where it
/// records none.
fn taint_at(region: &Region, index: usize) -> TaintLevel {
    region
        .taint
        .as_ref()
        .and_then(|t| t.entry_taint(index))
        .unwrap_or(TaintLevel::Public)
}

/// What `region` should hold, when it does not already hold exactly that: the
/// run's own entry first, then anything unkeyed as it stands, then `others`
/// in order, as many as fit by tokens and by the region's entry cap.
///
/// `None` when the region is already right, so a tick where nothing moved
/// writes nothing. A region that tracks taint is also wrong when a copied
/// entry's taint is not its writer's.
pub(crate) fn plan(region: &Region, own: &str, others: &[&Record]) -> Option<Vec<Laid>> {
    let indexed = || region.content.iter().enumerate();
    let mut laid: Vec<Laid> = indexed()
        .filter(|(_, e)| e.key.as_deref() == Some(own))
        .chain(indexed().filter(|(_, e)| e.key.is_none()))
        .map(|(i, e)| (e.clone(), taint_at(region, i)))
        .collect();
    let mut used: usize = laid.iter().map(|(e, _)| e.tokens).sum();
    let cap = match &region.kind {
        RegionKind::HashMap { max_entries } => *max_entries,
        _ => None,
    };
    let at = chrono::Utc::now().timestamp();
    for r in others {
        if used + r.tokens <= region.max_tokens && cap.is_none_or(|m| laid.len() < m) {
            used += r.tokens;
            let entry = RegionEntry {
                content: r.content.clone().into(),
                tokens: r.tokens,
                timestamp: at,
                metadata: None,
                kind: leviath_core::EntryKind::default(),
                key: Some(r.run.clone()),
                reasoning: None,
                // Stamped with the rest when the region is laid out.
                seq: 0,
                sticky: false,
            };
            laid.push((entry, r.taint));
        }
    }
    let tracked = region.taint.is_some();
    let now: Vec<_> = indexed()
        .map(|(i, e)| {
            let taint = tracked.then(|| taint_at(region, i));
            (e.key.as_deref(), e.content.as_str(), taint)
        })
        .collect();
    let want: Vec<_> = laid
        .iter()
        .map(|(e, t)| (e.key.as_deref(), e.content.as_str(), tracked.then_some(*t)))
        .collect();
    (now != want).then_some(laid)
}

/// Put `laid`, which [`plan`] sized to fit, in `window`'s shared region.
///
/// A region that tracks taint takes each entry's taint with it, so a record
/// from a run that read something private raises the reader's taint, and the
/// gate holds the reader's outbound tools as it holds the writer's. The taint
/// falls again once the record leaves.
pub(crate) fn apply(window: &mut ContextWindow, laid: Vec<Laid>) {
    let before = window.begin_change(SHARED_REGION);
    let region = window
        .get_region_mut(SHARED_REGION)
        .expect("planned against this region");
    let (entries, taints): (Vec<RegionEntry>, Vec<TaintLevel>) = laid.into_iter().unzip();
    region.current_tokens = entries.iter().map(|e| e.tokens).sum();
    region.content = entries;
    region.restamp_recency();
    region.taint = region
        .taint
        .take()
        .map(|_| RegionTaint::from_entry_taints(taints));
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
                taint: window.overall_taint().unwrap_or(TaintLevel::Public),
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
        if let Some(laid) = plan(region, &state.agent_id, &others) {
            apply(&mut window, laid);
        }
    }
}

#[cfg(test)]
#[path = "shared_region_tests.rs"]
mod tests;
