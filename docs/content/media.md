---
title: Typed media
description: Put images, audio, video, documents and 3D models into regions, tools, stages and outputs, and let a text model still read them.
group: Concepts
group_order: 2
order: 9
---

# Typed media

An agent that only moves text cannot look at the mockup you are asking it to edit, cannot hand
back the video it rendered, and cannot pass an image from one stage to the next. Leviath moves
typed **parts** instead. A part is one piece of content with a media type: a paragraph, a PNG,
a WAV clip, an MP4, a PDF, an OBJ model. Text is a part like any other. The only thing that sets
it apart is that its bytes travel inside the entry and reach a model directly.

```bash
lev run sprite-editor --task "edit sprite image @hero_idle.png so the arm is longer"
```

That task lands as one entry with two parts: the sentence, and `hero_idle.png`. The model sees
the image if it can take one, and a one-line stand-in for it if it cannot. The stage's tools see
the typed file either way.

## Where a part can go

| Place | What carries parts |
|---|---|
| A context region | Every entry is a list of parts. Text parts are inline, the rest are stored by hash. |
| A tool result | A tool returns text and any number of typed parts. |
| A user message | `lev run`, `lev msg`, `lev respond`, the dashboard, the API and the Agent Client Protocol all attach files. |
| A model reply | A model that emits images hands them back as parts on its turn. |
| A final output | The answer is a text part; declared artifacts are typed files the run produced. |

Bytes are only touched at the edges. When a part arrives it is written once under
`<run>/blobs/<sha256>`, and when a request is built the bytes are read back for the model.
Everything between, the journal, the snapshots, the events, carries a reference: hash, type,
size, dimensions, a token estimate. Deleting a run deletes its blobs.

## The registry

Leviath does not know what an image is. A **media registry** says what each type is, and you can
extend it. Every type resolves to a row with a family, a text flag, a token rule, extensions,
and optionally a magic prefix and a stand-in template. A row for `image/*` supplies defaults
for every image subtype; `*/*` is the last resort.

```toml
# ~/.leviath/config.toml, or [media_types] in an agent's blueprint
[media_types."model/obj"]
extensions = ["obj"]
text = true                      # UTF-8 under the hood: may reach a text model as text

[media_types."application/x-acme-scene"]
family = "model"
extensions = ["scene"]
magic = "41434D45"
tokens = { per_byte = 0.1 }
stand_in = "[{type} {size}] {name}"
```

| Key | Meaning |
|---|---|
| `family` | What providers key their encoders on: `text`, `image`, `audio`, `video`, `document`, `model`, `binary`, or a name of your own |
| `text` | The bytes are UTF-8 and may travel inline and reach any text model as text |
| `tokens` | One of `{ per_byte = 0.25 }`, `{ per_pixel = 750, max = 1600 }`, `{ per_second = 32 }`, `{ fixed = 1000 }` |
| `extensions` | Extensions, without the dot, that imply this type |
| `magic` | A hex prefix that identifies the bytes |
| `stand_in` | What a consumer that cannot take the type sees; `{type}` `{name}` `{size}` `{dims}` `{duration}` |

Rows layer. The compiled defaults come first, then your config, then the blueprint's own
`[media_types]`, then rows a Rhai provider declares. A row names only what it changes: adding
an extension to `image/png` keeps its family and token rule. `lev media list` prints the
effective table with the source of every row, and `lev media check <file>` says what type a
file resolves to, what its stand-in looks like, and how it reaches a model; `lev models list
--accepts <type>` names the models that take it natively.

A file's type is decided in a fixed order: the type the sender declared, then the registry's
magic prefixes, then the extension, then valid UTF-8 counts as `text/plain`, and anything else
is `application/octet-stream`.

## What a model sees

A model declares what it takes, as media types. Anthropic and OpenAI models list `image/*` and
`application/pdf`; Gemini adds `audio/*` and `video/*`; a local model you describe in
`[model_capabilities]` lists whatever it can do. When a request is built, each stored part goes
one of three ways:

| Delivery | When | What is sent |
|---|---|---|
| native | the model's `input_types` cover the part's type | the bytes, as that provider's image, audio or document block |
| text | the registry says `text = true`, or the stage's `as_text` names the type, or the part says `deliver = "text"` | the bytes decoded as UTF-8, as an ordinary text block |
| stand-in | anything else | one line: `[image/png 1024x768, 240 KB] hero.png` |

The text bypass is what lets a `model/obj` file reach a model that has never heard of 3D
models, while a tool declaring `@accepts model/obj` still receives it typed. The stand-in is
what keeps every text-only model working: it always names the part, so the model can pass
that name to a tool that can read it.

Stored parts are charged to their region like text is. The registry's token rule is the
estimate; an image is billed by its pixels when the header could be read, and a provider's
own count corrects the estimate after the first call.

## Regions hold typed inputs

A region can say what it accepts and how many stored parts it holds:

```toml
[context.regions]
brief         = { kind = "pinned", seed = "task_input", accepts = ["text/*"] }
voice_samples = { kind = "pinned", seed = "input", accepts = ["audio/*"], max_stored = 4 }
storyboard    = { kind = "pinned", seed = "input", accepts = ["image/*"], max_stored = 12 }
```

A stage's inputs are the regions it can see, so a stage that reads those three has three typed
inputs and nothing new to declare. A write that does not match `accepts` is refused and says
what the region does take. `max_stored` evicts the oldest entry carrying a stored part, or
refuses the write under `admission = "reject"`.

When a stage lists several models, the one that takes what the stage's regions accept goes
first, so a stage reading a storyboard lands on the model that can see it. `lev validate` says
what each stage takes and warns (`media-unseen`) when none of its models can see a type its
regions take. Two keys under `[stages.<name>.input]` adjust this: `accepts` states the types
outright, and `as_text` names types whose parts reach the model as text whatever it takes,
which is how a `model/obj` scene gets to a text model even when the registry calls it binary.

## Stages declare typed outputs

```toml
[stages.assemble]
available_tools = ["concat_video", "submit_output"]
[[stages.assemble.output.artifacts]]
name = "final"
type = "video/mp4"
required = true
```

`submit_output` names the files, Leviath checks that each exists inside the working directory
and is the type the stage declared, and a missing required artifact is refused back to the
model like a schema failure. Accepted artifacts land in the `final_output` region as parts, so
the next stage sees them, and `lev result` and the API serve them.
[Final outputs](/docs/outputs) has the details.

## Attaching files

```bash
lev run storyteller --task "a 30 second trailer" \
  --attach voice.wav:voice_samples --attach frame1.png:storyboard
lev run reviewer --task "does @mockup.png match @spec.md?"
lev respond <id> --attach marked_up.png "the arm is still wrong, see the circle"
```

`--attach path[:region][:type][:text]` puts a file in a region. An `@path` inside any text does
the same for the region the text lands in, and keeps the text exactly as written so the model
and the stand-in agree on the name. Write `\@` for a literal `@`. A token that names no file is
left alone, so an email address is never mistaken for one. On the command line, paths resolve
from where you ran the command; over the API they resolve inside the run's working directory.
A `--<region> @file` whose bytes are not text is attached to that region as a part rather than
read as its seed. The daemon types every part with its own registry, so a file the CLI could
not name still gets the type your `[media_types]` rows give it; `:type` overrides that, and
`:text`, `:native` or `:stand_in` override how the part reaches the model.

Over HTTP, `POST /api/agents` and `POST /api/agents/{id}/message` take `multipart/form-data`
with any number of file parts, or a JSON `parts` list naming files already inside the workdir.
[The API page](/docs/api) has the shapes.

## Limits

```toml
[media]
max_part_bytes = 33554432        # one part, at every ingress
inline_text_bytes = 1048576      # text kept inside the entry before it is stored by hash
max_stored_per_request = 100     # stored parts one model request carries
```

`lev doctor` reports a `[media_types]` row that will not load; the daemon skips such a row and
types that file by the built-in table until it is fixed.
