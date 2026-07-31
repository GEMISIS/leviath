---
title: Agent catalog
group: Get started
group_order: 1
order: 2
---

# Agent catalog

Leviath ships with ten pre-built agents. Each is a multi-stage [blueprint](/docs/agents) you can
run today — give one a task and the [daemon](/docs/daemon) hosts the run:

```bash
lev run coder --task "Build a CLI that converts CSV to JSON"
```

Every one is also a starting point: copy it, or `lev create my-agent` to scaffold your own and
tune the stages, per-stage models, and context regions. The rest of this page is a tour of what
each agent does and when to reach for it.

> [!TIP]
> Not sure which to pick? Match the *shape* of the work: a codebase change (Coding), a question
> to answer from sources (Research), or a recurring chore like logs, briefings, or drafts
> (Utility).

## Coding

Agents that operate on a codebase. All four orient themselves in the project before acting and
carry [stuck detection](/docs/stages) plus error-recovery edges.

| Agent | What it does |
| --- | --- |
| `software-engineer` | Plan-then-implement, with a human-approved plan before any code is written. Mirrors a discover → plan → **approve** → code → review workflow. Use when you want to sign off on the approach first. |
| `coder` | Fully autonomous coding: discover → analyze → (optionally prototype) → implement → review, with continuous verification. Use when you want it to just make the change. |
| `reviewer` | Code review only — a fast scan pass, then a deep review of correctness, security, and architecture, ending in a ranked, actionable report. Use to vet a PR or diff. |
| `parallel-fixer` | Fixes failing tests in parallel — one [sub-agent](/docs/sub-agents) worker per failure, then merge and re-run until the suite is green. Use for a broad "make the tests pass" sweep. |

**`software-engineer`** — stages: `discover`, `plan` (an approval checkpoint), `prototype`,
`implement`, `review`, plus `reassess` and `error_recovery`. The `prototype` stage spikes the
riskiest assumption before executing the approved plan.

**`coder`** — the same shape without the human plan gate: `discover`, `analyze`, elective
`prototype`, `implement`, `review`, `reassess`, `error_recovery`. Reassess is reached only when a
`stuck` edge fires; error_recovery only via error edges.

**`parallel-fixer`** — a fan-out workflow: `discover` (how the project runs its tests) → `validate`
→ `parallel_fix` (fan out `fix_worker` per failure) → `merge_fixes` → `verify`, which decides
between done and another fix round.

## Research

Agents that gather from sources, synthesize, and write it up. Pick by breadth vs. depth.

| Agent | What it does |
| --- | --- |
| `researcher` | General-purpose research: gather, analyze, summarize, with a gather↔analyze refinement loop. Use for a quick, focused answer. |
| `wide-researcher` | Broad landscape survey across many sub-topics — cast a wide net, compare approaches, deep-dive the interesting threads, and produce an overview with recommendations. Use to map a whole space. |
| `deep-researcher` | Thorough single-topic investigation — follows citation chains, cross-checks claims, and produces a structured, cited report. Use when rigor and sources matter. |

**`researcher`** — stages: `gather`, `analyze`, `summarize`, `error_recovery`.

**`wide-researcher`** — stages: `survey`, `compare`, `deep_dive`, `summarize`, `error_recovery`.
The survey casts wide before narrowing.

**`deep-researcher`** — stages: `gather`, `analyze`, `follow_citations`, `synthesize`,
`error_recovery`; analysis loops back to pull and read specific cited sources.

> [!NOTE]
> `wide-researcher` runs its sub-topics as parallel workers — the same [fan-out](/docs/sub-agents)
> mechanism `parallel-fixer` uses for tests.

## Utility

Everyday agents for logs, briefings, and writing.

| Agent | What it does |
| --- | --- |
| `log-analyzer` | Analyzes log files for anomalies, trends, and error patterns via a scripted analyze⇄script loop, keeping a severity-ranked findings index. Use to triage a noisy log. |
| `daily-briefer` | A morning summary agent — gathers from local and web sources, ranks items into a priorities index, and delivers a concise briefing. Use for a recurring standup-style digest. |
| `writing-assistant` | Research-backed writing from topic to polished draft, with an interactive outline checkpoint and a draft⇄edit loop plus a final proofread. Use to produce a sourced piece. |

**`log-analyzer`** — stages: `ingest`, `analyze`, `script` (write and run parsing/aggregation
scripts), `report`, `error_recovery`.

**`daily-briefer`** — stages: `collect`, `prioritize`, `brief`, `error_recovery`.

**`writing-assistant`** — stages: `research`, `outline` (an approval checkpoint), `draft`, `edit`,
`proofread`, `error_recovery`.

## Running one

Any agent runs the same way — name it and hand it a task:

```bash
lev run deep-researcher --task "Survey the state of solid-state batteries"
```

To go further, read how blueprints are built in [Agents](/docs/agents), how the stage graph
routes and recovers in [Multi-stage workflows](/docs/stages), and how the parallel agents split
work in [Sub-agents & fan-out](/docs/sub-agents).
