## What & why

<!-- What does this PR change, and what problem does it solve?
     Link the issue it addresses, if there is one: Fixes #NNN -->

## How it was tested

<!-- Beyond `cargo test`: if the change affects runtime behavior, describe the
     live run you did (`lev run ...`, `lev dash`, daemon restart, ...).
     CI-green alone is not evidence a behavior change works. -->

## Checklist

- [ ] Commits follow [Conventional Commits](https://www.conventionalcommits.org) (`feat:`, `fix:`, `docs:`, ...)
- [ ] `cargo test --workspace` and `cargo clippy --workspace` pass locally (the pre-commit hook enforces this)
- [ ] New code is fully covered — CI gates at 100% lines/regions/functions with no opt-out
- [ ] Docs updated if user-facing behavior changed (`README.md`, `docs/content/`)
- [ ] `CHANGELOG.md` updated under `## Unreleased` if this is user-visible
- [ ] Version bumped only if this PR is meant to ship — use `cargo xtask version <X.Y.Z>`, and apply the `allow-version-bump` label (see [CONTRIBUTING](../CONTRIBUTING.md#cutting-a-release))
