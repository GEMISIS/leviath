# Coder Agent

A multi-stage coding agent with structured context management. It orients itself in
the codebase before it writes anything, then verifies continuously rather than
declaring success at the end.

## Stages

1. **discover** (Autonomous) — Map the codebase and synthesize a verification workflow
2. **analyze** (Autonomous) — Understand requirements and create an implementation plan
3. **implement** (Autonomous) — Write code, capturing a test baseline first and
   re-verifying after each change
4. **review** (Interactive) — Human review before finalizing changes
5. **error_recovery** (Autonomous) — Reached only via error edges; diagnoses a failure
   and hands back to implement

## Usage

```bash
lev run agents/coder --task "Add authentication to the user API"

# Refuse the spawn-time repo scan (see Command seed below)
lev run agents/coder --task "..." --no-seed-commands
```

## Discovery and workflow synthesis

The `discover` stage answers two questions before any code is written: *what is this
codebase*, and *how do I verify my work in it*. It writes two regions that every later
stage reads:

- **`discovery`** — language, build system, test runner, the command to run a single
  test, directory layout, and local conventions.
- **`workflow`** — the project's tier (1 greenfield / 2 partial / 3 rich test
  infrastructure) plus three literal lines the later stages execute verbatim:

  ```
  BASELINE: <command to run BEFORE any edit>
  VERIFY:   <command to re-run after each change>
  DONE WHEN: <completion bar, including "no regressions vs baseline">
  ```

Both are `required`, so the runtime's own gate re-runs `discover` until they are
actually populated — the synthesized workflow is a commitment the review stage holds
the run to, not a suggestion.

`implement` captures the BASELINE before its first edit and diffs every later run
against it, so a test that used to pass and now fails is caught immediately instead of
at review time.

## Command seed

`repo_files` is seeded by running `git ls-files` once at spawn, so `discover` starts
from facts instead of spending iterations on `ls`. This executes **before the first
inference, and therefore before any tool-approval prompt** — it runs inside the entry
stage's sandbox when one is configured, is time- and size-capped, and outside a git
repo it simply fails and leaves the region empty. Refuse it per-run with
`--no-seed-commands`, or machine-wide with `[security] allow_seed_commands = false`.
`lev validate agents/coder` prints every command seed so it can be audited.

## Context Layout

Budgets are a percentage of the model's context window with an absolute `max_tokens`
guard-rail.

| Region | Kind | Purpose |
| --- | --- | --- |
| `task` | Pinned | The coding task (required) |
| `repo_files` | Pinned | Tracked file list, from the spawn-time `git ls-files` seed |
| `conventions` | Pinned | Style/lint rules, pre-loaded from the repo |
| `architecture` | Pinned | Design docs, pre-loaded from the repo |
| `plan` | Pinned | The implementation plan |
| `discovery` | Pinned | The codebase model (required) |
| `workflow` | Pinned | The synthesized verification workflow (required) |
| `baseline` | Pinned | Pre-change test state |
| `codebase` | Compacting | Files read during the run |
| `implementation` | Compacting | Active coding workspace |
| `codebase_history` / `impl_history` | CompactHistory | Summaries of the above |
| `test_results` | Clearable | Latest test output |
| `errors` | SlidingWindow | Last 5 failures, so a repeating pattern is visible |
| `conversation` | SlidingWindow | Recent turns (bulk eviction for prompt caching) |
| `scratch` | Clearable | Working memory |

## Features

- **Discovery before implementation** so the agent adapts to unfamiliar codebases
  instead of relying on priors
- **Continuous validation** against a captured baseline, so regressions surface
  immediately
- **Pinned regions** for discovery/workflow — no edge transform can clear or compact them
- **Compacting regions** for large codebases (auto-summarizes when the threshold is hit)
- **Interactive review** stage for human approval before finalizing
