# Contributing to Leviath

Contributions are welcome: bug reports, docs fixes, and code. This page covers
both the *process* (how a change lands) and the *tooling* (hooks, lints, and the
coverage gate).

## How a change lands

```mermaid
flowchart LR
  A["Fork &<br/>branch"] --> B["Commit<br/>(Conventional<br/>Commits)"]
  B --> C["Open PR<br/>against main"]
  C --> D["CI: tests, clippy,<br/>fmt, docs,<br/>100% coverage"]
  D --> E["Maintainer<br/>review"]
  E --> F["Merge queue<br/>(rebase)"]
```

Merges to `main` go through GitHub's merge queue: once a PR is approved and green, queueing it
re-runs the required checks on the exact tree that will land, then rebase-merges automatically.

- **Start with an issue** for anything non-trivial. Agreeing on the approach
  first saves you from writing code that can't be merged. Small fixes (typos,
  doc corrections, obvious bugs) can go straight to a PR.
- **Direct pushes to `main` are disabled** for everyone, maintainers included.
  Every change arrives as a pull request, must pass all required CI checks, and
  needs maintainer approval before it can merge.
- **History is rebase-only**, with no merge commits. Keep your branch rebased on
  `main`; force-pushing your own PR branch is fine and expected.
- **Commit messages follow [Conventional Commits](https://www.conventionalcommits.org)**:
  `feat:`, `fix:`, `docs:`, `ci:`, `refactor:`, `test:`, `chore:`. Look at
  `git log --oneline` for the house style.
- **Licensing**: by submitting a contribution you agree it is licensed under the
  repository's [MIT license](LICENSE). There is no CLA.
- **Conduct**: we follow the [Contributor Covenant](CODE_OF_CONDUCT.md).
- **Security issues** go through
  [private vulnerability reporting](https://github.com/GEMISIS/leviath/security/advisories/new),
  never public issues. See [SECURITY.md](SECURITY.md).

## Getting the code building

```bash
git clone https://github.com/GEMISIS/leviath.git
cd leviath
cargo build
cargo test --workspace
cargo clippy --workspace
```

## The pre-commit hook

The hook installs itself the first time you build or test, with no setup step. `cargo test` / `cargo build` pulls in `xtask`'s dev-dependencies, which include [`cargo-husky`](https://github.com/rhysd/cargo-husky). On the first build it installs `.cargo-husky/hooks/pre-commit` into `.git/hooks/pre-commit` automatically.

The hook enforces, before every commit:

- **formatting** (`cargo fmt --check`)
- **clippy** with warnings-as-errors
- **doc lints** (`cargo doc` with `-D warnings`, so no broken or private intra-doc links and no stray HTML)
- the **full test suite**
- the **coverage-suppression-marker lint** (`ast-grep scan`, if `ast-grep` is installed; CI always enforces it)

It does **not** run the full `cargo xtask coverage` check, which is several minutes, too slow for a local commit gate. CI runs it on every push instead, enforcing 100% for real.

If the hook script itself changes (e.g. a commit edits `.cargo-husky/hooks/pre-commit`), `cargo-husky` only reinstalls it on a *fresh* compile of its crate, not on incremental builds. Force it with:

```bash
cargo clean -p cargo-husky && cargo test -p xtask
```

## `ast-grep` (suppression lint)

The suppression-marker scan uses [`ast-grep`](https://ast-grep.github.io), which matches Rust/YAML structurally (via tree-sitter), and the rules live in `.sgrules/`. CI installs it automatically and always enforces the scan; the pre-commit hook runs it only if it's installed locally, otherwise it prints a warning and skips (CI is the backstop). Install it once with any of:

```bash
brew install ast-grep            # macOS / Linuxbrew
cargo install ast-grep --locked  # from source
npm install -g @ast-grep/cli     # via npm
```

## Testing policy

The workspace is gated at a hard **100%** on lines, regions, and functions, with no way to opt out. Coverage-suppression markers (`#[cfg(not(test))]`, `coverage(off)`, tarpaulin/lcov/grcov annotations) are banned by the ast-grep lint above, so code can't be hidden from measurement; it has to be refactored until it's testable. The *only* un-unit-tested code is the thin `lev` binary entrypoint (`crates/leviath-cli/src/main.rs`): the composition root that wires real terminal, stdin, network, and subprocess I/O into the library's tested cores. It's excluded from coverage measurement and guarded by a CI check that requires maintainer sign-off to change.

### Suppressing a lint

You cannot. The pre-commit hook and CI's `check-exclusions` job run `ast-grep
scan`, whose rules fail the build on `#[allow(...)]` or
`#[expect(...)]` for `too_many_arguments`, `type_complexity`, `dead_code`,
`deprecated`, `async_fn_in_trait`, `match_same_arms`, `new_without_default`,
`permissions_set_readonly_false` and `enum_variant_names`.

Every one of those was cleared from this tree by changing the code, and in each
case the change was better than the suppression it replaced:

| Lint | What it turned into |
|---|---|
| `too_many_arguments` | Parameter structs, and `#[derive(SystemParam)]` for the bevy system. One of them exposed the same three fields written out twice, now a single `PromptLane<T>` |
| `permissions_set_readonly_false` | Restoring the file's *original* permissions instead of synthesising "not read-only" — which on Unix hands back `0o666` for a file that was `0o644` |
| `deprecated` | `socket2`, the layer that owns socket options, rather than tokio's wrapper that deprecates the option because it blocks a *runtime* thread |
| `async_fn_in_trait` | An explicit `impl Future` that states whether the future is `Send`. Adding the bound to `RiskyExecutors` did not compile, which is the useful answer: those futures hold non-`Send` state across awaits and the `async fn` left that unsaid |
| `match_same_arms` | One named constant per case, so "these can move independently" is expressed rather than commented |
| `new_without_default` | A constructor named for what it does (`open`, not `new`) |
| `dead_code` | Deleting the field, or wiring it up — `lev test` had two keys that parsed and did nothing |

`clippy::string_slice` is the one exception, and only as `#[expect(..., reason =
"...")]` naming the construct that guarantees the index is a char boundary. The
root `Cargo.toml` explains why that one has a hatch: it exists because slicing a
`&str` aborted the daemon twice.

`#[expect]` rather than `#[allow]` for that exception, and the difference
matters: `expect` fails the build the moment the lint stops firing, so a
suppression cannot outlive the problem it was written for. Converting the tree
from `allow` to `expect` found **six** that were already dead, including a
`dead_code` on a field that was being read.

If a new suppression looks unavoidable, it is worth an hour before it is worth an
attribute. If it genuinely is unavoidable, change `.sgrules/no-lint-suppression.yml`
in the same PR and say why.

### How long a file may be

`cargo xtask structure` holds every source file to **1,200 lines of production code**, and the pre-commit hook and CI both run it. It costs about a tenth of a second: it reads files and counts, nothing more.

It counts **production** lines, stopping at the first column-zero `#[cfg(test)]` and skipping sibling test files entirely. This workspace keeps most tests inline and gates a hard 100%, so about two thirds of the tree is test code; a total-lines rule would fire hardest on the best-tested files, which is exactly backwards.

**The cap is a ratchet and only ever goes down.** 1,200 is where the tree sits today, not where it should end up — the longest file is 1,152 against a median of 218, and the next rungs are 1,000 and 800. Each is earned by splitting the files above it, not by editing the number.

Raising the cap to admit one long file is how a limit stops being one. If a file wants to be longer, split it by concern: `config/`, `blueprint/`, `host/`, `components/`, `lint/` and `daemon/spawn/` are all worked examples, and the glob re-export they use means the split touches no call site. Split on what the code *is about*, not on where the line count lands — `lint/checks.rs` asks "will this agent work" and `lint/security.rs` asks "should this agent be allowed to", which is a real boundary; cutting the same file at "lint_a.rs" and "lint_b.rs" would have satisfied the same number and taught a reader nothing.

### Where a test module lives

Inline `#[cfg(test)] mod tests` in the file under test is the default; 205 files use it. A sibling `foo_tests.rs` (or `foo/tests.rs`) is the sanctioned alternative, used by 24 — but only for one reason.

The difference is what gets measured. An inline test module is part of the file, so the gate counts its *scaffolding* too: a helper written to build fixtures has to itself be exercised on every branch, or it fails the gate. llvm-cov excludes the sibling layout by default, so a test module whose helpers would otherwise need tests of their own belongs there. Reach for it when the scaffolding is the problem, not because a file is getting long.

Shared scaffolding goes further out: `leviath-testkit` is a dev-dependency-only crate for fixtures used by more than one crate (the always-on tracing subscriber, mock HTTP servers), and it is excluded from measurement entirely. Add to it rather than copying a helper into a second crate — that copying is exactly what it was created to stop.

## Running coverage locally

`cargo xtask coverage` gates each workspace package with `cargo llvm-cov --package <pkg> --fail-under-{lines,functions,regions} 100`. llvm-cov does the counting *and* the gating; there's no custom parsing or aggregation. CI enforces a hard **100%** on all three metrics on Linux, macOS, and Windows. Any file below 100% fails the build, and a browsable HTML report lands in the gitignored `coverage/` folder.

Measurement is deliberately **per-package**, not `--workspace`: `-C instrument-coverage` records every function in every binary that links it (including ones that never call it), and the whole-workspace merge can let a never-run record shadow the covered one, so one package at a time is what keeps llvm-cov's counts accurate. See the doc comment atop `xtask/src/coverage.rs`.

```bash
cargo xtask coverage                                   # full workspace
cargo llvm-cov --package <crate> --lib                 # a single crate
cargo llvm-cov --package <crate> --lib --html --open   # browsable per-crate report
```

> Branch coverage isn't collected: `cargo llvm-cov --branch` reliably SIGSEGVs inside LLVM's own coverage-mapping code ([open upstream bug](https://github.com/llvm/llvm-project/issues/119558)). See the doc comment atop `xtask/src/coverage.rs` for the full investigation.

## Running CodeQL locally

GitHub's default CodeQL setup scans every push with the `rust-code-scanning` suite and the `remote` threat model, and the repo keeps that count at zero with nothing dismissed. Waiting twenty minutes per push to learn whether a fix took is the slow way; the same queries run locally in a few minutes:

```bash
brew install codeql                                                    # the CLI (Linux: the GitHub release tarball)
codeql database create target/codeql-db --language=rust --overwrite    # builds the workspace through cargo
codeql database analyze target/codeql-db codeql/rust-queries:codeql-suites/rust-code-scanning.qls \
  --format=sarif-latest --output=target/codeql.sarif --download
python3 perf-tools/codeql_summary.py target/codeql.sarif                # rule, sink, and every flow's source
```

The summary's exit status is the number of findings, so a shell can gate on it. Two things worth knowing before reading a result: the Rust model treats every parameter of an axum handler as request data, `State` included (see `SECURITY.md`, "What the scanners say"), and the sensitive-data queries go by variable *name*, so a loop variable called `secret` that holds env-var names reads as a leak.

## Live-testing against a real daemon

A green test suite and a 100% coverage number are not the same thing as
"tested": this repository has shipped fixes that a unit test certified and a
running daemon ignored. Anything that changes what the daemon does on a tool
call, a spawn or an HTTP request gets driven through a real daemon as well.

`perf-tools/` holds the harness. `perf-tools/harness.sh CMD...` runs `CMD`
in an isolated environment (`LEVIATH_HOME=/tmp/lv`, the repo `.env` skipped,
the native OpenAI provider pointed at `perf-tools/mock.py`) and installs a
one-stage `probe` blueprint. `mock.py` is a stateless OpenAI-compatible server
that asks for a tool call of your choosing on the first turn; `daemon_drive.py`
starts everything, spawns runs over `lev serve`, and waits for them to finish:

```sh
cargo build --release -p leviath-cli
perf-tools/harness.sh python3 perf-tools/daemon_drive.py \
    --runs 2 --tool shell --args '{"command":"echo hi > note.txt"}' --yolo --keep
perf-tools/harness.sh target/release/lev timeline <run-id>
```

Every "the bad thing did not happen" probe needs a control in the same script
that makes the good thing happen. A probe whose control is also silent proves
that the harness is broken, not that the fix works. `mock.py`'s `GET /count`
exists so "no provider call was made" can be asserted rather than assumed.

The same directory holds the measuring sticks a performance change is gated
on (`dash_pty.py`, `serve_latency.py`, `binsize.sh`) and the baseline numbers
under `perf-tools/baselines/`; see `perf-tools/README.md`.

## Dependencies

Declare a dependency in `[workspace.dependencies]` and reference it from a crate as `{ workspace = true }`. A version written inline in a crate manifest is invisible to anyone reading the root, which is how one crate ends up on a different minor from the rest without anybody deciding that.

A version requirement here records **the minimum this workspace has actually been tested against, as of the last time someone deliberately raised it** — not the oldest version that might still compile. Raising a floor to match `Cargo.lock` is a no-op for resolution (the lock already picks that version), so it costs nothing and stops the requirement drifting into a claim nobody has checked. Do it when you notice the gap; there is no obligation to chase every release.

To find genuinely duplicated dependencies, ask cargo:

```bash
cargo tree --duplicates
```

Not `Cargo.lock`. The lockfile records **unenabled optional** dependencies too, so a crate can appear there while never being compiled — `cargo tree -i <crate>` printing "nothing to print" is the tell. Trimming a feature to remove such an entry changes neither the build nor the lockfile.

## Cutting a release

Releases are triggered by a version bump, not by a schedule. Merging a bump to
`main` is what puts a build in front of users, so it is a deliberate, separate
commit rather than something that rides along with a feature.

Everything ships in lockstep — all `leviath-*` crates plus the `lev` binary
carry one version — so there is one command:

```bash
cargo xtask version 0.1.3
```

That writes `[workspace.package] version`, the eleven intra-workspace pins in
`[workspace.dependencies]` just below it, the allocator pin in
`crates/leviath-cli/Cargo.toml` (which lives there rather than in the workspace
table because only the composition-root binary should pick an allocator), and
`Cargo.lock`, then moves `## Unreleased` in `CHANGELOG.md` under a dated
`## 0.1.3` heading and opens a fresh empty one. The pins exist because
`cargo publish` refuses a path dependency with no version requirement, and Cargo
has no way to make those requirements inherit the workspace version — so they
are written out, and they have to agree. `cargo xtask version --check` is the CI
job that fails a hand-edit which moved only some of them.

That same check compares the `[profile.release]` block in
`crates/leviath-cli/Cargo.toml` against the root's. The copy exists because
`cargo install leviath-cli` builds with no workspace — that manifest is the root
there — and without it cargo's defaults apply, silently dropping
`overflow-checks`. Edit one and you have to edit the other.

Then write the changelog entries for what you are shipping, and open the PR.
Because merging a bump publishes, CI requires the **`allow-version-bump`** label
on any PR that moves the version — the same deliberate sign-off
`allow-main-rs-change` asks for, with its own label so approving a release never
doubles as approving an entrypoint change.

If you label the PR after CI has already started, the guard job that ran without
it stays red and keeps the PR blocked, because the merge rollup counts every
suite's copy of a check rather than only the newest. Clear it by rerunning that
job in the suite it failed in:

```bash
gh run rerun <run-id> --failed
```

The guards read the PR's labels live when they run, so the rerun sees the label
you just applied. Adding the label does not start a run of its own.

Merging fires the alpha release for the bump commit; beta promotes it the
following Monday and stable the Thursday after. Channels with nothing new to
publish skip in seconds. See
[Releases and channels](https://leviath.dev/docs/releases) for the full picture.

### Signing the Windows binary

The alpha build signs `lev.exe` with Azure Artifact Signing when six repository
secrets exist, and prints a `::notice` and ships it unsigned when they do not,
so nothing about a fork or a PR build depends on Azure. Setting it up is a
one-time job for a maintainer with an Azure subscription:

1. **Azure portal → Artifact Signing** (Basic tier): create an account, complete
   the identity validation for Sun Forge AI (this is the slow step - days - and
   nothing signs until it is approved), then add a certificate profile of type
   *Public Trust*. Note the account's endpoint (for example
   `https://eus.codesigning.azure.net`).
2. **Microsoft Entra → App registrations**: create an app. Under *Certificates &
   secrets → Federated credentials*, add one for GitHub Actions: organization
   `GEMISIS`, repository `leviath`, entity **Branch**, branch `main`. No client
   secret - the workflow authenticates with OpenID Connect, and the alpha build
   only ever runs from `main`.
3. **The signing account → Access control (IAM)**: assign the app the role
   *Artifact Signing Certificate Profile Signer*.
4. **Repository secrets**: `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`,
   `AZURE_SUBSCRIPTION_ID`, `ARTIFACT_SIGNING_ENDPOINT`,
   `ARTIFACT_SIGNING_ACCOUNT`, `ARTIFACT_SIGNING_PROFILE`.

The next alpha run signs. Verify on the published asset with
`Get-AuthenticodeSignature lev.exe`. Until signing is on, a new build that an
antivirus quarantines should be reported to the vendor as a false positive
(Microsoft: <https://www.microsoft.com/wdsi/filesubmission>, as a software
developer, with the detection name from Protection history); each build is a
new hash, so that has to be repeated per release, which is the whole reason to
sign.

## Design notes

Longer-form rationale that does not belong in the user docs lives in `docs/design/`. Today that is
[the `lev setup` wizard](docs/design/setup-wizard.md), which explains what the old onboarding flow
got wrong and why the replacement is shaped the way it is. Worth reading before changing `lev setup`.

## Regenerating the workflow diagrams

The agent workflow SVGs in `docs/assets/agents/` are rendered from the Mermaid sources in `docs/assets/agents/src/`. See `docs/assets/agents/src/README.md` for the render command and theme configs. If you change an agent's stage graph in `crates/leviath-cli/agents/<name>/agent.leviath`, update its `.mmd` source and re-render both the light and dark variants.
