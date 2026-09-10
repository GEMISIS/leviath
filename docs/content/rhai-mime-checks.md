---
title: Rhai mime checks
description: Say what bytes must look like to be stored as one of your mime types, so a mislabelled file is refused where it arrives, not three tools later.
group: Reference
group_order: 3
order: 13
---

# Rhai mime checks

A [mime type](/docs/mime) is a claim. Leviath checks the claim's spelling wherever a type is
written, and its registry's magic prefixes and extensions decide a type for bytes nobody named,
but nothing asks whether an upload that arrived as `image/png` is a PNG. For the built-in types
that is a fair trade: a provider handed a broken PNG says so. For a type of your own it is not,
because nothing downstream knows the format at all, and a mislabelled file fails wherever it is
first read rather than where it came in.

So a registry row can name a check: a `.rhai` file that sees the bytes and the type they claim,
and says what is wrong when something is.

## Write one

```rhai
// checks/scene.rhai
fn check(bytes, mime_type) {
    if bytes.len() < 8 {
        return "too short to be a scene file";
    }
    if bytes.extract(0, 4) != "ACME".to_blob() {
        return "missing the ACME tag";
    }
    ()   // fine
}
```

One function, one contract: **return `()` when the bytes are what they claim, or a string saying
why they are not**. The bytes arrive as a Rhai blob (`len`, indexing, `extract`, `to_blob` and the
rest of the blob API are all there) and the type as a string, so one script can answer for a whole
family: a `check` on `image/*` sees `"image/png"` and `"image/webp"` in turn.

## Name it in a row

```toml
# ~/.leviath/mime_types.toml, or [mime_types] in a blueprint
["application/x-acme-scene"]
family = "model"
extensions = ["scene"]
check = "checks/scene.rhai"
```

The path is relative to the file that names it: the config's directory for
[`mime_types.toml`](/docs/configuration#mime_typestoml) and a `[mime_types]` table in
`config.toml`, the blueprint's own directory for a row in a manifest. It has to resolve inside
that directory, the same fence every other script gets. `lev mime add <type> --check <path>`
writes the row for you.

A check resolves like every other field in a row. A subtype inherits its family's check, a row
that names its own replaces it, and `check = ""` lifts one a broader row put on the type.
`lev mime list` shows the check each type answers to, and `lev mime check <file>` runs it and
prints the verdict.

## What it covers

The check runs once, where bytes come to rest: in the run's blob store, before anything is
written. That one line covers every way bytes reach a run, so you never have to remember it at a
call site:

| Bytes arriving as | What refuses them |
|---|---|
| `lev run --attach`, an `@path` in a task, a multipart upload | the spawn or the message, naming the part and the reason |
| a tool result, a script's `write_part`, `context_attach` | the tool's own result, as an `[error]` the model reads |
| a `read_file` on a file of your type | the read, so the model learns the file is not what its name says |
| a model's own reply | a line in the reply saying what was dropped and why |
| `submit_output` artifacts | the submission, like a schema failure |

The reason goes back verbatim to whoever handed the bytes in, so write it for them.

## When the check itself fails

A check that throws, runs past its operation budget, or returns something that is neither `()`
nor a string is broken, and bytes it could not look at have not passed it: they are refused,
with the script's own error in the reason. There is no `accept` policy here as there is for
[output validators](/docs/rhai-validators#when-the-validator-itself-fails), because a byte check
is a gate on what enters the run, and a gate that opens when it breaks is not one.

A check is compiled when the registry is built: at daemon boot and on every reload for the
operator's rows, at spawn for a blueprint's. A script that is missing, does not compile, or
defines `check` with the wrong arity is a load error named by row, reported by `lev doctor` and
`lev mime`, and the daemon keeps the compiled defaults rather than a registry with a check it
cannot run.

## The sandbox

The same hardened engine the validators use: no filesystem, no network, no `eval`, `print` muted,
with a larger operation budget (five million) so a byte-by-byte scan of a large file finishes.
The [functions every script can call](/docs/scripting#what-every-script-can-call) are there;
nothing that reaches outside the process is.
