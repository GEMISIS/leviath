//! The checks that read a graph as a shape: does every stage it names exist,
//! can the run reach an output, does a tool it advertises actually resolve.
//!
//! Split from the security checks next door because these answer "will this
//! agent work" and those answer "should this agent be allowed to".

use super::*;
use leviath_runtime::pipeline::model_key;
use leviath_runtime::spec::graph::{
    Budget, EdgeCarry, EdgeCondition, EdgeDef, RegionKind, StageDef, WorkerSource,
};

/// Fields the stage left to a default: its models and `max_iterations`.
pub(super) fn lint_declarations(stage: &StageDef) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    if stage.model.models.is_empty() {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "stage-missing-model",
                "names no model, so the stage runs on your configured \
                 default_provider, whatever that is"
                    .to_string(),
            )
            .in_stage(stage.name.as_str())
            .with_fix(format!(
                "add model = {{ models = [{{ provider = \"...\", model = \"...\" }}] }} \
                 to the stage named '{}'",
                stage.name
            )),
        );
    }

    // A fan_out stage does not run inference itself - it splits work and waits
    // on its workers - so it has no iteration count to cap.
    let counts_iterations = !matches!(stage.mode, StageMode::FanOut(_));
    if counts_iterations && stage.max_iterations.is_none() {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "stage-missing-max-iterations",
                "no max_iterations, so the stage is unbounded unless your config \
                 sets [limits] default_max_iterations"
                    .to_string(),
            )
            .in_stage(stage.name.as_str())
            .with_fix("give the stage a max_iterations it should never reach"),
        );
    }

    findings
}

/// Tool names that resolve to nothing, and permissions for tools the stage
/// never granted.
pub(super) fn lint_tools(stage: &StageDef, env: &LintEnv) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let groups = tool_groups(stage);

    // Which group a name written elsewhere in the stage would fall under, or
    // `None` when the install was never asked (or the name matches nothing),
    // in which case a group-aware check says nothing rather than guessing.
    let source_of = |name: &str| -> Option<ToolGroup> {
        if name.contains("__") {
            return Some(ToolGroup::Mcp);
        }
        leviath_tools::tool_name_spellings(name).find_map(|n| env.tool_sources.get(n).copied())
    };
    let group_grants = |name: &str| {
        source_of(name).is_some_and(|source| groups.iter().any(|g| covers(*g, source)))
    };

    if !env.known_tools.is_empty() {
        for tool in named_tools(stage) {
            // `server__tool` is an MCP name, and every MCP name has that shape:
            // advertised names are always server-qualified, so the test is
            // exact rather than a heuristic. Such a name resolves only once
            // that server is installed and connected, which is not a property
            // of the blueprint, so it is never this check's business.
            if tool.contains("__") || env.known_tools.contains(tool) {
                continue;
            }
            findings.push(
                LintFinding::new(
                    LintSeverity::Error,
                    "unknown-tool",
                    format!(
                        "grants '{tool}', which is not a built-in, a sub-agent \
                         tool, or one of this agent's own tools/*.rhai"
                    ),
                )
                .in_stage(stage.name.as_str())
                .with_fix("check the spelling, or drop the entry"),
            );
        }
    }

    // A required tool the stage never grants is a tool the model never sees,
    // whatever the name promised. Whether a group like `@scripts` reaches it
    // depends on where the tool lives, which only an install can say, so the
    // check runs only when the install was asked.
    if !groups.is_empty() && !env.tool_sources.is_empty() {
        let named: Vec<&str> = named_tools(stage).collect();
        for tool in &stage.required_tools {
            let tool = tool.as_str();
            let by_name = named
                .iter()
                .any(|n| canonical_tool_name(n) == canonical_tool_name(tool));
            if by_name || group_grants(tool) {
                continue;
            }
            let granted = groups
                .iter()
                .map(|g| group_token(*g))
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(
                LintFinding::new(
                    LintSeverity::Error,
                    "required-tool-not-granted",
                    format!(
                        "requires '{tool}' but grants it neither by name nor through \
                         {granted}, so the model never sees it"
                    ),
                )
                .in_stage(stage.name.as_str())
                .with_fix(format!(
                    "add '{tool}' to the stage's tools, or grant the group it belongs to"
                )),
            );
        }
    }

    // A stage granting a whole connector has a tool set nobody can enumerate
    // here: it is whatever that server advertises at spawn, which is the point
    // of naming the server. So a permission that looks orphaned might name a
    // tool the connector grants, and the check has nothing to tell it apart
    // with. Skipped rather than guessed, the same way an MCP tool name is never
    // reported as unknown above.
    if !stage.connectors.is_empty() {
        return findings;
    }

    let granted: HashSet<&str> = named_tools(stage).collect();
    for tool in stage.tool_permissions.keys().map(|t| t.as_str()) {
        if granted.contains(tool) {
            continue;
        }
        // A group is a grant too, of a set only the install can spell out.
        // Where it can, a permission the group reaches is not orphaned; where
        // it cannot (no inventory), the check stays quiet for the same reason
        // it does under a connector.
        if !groups.is_empty() && (env.tool_sources.is_empty() || group_grants(tool)) {
            continue;
        }
        findings.push(
            LintFinding::new(
                LintSeverity::Error,
                "orphan-stage-permission",
                format!(
                    "sets a permission for '{tool}', which it does not grant in \
                     its tools - it reads as a grant and is not one"
                ),
            )
            .in_stage(stage.name.as_str())
            .with_fix(format!(
                "add '{tool}' to the stage's tools, or drop the permission"
            )),
        );
    }

    // A stage that routes tool output into a knowledge region tells the model,
    // in the pointer left behind, that the output lives in that region. If it
    // also hands the model a file-reading tool and no `context_read`, the only
    // read verb in reach points at the filesystem - and models take it, aiming
    // `read_file` at the region name. Measured over 152 local runs: 90 of 168
    // failed `read_file` calls were a region name where a path belongs.
    //
    // A Warning rather than an Error: the runtime names the region's heading
    // in the pointer and corrects the mistake on the error, so this is an
    // ergonomics gap and not a broken blueprint.
    let routes_to_region = stage.tool_routing.as_ref().is_some_and(|r| {
        r.default_region.as_str() != "conversation"
            || r.tool_regions
                .values()
                .any(|v| v.as_str() != "conversation")
    });
    let all_builtins = grants_all_builtins(stage);
    let reads_files =
        all_builtins || granted.contains("read_file") || granted.contains("read_files");
    let reads_context = all_builtins || granted.contains("context_read");
    if routes_to_region && reads_files && !reads_context {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "routing-without-region-read",
                "routes tool output into a context region and grants a file-reading \
                 tool but not 'context_read', so the only way the model can act on \
                 \"go and read that region\" is to aim read_file at the region name"
                    .to_string(),
            )
            .in_stage(stage.name.as_str())
            .with_fix("add 'context_read' to the stage's tools".to_string()),
        );
    }

    findings
}

/// Human-in-the-loop tools offered by a stage that runs with nobody attached.
pub(super) fn lint_blocking_tools(stage: &StageDef) -> Vec<LintFinding> {
    // Only autonomous stages are a problem: the interactive modes are where a
    // person is expected, and the one tool a fan_out stage carries is its own
    // `fan_out`, which blocks on nobody.
    if !matches!(stage.mode, StageMode::Autonomous) || stage.allow_blocking_tools {
        return Vec::new();
    }
    // A tool kept in `required_tools` is the same statement of intent
    // `allow_blocking_tools` makes, made one tool at a time - and it is the
    // one that also survives an unattended run, so it is worth more.
    //
    // Canonicalised on both sides, as the runtime does: a stage granting
    // `bash` and keeping `shell` is one decision, not two.
    let required = |tool: &str| {
        stage
            .required_tools
            .iter()
            .any(|r| canonical_tool_name(r.as_str()) == canonical_tool_name(tool))
    };
    let mut findings: Vec<LintFinding> = named_tools(stage)
        .filter(|t| BLOCKING_INTERACTION_TOOLS.contains(&canonical_tool_name(t)))
        .filter(|t| !required(t))
        .map(|tool| {
            LintFinding::new(
                LintSeverity::Warning,
                "blocking-tool-in-autonomous-stage",
                format!(
                    "is autonomous but grants '{tool}', which suspends the run \
                     until a person answers"
                ),
            )
            .in_stage(stage.name.as_str())
            .with_fix(
                "drop the tool, switch the stage to an interactive mode, list it in \
                 required_tools so it survives an unattended run too, or set \
                 allow_blocking_tools = true to say you meant it",
            )
        })
        .collect();

    // A group that reaches the built-ins reaches every blocking tool at once.
    // One finding for the group rather than five for its members: the fix is
    // the same whichever member is named, and a list that long is skimmed.
    let group = tool_groups(stage)
        .into_iter()
        .find(|g| covers(*g, ToolGroup::Builtin));
    if let Some(group) = group {
        let members: Vec<&str> = BLOCKING_INTERACTION_TOOLS
            .iter()
            .copied()
            .filter(|t| !required(t))
            .collect();
        if !members.is_empty() {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "blocking-tool-in-autonomous-stage",
                    format!(
                        "is autonomous but grants '{}', which includes {}; each \
                         suspends the run until a person answers",
                        group_token(group),
                        members.join(", ")
                    ),
                )
                .in_stage(stage.name.as_str())
                .with_fix(
                    "name the tools you want instead of the group, switch the stage to an \
                     interactive mode, list the blocking tools you need in required_tools \
                     so they survive an unattended run too, or set allow_blocking_tools = \
                     true to say you meant it",
                ),
            );
        }
    }
    findings
}

/// A stage's own output declarations: a demand it cannot meet, a shape nothing
/// will read, or a reporting stage that can also change the workspace.
pub(super) fn lint_output_stage(stage: &StageDef) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let grants_submit = named_tools(stage).any(|t| canonical_tool_name(t) == SUBMIT_OUTPUT_TOOL);

    if stage.require_output && !grants_submit {
        findings.push(
            LintFinding::new(
                LintSeverity::Error,
                "output-missing-submit-tool",
                format!("must produce a final output but does not grant '{SUBMIT_OUTPUT_TOOL}'"),
            )
            .in_stage(stage.name.as_str())
            .with_fix(format!(
                "add '{SUBMIT_OUTPUT_TOOL}' to the stage's tools, or use mode = \"output\""
            )),
        );
    }

    // A declared shape nobody is obliged to produce is a wish, not a contract:
    // the tool description carries it, and the agent may still finish without
    // calling the tool at all. An output stage is required to produce one:
    // the resolver sets `require_output` on every stage in that mode.
    if stage.output.is_some() && !stage.require_output && stage.mode != StageMode::Output {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "output-shape-not-required",
                "declares an output shape but is not required to produce one, so the run may \
                 finish with nothing"
                    .to_string(),
            )
            .in_stage(stage.name.as_str())
            .with_fix("set require_output = true, or move the shape to the stage that submits"),
        );
    }

    // An output stage summarizes work; one that can also change files invites
    // the model to keep working where it was meant to report.
    if stage.mode == StageMode::Output {
        let modifying = named_tools(stage)
            .filter(|t| MODIFYING_TOOLS.contains(&canonical_tool_name(t)))
            .map(|tool| format!("'{tool}', which changes the workspace"));
        // A group reaching the built-ins carries every modifying tool with it,
        // so it is named once, as the group, rather than once per member.
        let grouped = tool_groups(stage)
            .into_iter()
            .find(|g| covers(*g, ToolGroup::Builtin))
            .map(|g| {
                format!(
                    "'{}', which includes every tool that changes the workspace",
                    group_token(g)
                )
            });
        for what in modifying.chain(grouped) {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "output-stage-can-modify",
                    format!("is an output stage but grants {what}"),
                )
                .in_stage(stage.name.as_str())
                .with_fix(
                    "drop the tool: an output stage reports what happened, and work done here \
                     lands after the review that was meant to check it",
                ),
            );
        }
    }
    findings
}

/// An output stage whose models cannot call tools and which declares no
/// file for them to hand back.
///
/// `submit_output` is a tool, so a stage on an image model or a 3D generator
/// (every compiled row for those says `supports_tools = false`) can never
/// call it. Such a stage answers only one way: the model's produced part,
/// routed into a region with `output_routing` and declared under the stage's
/// `output.artifacts`, is emitted as the run's answer. Without both, the
/// runtime re-enters the stage and nudges it for text it cannot write - up to
/// six re-submitted jobs on a paid API - and the run ends with nothing. Only
/// providers with compiled tables are judged; an open route is taken on trust.
pub(super) fn lint_output_stage_can_answer(stage: &StageDef) -> Vec<LintFinding> {
    if !(stage.require_output || stage.mode == StageMode::Output) || !all_pinned(&stage.model) {
        return Vec::new();
    }
    let catalog = leviath_providers::capabilities::builtin_catalog();
    let judged: Vec<(&str, &str)> = stage.model.models.iter().map(route).collect();
    let tool_less: Vec<String> = judged
        .iter()
        .filter(|(provider, model)| {
            catalog.iter().any(|row| {
                row.provider == *provider && row.id == *model && !row.capabilities.supports_tools
            })
        })
        .map(|(provider, model)| format!("{provider}/{model}"))
        .collect();
    if tool_less.len() != judged.len() {
        return Vec::new();
    }
    let declares_file = stage
        .output
        .as_ref()
        .is_some_and(|o| !o.artifacts.is_empty());
    let routes_part = !stage.output_routing.is_empty();
    if declares_file && routes_part {
        return Vec::new();
    }
    let missing = match (declares_file, routes_part) {
        (false, false) => "declares no artifact and routes no produced part",
        (false, true) => "routes its produced part but declares no artifact for it",
        _ => "declares an artifact but routes no produced part into a region",
    };
    vec![
        LintFinding::new(
            LintSeverity::Error,
            "output-stage-cannot-answer",
            format!(
                "must produce a final output, but its models ({}) cannot call tools, so \
                 submit_output is out of reach, and it {missing}",
                tool_less.join(", ")
            ),
        )
        .in_stage(stage.name.as_str())
        .with_fix(
            "declare the file under the stage's output.artifacts and route the model's \
             part into a region with the stage's output_routing, so the runtime emits it \
             as the answer; or list a model that calls tools",
        ),
    ]
}

/// Whether an edge is one a stage leaves by in the ordinary course of things:
/// when it ends, or when the model picks it.
fn is_normal(edge: &EdgeDef) -> bool {
    matches!(edge.when, EdgeCondition::Always | EdgeCondition::LlmChoice)
}

/// Stages whose every normal exit can run out of `max_revisits` budget.
///
/// A stage moves on along its `always`/`llm_choice` edges; an edge whose
/// target has `max_revisits` stops being followable once the budget is spent.
/// When EVERY normal edge is like that, a long enough run strands the stage
/// with nowhere to go, which the engine reports as a dead-end *error* rather
/// than as `complete` from the middle of the graph with the output stage still
/// pending. The live shape: a wide-researcher bouncing deep_dive → compare
/// until compare's budget runs out, with nothing produced.
///
/// The fix is one un-exhaustible way forward: a `dead_end` (or `error`) edge
/// to a stage without `max_revisits`.
pub(super) fn lint_dead_end_possible(graph: &RunGraph) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    for stage in &graph.stages {
        let edges: Vec<&EdgeDef> = graph.edges_from(stage.name.as_str()).collect();
        let normal: Vec<&&EdgeDef> = edges.iter().filter(|e| is_normal(e)).collect();
        if normal.is_empty() {
            continue; // terminal (or conditioned-only) stage: nothing to strand
        }
        let all_exhaustible = normal.iter().all(|e| {
            graph
                .stage(e.to.as_str())
                .is_none_or(|t| t.max_revisits.is_some())
        });
        // An escape the runtime actually consults on this path: a dead end
        // resolves down a `dead_end` edge, then an `error` edge, so either
        // satisfies the check - provided its own target can still be
        // entered, or it is no escape at all.
        let has_escape = edges.iter().any(|e| {
            matches!(e.when, EdgeCondition::DeadEnd | EdgeCondition::Error)
                && graph
                    .stage(e.to.as_str())
                    .is_some_and(|t| t.max_revisits.is_none())
        });

        if all_exhaustible && !has_escape {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "dead-end-possible",
                    "can strand the run: every normal edge's target has a max_revisits \
                     budget, and once they are all spent the run errors as dead-ended"
                        .to_string(),
                )
                .in_stage(stage.name.as_str())
                .with_fix(
                    "add an edge with when = \"dead_end\" to a stage without max_revisits \
                     (the output stage, usually). It is taken only when the graph would \
                     otherwise strand, so it is not a route the model can choose early - \
                     unlike a plain edge to the same stage, which is offered on every visit",
                ),
            );
        }
    }
    findings
}

/// Output stages nothing can reach, and the upstream `allow_complete` that is
/// the usual reason.
///
/// The second half is the one that fails quietly. `allow_complete` offers the
/// model a "DONE" it may pick instead of routing onward, and it is appended even
/// to a stage's custom `transition_prompt` - so a stage can offer an exit its own
/// prompt never mentions. A run that takes it ends with no answer and looks
/// exactly like success.
pub(super) fn lint_output_reachable(graph: &RunGraph) -> Vec<LintFinding> {
    let outputs: Vec<&StageDef> = graph
        .stages
        .iter()
        .filter(|s| s.mode == StageMode::Output)
        .collect();
    if outputs.is_empty() {
        return Vec::new();
    }
    let mut findings = Vec::new();
    let entry = graph.entry_stage().map(|s| s.name.as_str());

    for output in &outputs {
        let reached = graph
            .edges
            .iter()
            .any(|e| e.from != output.name && e.to == output.name);
        if !reached && entry != Some(output.name.as_str()) {
            findings.push(
                LintFinding::new(
                    LintSeverity::Error,
                    "output-unreachable",
                    "is an output stage no edge routes to, so the run can never produce one"
                        .to_string(),
                )
                .in_stage(output.name.as_str())
                .with_fix(format!(
                    "add an edge to '{}' from whichever stage finishes the work",
                    output.name
                )),
            );
        }
    }

    for stage in &graph.stages {
        if stage.allow_complete && stage.mode != StageMode::Output {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "allow-complete-skips-output",
                    "may end the run itself, so the model can finish here and never reach the \
                     output stage"
                        .to_string(),
                )
                .in_stage(stage.name.as_str())
                .with_fix(
                    "drop allow_complete and route to the output stage instead - the run then \
                     still explains what it did",
                ),
            );
        }
    }
    findings
}

/// Graph shape: stages the entry can never reach, and cycles with no revisit
/// cap. Both only mean anything for a graph that declares edges at all.
pub(super) fn lint_graph(graph: &RunGraph) -> Vec<LintFinding> {
    if graph.edges.is_empty() {
        return Vec::new();
    }
    // The entry as written, even when it names no stage: then nothing is
    // reachable, which is what is worth saying about such a graph.
    let Some(entry) = graph
        .entry
        .as_ref()
        .or_else(|| graph.stages.first().map(|s| &s.name))
        .map(ToString::to_string)
    else {
        return Vec::new();
    };

    // Breadth-first from the entry stage; whatever is left over is orphaned.
    let mut reachable: HashSet<String> = HashSet::new();
    let mut queue = std::collections::VecDeque::from([entry.clone()]);
    while let Some(name) = queue.pop_front() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        let Some(stage) = graph.stage(&name) else {
            continue;
        };
        // A fan_out stage reaches its worker and merge stages through its own
        // settings rather than an edge, so following only edges would report a
        // perfectly wired worker as an orphan.
        let fan_out = match &stage.mode {
            StageMode::FanOut(config) => [
                match &config.worker {
                    WorkerSource::Stage(worker) => Some(worker.as_str()),
                    _ => None,
                },
                config.merge_stage.as_ref().map(|s| s.as_str()),
            ],
            _ => [None, None],
        };
        let targets = graph
            .edges_from(stage.name.as_str())
            .map(|e| e.to.as_str())
            .chain(fan_out.into_iter().flatten());
        for target in targets {
            if !reachable.contains(target) && graph.stage(target).is_some() {
                queue.push_back(target.to_string());
            }
        }
    }

    let mut findings: Vec<LintFinding> = graph
        .stages
        .iter()
        .filter(|s| !reachable.contains(s.name.as_str()))
        .map(|s| {
            LintFinding::new(
                LintSeverity::Warning,
                "unreachable-stage",
                format!("cannot be reached from entry stage '{entry}'"),
            )
            .in_stage(s.name.as_str())
            .with_fix("give some stage an edge to it, or delete it")
        })
        .collect();

    // A pair of stages that each have an edge to the other, where the one
    // being returned to has no revisit cap, can bounce forever. Each pair is
    // judged once per direction, however many edges join them.
    let mut judged: HashSet<(&str, &str)> = HashSet::new();
    for edge in &graph.edges {
        let (from, to) = (edge.from.as_str(), edge.to.as_str());
        if from == to || !judged.insert((from, to)) {
            continue;
        }
        let Some(target) = graph.stage(to) else {
            continue;
        };
        if graph.edges_from(to).any(|e| e.to.as_str() == from) && target.max_revisits.is_none() {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "cycle-without-max-revisits",
                    format!("is in a cycle with '{from}' and has no max_revisits"),
                )
                .in_stage(to)
                .with_fix("set max_revisits so the loop has to end"),
            );
        }
    }

    findings
}

/// A few names out of a catalogue, for a message that has to fit on a line.
///
/// The count is worth carrying even when the list is short: "it lists 2" and
/// "it lists 340" send someone to different places, the first to a typo in the
/// script's own `list_models` and the second to a typo in the blueprint.
fn sample_catalog(ids: &[String]) -> String {
    const SHOWN: usize = 3;
    let head: Vec<&str> = ids.iter().take(SHOWN).map(String::as_str).collect();
    match ids.len() > SHOWN {
        true => format!("{} and {} more", head.join(", "), ids.len() - SHOWN),
        false => head.join(", "),
    }
}

/// Under `[providers] zero_retention`, the models this stage names that keep
/// something: an error for the one the stage would start on, since the spawn
/// gate refuses it, and a warning for a fallback, since failover drops it.
pub(super) fn lint_retention(stage: &StageDef, env: &LintEnv) -> Vec<LintFinding> {
    let Some(refusals) = env.retention_refusals.get(stage.name.as_str()) else {
        return Vec::new();
    };
    refusals
        .iter()
        .map(|r| match r.head {
            true => LintFinding::new(
                LintSeverity::Error,
                "retention-not-zero",
                format!(
                    "would run on {}, which does not run with zero data retention ({}); \
                     `[providers] zero_retention` is on, so the spawn is refused",
                    r.route, r.reason
                ),
            )
            .in_stage(stage.name.as_str())
            .with_fix(
                "name a model that keeps nothing (`lev providers retention` says which), \
                 declare the agreement in `[providers] zero_retention_agreements` if you \
                 hold one, or `lev providers retention set off`",
            ),
            false => LintFinding::new(
                LintSeverity::Warning,
                "retention-fallback-dropped",
                format!(
                    "lists {} as a fallback, which does not run with zero data retention \
                     ({}); `[providers] zero_retention` is on, so failover skips it",
                    r.route, r.reason
                ),
            )
            .in_stage(stage.name.as_str())
            .with_fix("list a fallback that keeps nothing, or drop this one"),
        })
        .collect()
}

/// Models and providers the install cannot resolve.
pub(super) fn lint_models(stage: &StageDef, env: &LintEnv) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    for (provider, model) in stage.model.models.iter().map(route) {
        // What the provider itself said, when this install asked it. It beats
        // every check below: a live catalogue knows about models released after
        // this build, and about a script provider's models that no build could
        // know.
        match env.provider_catalogs.get(provider) {
            Some(ProviderCatalog::Complete(ids)) => {
                let key = model_key(model);
                if !ids.iter().any(|id| model_key(id) == key) {
                    // The provider's own reason wins. "Does not serve it" is
                    // right for a typo and wrong for a model the route carries
                    // and this account cannot reach, and the two send a reader
                    // to different places.
                    let reason = env.provider_refusals.get(&format!("{provider}/{model}"));
                    findings.push(
                        LintFinding::new(
                            LintSeverity::Error,
                            "unserved-model",
                            match reason {
                                Some(reason) => format!("names {provider}/{model}: {reason}"),
                                None => format!(
                                    "names {provider}/{model}, which provider '{provider}' \
                                     does not serve (it lists {})",
                                    sample_catalog(ids),
                                ),
                            },
                        )
                        .in_stage(stage.name.as_str())
                        .with_fix(format!(
                            "run `lev models list --provider {provider}` and name one of those"
                        )),
                    );
                }
                // Checked against the provider's own answer, so the compiled
                // table below has nothing left to add and would only put a
                // second, weaker finding on the same entry.
                continue;
            }
            Some(ProviderCatalog::ScriptSaidNothing) => {
                findings.push(
                    LintFinding::new(
                        LintSeverity::Warning,
                        "catalog-unchecked",
                        format!(
                            "names {provider}/{model}, and provider '{provider}' does not say \
                             which models it serves, so the name went unchecked"
                        ),
                    )
                    .in_stage(stage.name.as_str())
                    .with_fix(format!(
                        "give {provider}.rhai a `list_models(state)`, or list its models under \
                         `[model_providers.{provider}] serves`"
                    )),
                );
                continue;
            }
            None => {}
        }

        // A provider with no catalog here is open-ended (Ollama serves whatever
        // is pulled, OpenRouter's list runs to hundreds, a script provider
        // defines its own). Checking a model against a catalog that does not
        // claim to be complete would only produce false alarms.
        let catalog_known = env.known_models.iter().any(|(p, _)| p == provider);
        let listed = env
            .known_models
            .iter()
            .any(|(p, m)| p == provider && m == model);
        if catalog_known && !listed {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "unknown-model",
                    format!(
                        "names {provider}/{model}, which is not a model this build knows about"
                    ),
                )
                .in_stage(stage.name.as_str())
                .with_fix(
                    "check `lev models list`, or `lev models list --remote` \
                           if it is newer than this build",
                ),
            );
        }
    }

    // Reported per stage, not per entry: the models list is an ordered set of
    // fallbacks, so naming a provider this install cannot reach is normal and
    // expected as long as something later in the list answers. What is worth
    // saying is that *nothing* in the list does, which is the shape that
    // reaches the runtime as "no usable provider" at spawn.
    //
    // An entry is reachable when this install can actually run it: a pinned one
    // needs its provider registered, an open one needs something that serves
    // its model, which arrives here as `unrouted_models`. That is empty both
    // when nobody asked and when everything routes, and both read as reachable
    // here: a question nobody asked must not turn into a finding.
    let reachable = |(provider, model): (&str, &str)| match provider.is_empty() {
        true => !env.unrouted_models.contains(model),
        false => env
            .available_providers
            .as_ref()
            .is_some_and(|a| a.contains(provider)),
    };
    if env.available_providers.is_some()
        && !stage.model.models.is_empty()
        && !stage.model.models.iter().map(route).any(reachable)
    {
        // Written the way the blueprint writes them, so the list in the message
        // can be found in the file: a bare name for an entry that left the route
        // open, `provider/model` for one that pinned it.
        let tried: Vec<String> = stage.model.models.iter().map(ToString::to_string).collect();
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "no-reachable-provider",
                format!(
                    "names nothing this install can run (tried {}), so it \
                     falls back to your fallback_model, if one is set",
                    tried.join(", ")
                ),
            )
            .in_stage(stage.name.as_str())
            .with_fix("run `lev setup` to configure one of them, or name a model you have"),
        );
    }

    findings
}

/// Whether a region is one an edge's whole-context summary reaches: anything
/// but the regions kept for the whole run.
fn is_stage_specific(kind: &RegionKind) -> bool {
    !matches!(
        kind,
        RegionKind::Pinned
            | RegionKind::CompactHistory { .. }
            | RegionKind::Keyed { .. }
            | RegionKind::Custom { pinned: true, .. }
    )
}

/// A `compact` edge that would summarize a region holding a deliverable.
///
/// `carry = { compact = {} }` reads as "summarize the transcript on the way
/// out" and means "summarize every region that is not kept for the whole
/// run", which includes the ones holding the run's results. Figures that
/// survive a paraphrase are no longer figures, and nothing about the
/// blueprint is malformed, so the only place to say so is here.
///
/// Scoped to regions declared `required` rather than every region a compact
/// touches. `required` is the author saying "a stage must populate this",
/// which is the closest thing a blueprint has to "this is a deliverable" -
/// warning on all of them would teach people to ignore it.
pub(super) fn lint_compacted_deliverables(graph: &RunGraph) -> Vec<LintFinding> {
    // Named once per region, however many edges would summarize it: the fix is
    // on the region, so repeating it per edge is noise.
    let mut at_risk: Vec<&str> = Vec::new();
    for stage in &graph.stages {
        let compacts = graph
            .edges_from(stage.name.as_str())
            .any(|e| matches!(e.carry, EdgeCarry::Compact { .. }));
        if !compacts {
            continue;
        }
        for region in &graph.layout_for(stage).regions {
            if region.required
                && region.summarizable
                && is_stage_specific(&region.kind)
                && !at_risk.contains(&region.name.as_str())
            {
                at_risk.push(region.name.as_str());
            }
        }
    }

    at_risk
        .into_iter()
        .map(|region| {
            LintFinding::new(
                LintSeverity::Warning,
                "compact-summarizes-deliverable",
                format!(
                    "region '{region}' is declared required - a stage must populate it - \
                     and an edge carrying `compact` would hand it to the summarizer on the \
                     way out, so whatever the stage wrote reaches later stages paraphrased"
                ),
            )
            .with_fix(format!(
                "set summarizable = false on region '{region}' if its content does not \
                 survive a rewrite, or name the regions to summarize with \
                 carry = {{ custom = {{ ... }} }}"
            ))
        })
        .collect()
}

/// A `required` region no stage is able to populate, so nothing enforces it.
///
/// `required` reads as a guarantee: a stage may not complete while the region is
/// empty. The runtime gate that provides it opens with an escape - a stage
/// granting neither `context_write` nor `context_append` is skipped entirely,
/// because gating a stage that could never populate the region would loop
/// until the re-entry cap and then proceed anyway.
///
/// That escape is right per stage and wrong per blueprint. If *no* stage using
/// the layout grants a context-writing tool, the flag is inert everywhere: it
/// looks like the deliverable is protected, and it is not. The failure it hides
/// is quiet - `sources_index` stayed empty through all seven stages of a
/// research run and the report stage invented a bibliography rather than
/// reporting it had none.
///
/// Regions an input fills are exempt for the same reason the runtime exempts
/// them: the caller owns those, and they are checked at spawn.
pub(super) fn lint_required_regions_enforceable(graph: &RunGraph) -> Vec<LintFinding> {
    let writes_context = |stage: &StageDef| {
        grants_all_builtins(stage)
            || named_tools(stage).any(|t| t == "context_write" || t == "context_append")
    };

    let mut findings = Vec::new();
    let mut named: Vec<&str> = Vec::new();
    for stage in &graph.stages {
        for region in &graph.layout_for(stage).regions {
            if !region.required
                || bound_by_input(graph, region)
                || named.contains(&region.name.as_str())
            {
                continue;
            }
            // Any stage sharing this region's layout and able to write context
            // is enough: that stage is where the gate binds.
            let enforceable = graph.stages.iter().any(|s| {
                writes_context(s)
                    && graph
                        .layout_for(s)
                        .regions
                        .iter()
                        .any(|r| r.name == region.name)
            });
            if enforceable {
                continue;
            }
            named.push(region.name.as_str());
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "required-region-unenforceable",
                    format!(
                        "region '{}' is declared required, but no stage that uses it grants \
                         context_write or context_append - so nothing can populate it and the \
                         gate that would hold a stage for it is skipped. The flag has no effect",
                        region.name
                    ),
                )
                .with_fix(format!(
                    "add context_write or context_append to the tools of the stage that \
                     owes '{}', or drop required = true",
                    region.name
                )),
            );
        }
    }
    findings
}

/// A region that evicts, bounded by a share of a window nobody has measured.
///
/// Percentage budgets exist so an author's intent survives a change of model:
/// "35%" means the same thing whatever the window. For a *fixed* region that
/// holds. For one whose whole design is to evict, it does not - because eviction
/// only ever runs at the bound, so the bound is the discipline, and a percentage
/// re-reads that discipline every time the model changes.
///
/// The bundled researcher declares a temporary `raw_findings` region at
/// `budget = "38%"`. Written against ~200k windows that means "hold the last
/// ~76k of raw source material" - sane. Resolved against a 1M window the same
/// line means a 380k ceiling: oldest-first eviction exists, and never
/// triggers, because the bound is never reached. A measured run grew
/// monotonically from 3k to 196k tokens per request over 31 requests and
/// burned 3.3M cache-write tokens without finishing. A 24000-token cap fixed
/// it completely.
///
/// Nothing errored, which is the point. The failure is invisible until the bill
/// arrives, so it is worth saying out loud at the only moment somebody is
/// looking at the blueprint.
///
/// Warned once per region name however many layouts declare it: the fix is on
/// the declaration.
pub(super) fn lint_unbounded_percentage(graph: &RunGraph, env: &LintEnv) -> Vec<LintFinding> {
    let Some((model, window)) = widest_declared_window(graph, env) else {
        // No window in hand means no number to put in the sentence, and the
        // sentence is the whole value: "38% might be large" is not actionable.
        return Vec::new();
    };

    let mut named: Vec<(&str, usize, f64)> = Vec::new();
    for layout in layouts(graph) {
        for region in &layout.regions {
            let Budget::Percent {
                percent, max: None, ..
            } = region.budget
            else {
                continue;
            };
            if !evicts_at_its_bound(&region.kind) {
                continue;
            }
            if named
                .iter()
                .any(|(name, _, _)| *name == region.name.as_str())
            {
                continue;
            }
            named.push((
                region.name.as_str(),
                resolve_budget(&region.budget, window),
                percent,
            ));
        }
    }

    named
        .into_iter()
        .map(|(region, ceiling, percent)| {
            LintFinding::new(
                LintSeverity::Warning,
                "unbounded-percentage-budget",
                format!(
                    "region '{region}' evicts at its bound, and its budget \
                     \"{pct:.0}%\" resolves to {ceiling} tokens on {model} \
                     ({window} window) - a bound that large may never be reached, \
                     so the region hoards instead of evicting",
                    pct = percent * 100.0,
                ),
            )
            .with_fix(format!(
                "give region '{region}' a cap, budget = {{ percent = \"{pct:.0}%\", max = ... }} - \
                 the percentage still applies on smaller windows, and the cap keeps \
                 eviction running on larger ones",
                pct = percent * 100.0,
            ))
        })
        .collect()
}

/// The largest context window among the models this graph names, and which
/// model that is.
///
/// The largest rather than the average: it is the one that turns a modest
/// percentage into a hoard, and the blueprint will meet it as soon as anybody
/// runs a stage on it.
fn widest_declared_window<'a>(graph: &RunGraph, env: &'a LintEnv) -> Option<(&'a str, usize)> {
    model_entries(graph)
        .map(route)
        // A model that does not write text has no context window in the
        // sense this check means: a 3D generator's "window" is the ceiling
        // its REST call takes a mesh under, and one Meshy stage made every
        // percentage region in the blueprint resolve against it.
        .filter(|(provider, model)| {
            leviath_providers::mime_tables::builtin_mime(provider, model)
                .produces(&leviath_core::mime::text_plain())
        })
        .filter_map(|(provider, model)| {
            env.model_windows
                .get_key_value(&(provider.to_string(), model.to_string()))
                .map(|((_, model), window)| (model.as_str(), *window))
        })
        .max_by_key(|(_, window)| *window)
}

/// Whether this kind of region drops content when it reaches its ceiling.
///
/// The kinds that do are the ones the warning is about: a bound they cannot
/// reach is a mechanism that never runs. Everything else either holds what it is
/// given (`Pinned`, `Checklist`) or is bounded by something other than a token
/// count, and a percentage there is exactly as intended.
fn evicts_at_its_bound(kind: &RegionKind) -> bool {
    matches!(
        kind,
        RegionKind::Temporary
            | RegionKind::Clearable
            | RegionKind::SlidingWindow { .. }
            | RegionKind::Compacting { .. }
    )
}
