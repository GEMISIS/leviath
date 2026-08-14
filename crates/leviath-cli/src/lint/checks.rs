//! The checks that read a manifest as a shape: does every stage it names exist,
//! can the run reach an output, does a tool it advertises actually resolve.
//!
//! Split from the security checks next door because these answer "will this
//! agent work" and those answer "should this agent be allowed to".

use super::*;

/// Fields the stage left to a default: `mode`, `model`, and `max_iterations`.
pub(super) fn lint_declarations(stage: &leviath_core::Stage, keys: StageKeys) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    if !keys.mode {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "stage-missing-mode",
                "no mode is set, so the stage runs as autonomous".to_string(),
            )
            .in_stage(&stage.name)
            .with_fix("write mode = \"autonomous\" if that is what you meant"),
        );
    }

    if !keys.model {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "stage-missing-model",
                format!(
                    "no [stages.{}.model] block, so the stage runs on your \
                     configured default_provider, whatever that is",
                    stage.name
                ),
            )
            .in_stage(&stage.name)
            .with_fix(format!(
                "add model = {{ models = [{{ provider = \"...\", model = \"...\" }}] }} \
                 to [stages.{}]",
                stage.name
            )),
        );
    }

    // A fan_out stage does not run inference itself - it splits work and waits
    // on its workers - so it has no iteration count to cap.
    let counts_iterations = !matches!(stage.mode, StageMode::FanOut { .. });
    if counts_iterations && stage.max_iterations.is_none() {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "stage-missing-max-iterations",
                "no max_iterations, so the stage is unbounded unless your config \
                 sets [limits] default_max_iterations"
                    .to_string(),
            )
            .in_stage(&stage.name)
            .with_fix("give the stage a max_iterations it should never reach"),
        );
    }

    findings
}

/// Tool names that resolve to nothing, and permissions for tools the stage
/// never granted.
pub(super) fn lint_tools(stage: &leviath_core::Stage, env: &LintEnv) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    if !env.known_tools.is_empty() {
        for tool in &stage.available_tools {
            // `server__tool` is an MCP name. It resolves only once that server
            // is installed and connected, which is not a property of the
            // manifest, so it is never this check's business.
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
                .in_stage(&stage.name)
                .with_fix("check the spelling, or drop the entry"),
            );
        }
    }

    let granted: HashSet<&str> = stage.available_tools.iter().map(String::as_str).collect();
    for tool in stage.tool_permissions.keys() {
        if granted.contains(tool.as_str()) {
            continue;
        }
        findings.push(
            LintFinding::new(
                LintSeverity::Error,
                "orphan-stage-permission",
                format!(
                    "sets a permission for '{tool}', which it does not grant in \
                     available_tools - it reads as a grant and is not one"
                ),
            )
            .in_stage(&stage.name)
            .with_fix(format!(
                "add '{tool}' to available_tools, or drop the permission"
            )),
        );
    }

    findings
}

/// Human-in-the-loop tools offered by a stage that runs with nobody attached.
pub(super) fn lint_blocking_tools(stage: &leviath_core::Stage) -> Vec<LintFinding> {
    // Only autonomous stages are a problem: the interactive modes are where a
    // person is expected, and a fan_out stage runs no tools of its own.
    if !matches!(stage.mode, StageMode::Autonomous) || stage.allow_blocking_tools {
        return Vec::new();
    }
    stage
        .available_tools
        .iter()
        .filter(|t| BLOCKING_INTERACTION_TOOLS.contains(&canonical_tool_name(t)))
        // A tool kept in `required_tools` is the same statement of intent
        // `allow_blocking_tools` makes, made one tool at a time - and it is the
        // one that also survives an unattended run, so it is worth more.
        //
        // Canonicalised on both sides, as the runtime does: a stage granting
        // `bash` and keeping `shell` is one decision, not two.
        .filter(|t| {
            !stage
                .required_tools
                .iter()
                .any(|r| canonical_tool_name(r) == canonical_tool_name(t))
        })
        .map(|tool| {
            LintFinding::new(
                LintSeverity::Warning,
                "blocking-tool-in-autonomous-stage",
                format!(
                    "is autonomous but grants '{tool}', which suspends the run \
                     until a person answers"
                ),
            )
            .in_stage(&stage.name)
            .with_fix(
                "drop the tool, switch the stage to an interactive mode, list it in \
                 required_tools so it survives an unattended run too, or set \
                 allow_blocking_tools = true to say you meant it",
            )
        })
        .collect()
}

/// A stage's own output declarations: a demand it cannot meet, a shape nothing
/// will read, or a reporting stage that can also change the workspace.
pub(super) fn lint_output_stage(stage: &leviath_core::Stage) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let grants_submit = stage
        .available_tools
        .iter()
        .any(|t| canonical_tool_name(t) == leviath_core::blueprint::SUBMIT_OUTPUT_TOOL);

    // `Stage::validate` already refuses this outright, so reaching it here means
    // the manifest never loaded. Reported anyway because `lev validate` runs the
    // linter over a blueprint it *did* load, and a future path that relaxes the
    // hard error should still surface it.
    if stage.require_output && !grants_submit {
        findings.push(
            LintFinding::new(
                LintSeverity::Error,
                "output-missing-submit-tool",
                format!(
                    "must produce a final output but does not grant '{}'",
                    leviath_core::blueprint::SUBMIT_OUTPUT_TOOL
                ),
            )
            .in_stage(&stage.name)
            .with_fix(format!(
                "add '{}' to available_tools, or use mode = \"output\", which grants it",
                leviath_core::blueprint::SUBMIT_OUTPUT_TOOL
            )),
        );
    }

    // A declared shape nobody is obliged to produce is a wish, not a contract:
    // the tool description carries it, and the agent may still finish without
    // calling the tool at all.
    if stage.output.is_some() && !stage.require_output {
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "output-shape-not-required",
                "declares an output shape but is not required to produce one, so the run may \
                 finish with nothing"
                    .to_string(),
            )
            .in_stage(&stage.name)
            .with_fix("set require_output = true, or move the shape to the stage that submits"),
        );
    }

    // An output stage summarizes work; one that can also change files invites
    // the model to keep working where it was meant to report.
    if stage.mode == StageMode::Output {
        let modifying: Vec<&String> = stage
            .available_tools
            .iter()
            .filter(|t| leviath_core::blueprint::MODIFYING_TOOLS.contains(&canonical_tool_name(t)))
            .collect();
        for tool in modifying {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "output-stage-can-modify",
                    format!("is an output stage but grants '{tool}', which changes the workspace"),
                )
                .in_stage(&stage.name)
                .with_fix(
                    "drop the tool: an output stage reports what happened, and work done here \
                     lands after the review that was meant to check it",
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
/// Stages whose every normal exit can run out of `max_revisits` budget.
///
/// A stage transitions along its `Always`/`LlmChoice` edges; an edge whose
/// target has `max_revisits` stops being followable once the budget is spent.
/// When EVERY normal edge is like that, a long enough run strands the stage
/// with nowhere to go - which the engine now reports as a dead-end *error*
/// (it used to read as `complete`, from the middle of the graph, with the
/// output stage still pending). Observed live: a wide-researcher bounced
/// deep_dive → compare until compare's budget ran out, then "completed" with
/// nothing produced.
///
/// The fix is one un-exhaustible way forward: an edge to a stage without
/// `max_revisits` (an output/terminal stage usually), or a
/// `condition = "max_iterations"` escape.
pub(super) fn lint_dead_end_possible(blueprint: &Blueprint) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    for stage in &blueprint.stages {
        let Some(transitions) = &stage.transitions else {
            continue;
        };
        let normal: Vec<&leviath_core::blueprint::TransitionEdge> = transitions
            .values()
            .filter(|e| {
                matches!(
                    e.condition,
                    leviath_core::blueprint::TransitionCondition::Always
                        | leviath_core::blueprint::TransitionCondition::LlmChoice
                )
            })
            .collect();
        if normal.is_empty() {
            continue; // terminal (or conditioned-only) stage: nothing to strand
        }
        let all_exhaustible = normal.iter().all(|e| {
            blueprint
                .find_stage(&e.target)
                .is_none_or(|t| t.max_revisits.is_some())
        });
        // An escape the runtime actually consults on this path. `resolve_transition`
        // resolves a dead end down a `dead_end` edge, then an `error` edge, so
        // either satisfies the check - provided its own target can still be
        // entered, or it is no escape at all.
        let has_escape = transitions.values().any(|e| {
            matches!(
                e.condition,
                leviath_core::blueprint::TransitionCondition::DeadEnd
                    | leviath_core::blueprint::TransitionCondition::Error
            ) && blueprint
                .find_stage(&e.target)
                .is_some_and(|t| t.max_revisits.is_none())
        });

        if all_exhaustible && !has_escape {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "dead-end-possible",
                    "can strand the run: every normal transition's target has a max_revisits \
                     budget, and once they are all spent the run errors as dead-ended"
                        .to_string(),
                )
                .in_stage(&stage.name)
                .with_fix(
                    "add a condition = \"dead_end\" edge to a stage without max_revisits \
                     (the output stage, usually). It is taken only when the graph would \
                     otherwise strand, so it is not a route the model can choose early - \
                     unlike a plain edge to the same stage, which is offered on every visit",
                ),
            );
        }
    }
    findings
}

pub(super) fn lint_output_reachable(blueprint: &Blueprint) -> Vec<LintFinding> {
    let outputs: Vec<&leviath_core::Stage> = blueprint
        .stages
        .iter()
        .filter(|s| s.mode == StageMode::Output)
        .collect();
    if outputs.is_empty() {
        return Vec::new();
    }
    let mut findings = Vec::new();

    for output in &outputs {
        let reached = blueprint.stages.iter().any(|s| {
            s.name != output.name
                && s.transitions
                    .iter()
                    .flat_map(|edges| edges.values())
                    .any(|e| e.target == output.name)
        });
        let is_entry = blueprint.entry_stage.as_deref() == Some(output.name.as_str())
            || blueprint.stages.first().map(|s| s.name.as_str()) == Some(output.name.as_str());
        if !reached && !is_entry {
            findings.push(
                LintFinding::new(
                    LintSeverity::Error,
                    "output-unreachable",
                    "is an output stage no edge routes to, so the run can never produce one"
                        .to_string(),
                )
                .in_stage(&output.name)
                .with_fix(format!(
                    "add a transition to '{}' from whichever stage finishes the work",
                    output.name
                )),
            );
        }
    }

    for stage in &blueprint.stages {
        if stage.allow_complete && stage.mode != StageMode::Output {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "allow-complete-skips-output",
                    "may end the run itself, so the model can finish here and never reach the \
                     output stage"
                        .to_string(),
                )
                .in_stage(&stage.name)
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
/// cap. Both only mean anything for a blueprint that declares transitions at
/// all - a linear one has no graph to walk.
pub(super) fn lint_graph(blueprint: &Blueprint) -> Vec<LintFinding> {
    if !blueprint.stages.iter().any(|s| s.transitions.is_some()) {
        return Vec::new();
    }
    let stage_names: HashSet<&str> = blueprint.stages.iter().map(|s| s.name.as_str()).collect();
    let entry = blueprint.resolve_entry_stage_name();

    // Breadth-first from the entry stage; whatever is left over is orphaned.
    let mut reachable = HashSet::new();
    let mut queue = std::collections::VecDeque::from([entry.clone()]);
    while let Some(name) = queue.pop_front() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        let Some(stage) = blueprint.find_stage(&name) else {
            continue;
        };
        // A fan_out stage reaches its worker and merge stages through its own
        // config rather than a transition edge, so following only `transitions`
        // would report a perfectly wired worker as an orphan.
        let fan_out = match &stage.mode {
            StageMode::FanOut { config } => [
                config.worker_stage.as_deref(),
                config.merge_stage.as_deref(),
            ],
            _ => [None, None],
        };
        let edges = stage
            .transitions
            .iter()
            .flat_map(|t| t.keys().map(String::as_str))
            .chain(fan_out.into_iter().flatten());
        for target in edges {
            if !reachable.contains(target) && stage_names.contains(target) {
                queue.push_back(target.to_string());
            }
        }
    }

    let mut findings: Vec<LintFinding> = blueprint
        .stages
        .iter()
        .filter(|s| !reachable.contains(s.name.as_str()))
        .map(|s| {
            LintFinding::new(
                LintSeverity::Warning,
                "unreachable-stage",
                format!("cannot be reached from entry stage '{entry}'"),
            )
            .in_stage(&s.name)
            .with_fix("give some stage a transition to it, or delete it")
        })
        .collect();

    // A pair of stages that each transition to the other, where the one being
    // returned to has no revisit cap, can bounce forever.
    for stage in &blueprint.stages {
        let Some(transitions) = &stage.transitions else {
            continue;
        };
        for target in transitions.keys().filter(|t| **t != stage.name) {
            let Some(target_stage) = blueprint.find_stage(target) else {
                continue;
            };
            let Some(t2) = &target_stage.transitions else {
                continue;
            };
            if t2.contains_key(&stage.name) && target_stage.max_revisits.is_none() {
                findings.push(
                    LintFinding::new(
                        LintSeverity::Warning,
                        "cycle-without-max-revisits",
                        format!(
                            "is in a cycle with '{}' and has no max_revisits",
                            stage.name
                        ),
                    )
                    .in_stage(target)
                    .with_fix("set max_revisits so the loop has to end"),
                );
            }
        }
    }

    findings
}

/// Models and providers the install cannot resolve.
pub(super) fn lint_models(stage: &leviath_core::Stage, env: &LintEnv) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    for entry in &stage.model.models {
        // A provider with no catalog here is open-ended (Ollama serves whatever
        // is pulled, OpenRouter's list runs to hundreds, a script provider
        // defines its own). Checking a model against a catalog that does not
        // claim to be complete would only produce false alarms.
        let catalog_known = env.known_models.iter().any(|(p, _)| *p == entry.provider);
        let listed = env
            .known_models
            .iter()
            .any(|(p, m)| *p == entry.provider && *m == entry.model);
        if catalog_known && !listed {
            findings.push(
                LintFinding::new(
                    LintSeverity::Warning,
                    "unknown-model",
                    format!(
                        "names {}/{}, which is not a model this build knows about",
                        entry.provider, entry.model
                    ),
                )
                .in_stage(&stage.name)
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
    if let Some(available) = &env.available_providers
        && !stage.model.models.is_empty()
        && !stage
            .model
            .models
            .iter()
            .any(|e| available.contains(&e.provider))
    {
        let tried: Vec<&str> = stage
            .model
            .models
            .iter()
            .map(|e| e.provider.as_str())
            .collect();
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "no-reachable-provider",
                format!(
                    "names no provider this install can reach (tried {}), so it \
                     falls back to your default model",
                    tried.join(", ")
                ),
            )
            .in_stage(&stage.name)
            .with_fix("run `lev setup` to configure one of them, or add a provider you have"),
        );
    }

    findings
}

/// A bare `compact` edge that would summarize a region holding a deliverable.
///
/// `transform = "compact"` reads as "summarize the transcript on the way out"
/// and means "summarize every region that is not pinned", which includes the
/// ones holding the run's results. Figures that survive a paraphrase are no
/// longer figures, and nothing about the blueprint is malformed, so the only
/// place to say so is here (#369).
///
/// Scoped to regions declared `required` rather than every region a bare
/// compact touches. `required` is the author saying "a stage must populate
/// this", which is the closest thing a blueprint has to "this is a
/// deliverable" - warning on all of them would fire on every agent that ever
/// wrote `transform = "compact"` and teach people to ignore it.
pub(super) fn lint_compacted_deliverables(blueprint: &Blueprint) -> Vec<LintFinding> {
    use leviath_core::blueprint::EdgeTransform;

    // Named once per region, however many edges would summarize it: the fix is
    // on the region, so repeating it per edge is noise.
    let mut at_risk: Vec<&str> = Vec::new();
    for stage in &blueprint.stages {
        let layout = stage
            .context_layout
            .as_ref()
            .unwrap_or(&blueprint.context_layout);
        let bare_compact = stage
            .transitions
            .iter()
            .flat_map(|edges| edges.values())
            .any(|e| matches!(e.transform, EdgeTransform::Compact { .. }));
        if !bare_compact {
            continue;
        }
        for region in &layout.regions {
            if region.required
                && region.summarizable
                && leviath_runtime::is_stage_specific(&region.kind)
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
                     and a `transform = \"compact\"` edge would hand it to the summarizer \
                     on the way out, so whatever the stage wrote reaches later stages \
                     paraphrased"
                ),
            )
            .with_fix(format!(
                "add summarizable = false to [context.regions] {region} if its content \
                 does not survive a rewrite, or name the regions to summarize with \
                 transform = \"custom\""
            ))
        })
        .collect()
}

/// A region that evicts, bounded by a share of a window nobody has measured.
///
/// Percentage budgets exist so an author's intent survives a change of model:
/// "35%" means the same thing whatever the window. For a *fixed* region that
/// holds. For one whose whole design is to evict, it does not - because eviction
/// only ever runs at the bound, so the bound is the discipline, and a percentage
/// re-reads that discipline every time the model changes.
///
/// The bundled researcher declares `raw_findings = { kind = "temporary", budget
/// = "38%" }`. Written against ~200k windows that means "hold the last ~76k of
/// raw source material" - sane. Resolved against a 1M window the same line means
/// a 380k ceiling: oldest-first eviction exists, and never triggers, because the
/// bound is never reached. A measured run grew monotonically from 3k to 196k
/// tokens per request over 31 requests and burned 3.3M cache-write tokens
/// without finishing. `max_tokens = 24000` fixed it completely.
///
/// Nothing errored, which is the point. The failure is invisible until the bill
/// arrives, so it is worth saying out loud at the only moment somebody is
/// looking at the blueprint.
///
/// Warned once per region name however many layouts declare it: the fix is on
/// the declaration.
pub(super) fn lint_unbounded_percentage(blueprint: &Blueprint, env: &LintEnv) -> Vec<LintFinding> {
    let Some((model, window)) = widest_declared_window(blueprint, env) else {
        // No window in hand means no number to put in the sentence, and the
        // sentence is the whole value: "38% might be large" is not actionable.
        return Vec::new();
    };

    let mut named: Vec<(&str, usize, f64)> = Vec::new();
    for layout in std::iter::once(&blueprint.context_layout).chain(
        blueprint
            .stages
            .iter()
            .filter_map(|s| s.context_layout.as_ref()),
    ) {
        for region in &layout.regions {
            let leviath_core::layout::BudgetSpec::Percent {
                percent, max: None, ..
            } = region.budget
            else {
                continue;
            };
            if !evicts_at_its_bound(&region.kind) {
                continue;
            }
            if named.iter().any(|(name, _, _)| *name == region.name) {
                continue;
            }
            named.push((&region.name, region.budget.resolve(window), percent));
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
                "add max_tokens to [context.regions] {region} - the percentage \
                 still applies on smaller windows, and the guard keeps eviction \
                 running on larger ones"
            ))
        })
        .collect()
}

/// The largest context window among the models this blueprint names, and which
/// model that is.
///
/// The largest rather than the average: it is the one that turns a modest
/// percentage into a hoard, and the blueprint will meet it as soon as anybody
/// runs a stage on it.
fn widest_declared_window<'a>(blueprint: &Blueprint, env: &'a LintEnv) -> Option<(&'a str, usize)> {
    blueprint
        .stages
        .iter()
        .flat_map(|stage| stage.model.models.iter())
        .filter_map(|m| {
            env.model_windows
                .get_key_value(&(m.provider.clone(), m.model.clone()))
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
fn evicts_at_its_bound(kind: &leviath_core::RegionKind) -> bool {
    matches!(
        kind,
        leviath_core::RegionKind::Temporary
            | leviath_core::RegionKind::Clearable
            | leviath_core::RegionKind::SlidingWindow { .. }
            | leviath_core::RegionKind::Compacting { .. }
    )
}
