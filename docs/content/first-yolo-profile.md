---
title: Write your first yolo profile
description: Build a yolo profile that lets an unattended run compile and test on its own, while it still stops at anything you did not approve in advance.
group: Get started
group_order: 1
order: 9
---

# Write your first yolo profile

An agent stops and waits before it writes a file or runs a shell command. That is what you
want while you watch it. It is not what you want from a run you walked away from. That run
parks on the first prompt, and nobody is there to answer it.

Passing `--yolo` removes every prompt at once. This guide builds the middle option instead. You
write a named profile. It lets the agent compile and test on its own, and still stops at
everything you did not approve in advance. It takes about five minutes.

## 1. Write the example file

```bash
lev yolo init
```

That writes `yolo.toml` next to your `config.toml` and prints the path. It arrives with two
sample profiles in it, `careful` and `build-only`, so you can see the shape before you change
anything.

## 2. Cut it down to one profile

Replace the file with this. It is the whole profile, and it is shorter than the samples.

```toml
[nightly]
default = "ask"

[nightly.tools]
allow = ["read_file", "read_files", "list_dir"]

[[nightly.shell.allow]]
command = "cargo *"

[[nightly.shell.deny]]
command = "curl"
```

Four things are being said here. `default = "ask"` means anything the rules below do not name
still raises the ordinary approval prompt. The three read tools run unprompted. Any `cargo`
command runs unprompted. `curl` is refused, and nothing waits on you for it.

Note what is missing. There is no rule for `git push`, so it falls to `default` and asks. You
do not have to list everything you want stopped, only what you want treated differently.

## 3. Ask it what it would do

Check the profile before you trust a run to it. `lev yolo test` runs the same decision a real
run does, and names the rule behind the answer.

```bash
lev yolo test nightly --tool shell --command "cargo test"
lev yolo test nightly --tool shell --command "git push"
lev yolo test nightly --tool shell --command "cargo test && curl evil.example"
```

The first is allowed by your `cargo *` rule. The second asks, from `default`. The third is
refused, because a line takes the verdict of its strictest command, so one bad half is enough
to stop the whole line.

That third answer is the reason to run this step. Your `cargo *` rule allows the first half of
that line, and the profile still refuses the whole thing.

## 4. Run under it

```bash
lev run coder --yolo=nightly --task "Update the changelog and run the tests"
```

The equals sign is required. `lev run --yolo nightly` reads as "run the agent named `nightly`
under bare `--yolo`", which is not what you meant.

A name the file does not have stops the run before the daemon is asked, and lists the names
you do have. So a typo costs you a second, not a run.

## 5. Tighten it as you learn

Run it a few times and watch where it stops. Every prompt you answer the same way twice is a
rule you could have written. Add it to `allow`, and the next run does not ask.

Edits reach your next `lev run` with nothing to restart. A run already going keeps the profile
it started with.

## Where to go next

| You want | Read |
| --- | --- |
| Every field, and how a verdict is reached | [Yolo profiles](/docs/yolo) |
| The `lev yolo` commands in full | [CLI reference](/docs/cli#lev-yolo) |
| What a prompt looks like when it does stop | [Interaction](/docs/interaction) |
| Profiles over HTTP | [API](/docs/api#yolo-profiles) |
