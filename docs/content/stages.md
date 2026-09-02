---
title: Multi-stage workflows
description: Split an agent into stages so each phase of a task gets its own model, tools, and context.
group: Concepts
group_order: 2
order: 7
---

# Multi-stage workflows

Asking one model, with one prompt and one set of tools, to plan a change and write it and review it
usually goes badly. The tools that help it write are a distraction while it is planning, and by
review time its context is full of the work it is supposed to be judging.

So a Leviath agent is split into **stages**. Each stage is one job, with its own model, its own
tools, and its own context. When a stage is done, an edge decides which stage runs next.

## Graph

Here is a small workflow with all four kinds of arrow in it:

```mermaid
flowchart LR
  analyze -->|hint: plan ready| implement
  implement -->|hint: ready for review| review
  implement -->|condition: stuck| reassess
  review -->|condition: error| implement
  reassess -->|hint| implement
  review --> done["done"]
```

And the blueprint that produces it. This is the whole shape, so you can see how the pieces fit
before meeting them one at a time:

```toml
[stages.analyze.transitions.implement]
hint = "The plan is ready"

[stages.implement.transitions.review]
hint = "Implementation complete, ready for review"

[stages.implement.transitions.reassess]
condition = "stuck"

[stages.review.transitions.implement]
condition = "error"

[stages.reassess.transitions.implement]
hint = "Replanned, try again"
```

Check any graph before you run it:

```bash
lev validate .             # verifies the graph is well-formed and reachable
```

## Transitions

Every edge is one of two kinds:

- A **hint** edge is chosen by the agent. When it decides the stage's goal is met, it picks the edge
  whose hint best matches what it just did.
- A **conditional** edge fires on its own, on a signal from the runtime rather than the agent's
  choice. The signals are `error`, `stuck`, `max_iterations` (the stage hit its iteration cap),
  `dead_end` (the graph would otherwise strand here), and `always` (an unconditional edge).

```toml
[stages.implement.transitions.review]
hint = "Implementation complete, ready for review"

[stages.implement.transitions.reassess]
condition = "stuck"          # a runtime signal, not the agent's choice
```

An edge with a `hint` and no `condition` is routed by the model, exactly as though you had written
`condition = "llm_choice"`. The full set of values is `always`, `llm_choice`, `error`,
`max_iterations`, `stuck`, and `dead_end`. Anything else is a parse error rather than an edge that
quietly does nothing, so a typo fails at `lev validate` instead of at 2am.

### The escape that is not also a shortcut

`condition = "dead_end"` fires in one situation: the stage finished, and every normal edge's target
has spent its `max_revisits`. Without it the run errors out, and everything it established is
discarded: a profiled dataset, a plan, two rounds of critique.

```toml
[stages.plan.transitions.review]
hint = "Plan is ready for review"

# Taken only when `review` is out of revisits and there is nowhere legal to go.
[stages.plan.transitions.answer]
condition = "dead_end"
```

Why is this its own condition, rather than "add an ordinary edge to the output stage"? An ordinary
edge is offered to the model at the end of **every** visit, so it becomes a shortcut past
the rest of the pipeline. Measured on four agents, that shortcut was taken in 10 of 24 runs of one
and 21 of 36 of another, computing nothing on the way. A `dead_end` edge is invisible to the model's
choice and reachable only when the alternative is dying.

`error` edges are also consulted on this path, so a stage that already has one is covered. When both
are declared, `dead_end` wins: an `error` edge is carrying provider failures too and may want to go
somewhere else.

> [!NOTE]
> `condition = "max_iterations"` does **not** cover this. It fires when a stage burns its iteration
> budget, which is a different event. On the stranding path it is never consulted. `lev validate`
> reflects that: a `max_iterations` edge does not silence `dead-end-possible`.

### Stage keys that shape routing

| Key | Default | Effect |
|---|---|---|
| `max_revisits` | unlimited | How many times this stage may be re-entered, not counting the first visit. See below |
| `transition_prompt` | built-in | Replaces the prompt used to ask the model which edge to take |
| `allow_complete` | `false` | Offers the model an explicit `DONE` answer that ends the run, rather than forcing it down the one available edge |
| `requires_children` | `false` | Holds the stage until every sub-agent it spawned has finished |
| `allow_as_worker` | `false` | Lets this stage be the target of a [fan-out](/docs/sub-agents) |
| `accepts_messages` | `true` | Whether `lev msg` reaches this stage. See [Human-in-the-loop](/docs/interaction) |
| `allow_blocking_tools` | `false` | Marks an autonomous stage as deliberately offering the tools that wait on a person |
| `available_global_tools` | `false` | Also advertises every Rhai tool installed in `~/.leviath/tools/` at spawn. See [which tools a stage gets](/docs/agents#which-tools-a-stage-gets) |

Three of those need a sentence more.

`max_revisits` is also read when the runtime builds the list of edges to offer. An edge pointing at
a stage that is out of budget is dropped from the choices.

`allow_as_worker` is off by default so that you can only fan out into a stage that was designed for
it, rather than into any stage that happens to look suitable.

`allow_blocking_tools` grants nothing and changes no behaviour. An autonomous stage that calls a
human-in-the-loop tool waits until somebody answers, and on an unattended run that is forever, which
is why `lev validate` warns about it. Setting this key tells the linter you meant it, so it stops
reporting a deliberate choice as an oversight. Use it when the stage is driven from the dashboard,
or by somebody watching.

### Every stage should name its own model

`model` is per stage. There is no agent-level `[model]` block. Writing one parses fine and is then
read by nothing, so your stages carry on using their own defaults with no sign that the block was
ignored.

A stage that omits `model` does not fail either. It runs on whichever provider your `[providers]`
config makes the default. That is rarely what the author intended, and you will not find out until
the run picks the wrong model. `lev validate` reports both cases.

### Carrying context across an edge

Each edge decides what the next stage inherits, using `transform`:

```toml
[stages.implement.transitions.review]
hint      = "Ready for review"
transform = "compact"        # direct | clear | compact | summarize | custom
```

- `direct` is the default and carries everything as-is.
- `clear` drops stage-specific regions and keeps pinned ones.
- `compact`, and its alias `summarize`, sends the stage's content through a summarization pass
  before the next stage starts. **It summarizes every region that is not pinned**, not only the
  transcript. That includes the ones holding your results. A region whose content does not survive a
  rewrite should say so:

  ```toml
  [context.regions]
  results = { kind = "sliding_window", budget = "20%", summarizable = false }
  ```

  That protects it wherever it is used, rather than at each of the edges that might touch it, and it
  wins over an explicit `compact` list. `lev validate` warns when a bare `compact` edge would
  summarize a region declared `required`, which is the closest thing a blueprint has to "this is a
  deliverable".
- `custom` takes a `transform_config` that names regions one at a time:

```toml
[stages.implement.transitions.review]
transform = "custom"

[stages.implement.transitions.review.transform_config]
carry          = ["system", "files"]     # pass through untouched
compact        = ["conversation"]        # summarize into the next stage
clear          = ["scratch"]             # drop entirely
compact_prompt = "Summarize what changed and why"
```

A region declared `pinned` is never touched by an edge transform. That is why the error and stuck
reports described below are worth pinning: you want them to survive the edge that carries them.

### Gating an edge on actual work

A stage that was meant to change files can announce it is finished without having changed any. An
edge `gate` refuses the transition until the stage has something to show:

```toml
[stages.implement.transitions.review]
hint = "Implementation complete, ready for review"
gate = { require_modifications = true, max_attempts = 3 }
```

| Field | Default | Meaning |
|---|---|---|
| `require_modifications` | `false` | Require at least one successful file-modifying tool call in the stage being left |
| `require_regions` | `[]` | Regions that must **all** hold content. ANDed with every other condition here |
| `require_region_updated` | unset | Require that a named region **changed** during this stage, rather than only holding content. See below |
| `require_no_open_items` | unset | Name a [checklist region](/docs/context) that must have no open items before this edge is taken |
| `message` | generated | The nudge shown when the gate blocks |
| `region` | unset | An **alternative** way to satisfy `require_modifications`: the gate also passes if this region is non-empty. See below |
| `tools` | `[]` | Extra tool names to count as modifying, beyond `write_file` and `edit_file` |

### `region` is an alternative, `require_regions` is a requirement

These two read alike and do opposite things.

`region` is one of several ways to satisfy `require_modifications`, alongside "a file was modified"
and "a modification was denied by policy". It exists because per-stage counters do not survive a
daemon restart and a region does, so it is the durable stand-in. It is an **or**:

```toml
# Passes as soon as the stage writes any file, even with `plan` still empty.
gate = { require_modifications = true, region = "plan" }
```

`require_regions` is the conjunction. Every region named must hold content, whatever else the gate
is satisfied by:

```toml
# Does not leave until `plan` has been written, full stop.
gate = { require_regions = ["plan"] }

# And this one wants both: files changed AND the plan written.
gate = { require_modifications = true, require_regions = ["plan"] }
```

Like every gate it shares the one `max_attempts` budget, so it re-runs the stage a bounded number of
times and then lets the edge through with a warning rather than stranding the run. When that
happens the run records it: `flags.gates_forced` in `meta.json` counts the transitions that went
through unsatisfied, and `flags.required_regions_abandoned` names any `required = true` region a
stage gave up on. A run that produced its artifact and one that was asked twice and moved on both
finish `complete`, and those two fields are how you tell them apart.

### Requiring a revision, not a repetition

Every other gate asks whether something *exists*, which a stage sent back to redo its work can
satisfy by re-emitting what it already wrote. On a `review -> plan` back-edge that means a
reviewer's rejection can be answered with the same plan, and the loop spins until the stage runs
out of revisits.

```toml
[stages.plan.transitions.compute]
gate = { require_region_updated = "plan",
         message = "The check rejected this plan. Change it before computing again." }
```

The region's content is hashed when the stage is entered and compared when it tries to leave, so
"changed" means changed by *this* pass. It shares the same `max_attempts` budget as every other
gate: a gate that could hold a stage forever would strand the run, so after the budget the edge is
taken with a warning. A gate naming a region no stage declares is refused by `lev validate`. At runtime such a gate
would pass rather than block, since no amount of work could satisfy it. A typo there would read as
a gate that is never reached.
| `max_attempts` | `3` | How many times the stage re-runs before the gate gives up and lets the transition through with a warning |

Per-stage tool counters reset when a stage is entered, and they are not restored when a run resumes
after a daemon restart. Context regions are. So pointing `region` at whatever your write tools are
routed into keeps a resumed run honest.

Set `tools` when an agent's writes go through MCP or [script tools](/docs/rhai-tools) instead of the
built-ins.

### What counts as output

Four things count as an agent having produced something. A successful `write_file`, a successful
`edit_file`, a successful call to a tool you named in a gate's `tools` list, or a submitted
[final output](/docs/outputs).

Nothing else counts, and `shell` in particular does not. An agent can edit a file with `sed -i`, and
Leviath has no way to see that it happened.

Both an edge gate and the run's own `empty_output` verdict use that same rule. They differ only in
scope: the gate asks about one stage, the verdict asks about the whole run.

The verdict is only ever applied to agents that could plausibly write files. If no stage of a
blueprint advertises a file-modifying tool, the run is never marked as having produced nothing. A
router that delegates and a researcher whose answer is its text have no file changes to be missing,
so flagging them would be wrong. Such an agent can also settle the question outright by submitting a
[final output](/docs/outputs). The side effect is that an agent writing through MCP looks the same
way, so name that tool in a gate's `tools` list to have it counted.

`lev ps` marks such a run `complete (no output)`, and the flag travels with it into `meta.json`, the
completion webhook, and the `leviath.runs.total` metric.

## Stuck detection

A `stuck` edge gets a stage out of a loop it is not going to escape on its own. The important part
is that stuckness is **measured, not self-reported**. An agent cannot keep insisting it is nearly
done:

```toml
[stages.implement.transitions.reassess]
condition = "stuck"
stuck_after_iterations      = 20   # inferences in this stage
stuck_after_same_file_edits = 5    # write/edit calls against one path
stuck_after_tool_calls      = 100
stuck_after_minutes         = 30
```

Use any subset you like. The first threshold to trip fires the edge.

> [!TIP]
> When a `stuck` or `error` edge fires, the runtime writes *why* into the target stage's
> [context](/docs/context), so the recovery stage starts out knowing what went wrong instead of
> working it out again. The same happens when a stage hits its iteration cap: whatever runs next is
> told the work was cut off rather than finished.
>
> Stuck reasons go to a `stuck_report` region when the blueprint declares one. Error and
> iteration-cap notes prefer an `error_report` region. Declare both `pinned`, with a small budget
> like 2000 tokens, so the note survives the edge transform that carries it. Without them, the
> notes land in `conversation`.

## Nudging

When a stage's model replies with plain text before making a single tool call, the runtime normally
adds a `[System]` nudge saying "You have tools available" and re-runs the stage, up to three times.

That is the right reflex for a coding stage that has stalled. It is the wrong one for a stage whose
deliverable *is* text. A planner told to use its tools goes looking for a write tool it was never
given.

So each stage can say what should happen instead:

```toml
[agent.nudge]                # agent-wide default for every stage
max = 2

[stages.plan.nudge]
enabled = false              # this stage's deliverable is text, never nudge it

[stages.implement.nudge]
max = 2
text = "You have edit tools. Make the change described in {regions} rather than describing it again."
```

All three keys are optional and cascade independently. A stage block beats `[agent.nudge]`, which
beats the `[nudge]` section of your `config.toml`, which falls back to the built-in defaults.

This is a usability setting, not a permission, so a blueprint may raise `max` above your global
setting as freely as it lowers it.

The `text` can use `{stage}` for the stage's name and `{regions}` for the comma-separated names of
the stage's required context regions. The same substitution works in a required region's
`required_message`, where `{region}` names the region being asked for.

One stage shape is already exempt with nothing configured: a stage with interaction points presents
its text for review, so it is never nudged for producing exactly that text. Setting `enabled`
explicitly at any level overrides this in either direction.
