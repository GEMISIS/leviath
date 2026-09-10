---
title: Typed mime
description: Put images, audio, video, documents and 3D models into regions, tools, stages and outputs, and let a text model still read them.
group: Concepts
group_order: 2
order: 9
---

# Typed mime

An agent that only moves text cannot look at the mockup you are asking it to edit, cannot hand
back the video it rendered, and cannot pass an image from one stage to the next. Leviath moves
typed **parts** instead. A part is one piece of content with a mime type: a paragraph, a PNG,
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

Leviath does not know what an image is. A **mime registry** says what each type is, and you can
extend it. Every type resolves to a row with a family, a text flag, a token rule, extensions,
and optionally a magic prefix and a stand-in template. A row for `image/*` supplies defaults
for every image subtype; `*/*` is the last resort.

```toml
# ~/.leviath/mime_types.toml (lev mime init writes an example)
["model/obj"]
extensions = ["obj"]
text = true                      # UTF-8 under the hood: may reach a text model as text

["application/x-acme-scene"]
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
| `check` | A [Rhai script](/docs/rhai-mime-checks) that refuses bytes which are not what they claim; `""` lifts a broader row's check |

Rows layer. The compiled defaults come first, then a `[mime_types]` table in your config, then
[`mime_types.toml`](/docs/configuration#mime_typestoml) beside it, then the
[`[mime_types]` a blueprint carries](/docs/agents#mime-types-the-agent-brings), and the [rows a Rhai provider ships](/docs/rhai-providers) for the types its models are built for, which reach
that agent's runs only. A row names only what it changes: adding an extension to `image/png`
keeps its family and token rule. `lev mime add <type>` writes a row from the command line,
`lev mime list` prints the effective table with the source of every row, and
`lev mime check <file>` says what type a file resolves to, what its stand-in looks like, and
how it reaches a model; `lev models list --accepts <type>` names the models that take it
natively.

Every run reads its own copy of the table: the operator's rows with the blueprint's on top,
built when the run spawns. Edit `mime_types.toml` or the config while runs are live and they
pick the change up too, within the daemon's housekeeping interval of thirty seconds; a new run
sees it at once.

A file's type is decided in a fixed order: the type the sender declared, then the registry's
magic prefixes, then the extension, then valid UTF-8 counts as `text/plain`, and anything else
is `application/octet-stream`.

A type is a claim, and the claim is checked in two places. Its spelling is checked wherever a
type is written: lowercase `type/subtype`, no parameters, `type/*` only where a capability or
an `accepts` list is declared. Whether the bytes are that type is checked only when a row names
a `check`: the script runs once, where bytes are stored, so an upload, a tool result, a
`read_file`, a model's reply and an artifact are all refused with the reason when they are not
what they claim. Without one, a declared type is taken at its word, as every provider takes it.

## What a model sees

A model declares what it takes, as mime types. Anthropic and OpenAI models list `image/*` and
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

## What a model hands back

A model that draws or speaks answers with bytes as well as words. An OpenAI-shaped provider
reads them off the message as data URIs, from OpenRouter's `images` list and from `image_url`
items in a content array, streamed or not; a [Rhai provider](/docs/rhai-providers) returns them
under `parts`. The runtime stores each one in the run's blob store and writes it beside the
reply's text on the assistant turn, named as the provider named it (`image-1.png` when it did
not), so the next request, `lev blobs`, the dashboard's Context view and the API all see it,
and a later `context_export` or `submit_output` can hand it on as an artifact. A part the run
cannot keep (over `[mime] max_part_bytes`, or a world with no store) becomes a line in the
reply saying what was dropped. A plain URL in a reply is never fetched.

Most models that draw cannot call tools (`lev models show` says `supports_tools = false`), and
their providers refuse a request that carries a function call anywhere in it. A stage on such a
model is offered no tool, whatever it grants, and sees what earlier stages' tools did as prose,
so it can take a review stage's notes and draw again without the run dying on its second visit.

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
what each stage takes and warns (`mime-unseen`) when none of its models can see a type its
regions take. Two keys under `[stages.<name>.input]` adjust this: `accepts` states the types
outright, and `as_text` names types whose parts reach the model as text whatever it takes,
which is how a `model/obj` scene gets to a text model even when the registry calls it binary.

## What a tool may be handed

A tool says what it takes (`@accepts` on a script, the built-in tables), and a stage can
narrow that further for its own turn:

```toml
[stages.review.tool_accepts]
spawn_agent = ["image/*", "audio/*"]   # a sub-agent started here gets pictures and sound, never the video
context_export = ["text/*"]            # only text files may be written back into the workdir here
```

A stored part outside a tool's list is out of that tool's reach at the stage: `spawn_agent`'s
`parts` refuses it by name, a script's `list_parts` does not show it and its `read_part` says
which types the tool may be handed, and `context_export` refuses it the same way. Inline text
is never hidden by a limit, and a tool absent from the table keeps whatever it takes itself.
`lev validate` prints each stage's limits and warns (`tool-accepts-ungranted`) about a limit
on a tool the stage does not grant; the dashboard's agent editor sets them on the Models and
tools tab.

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
not name still gets the type your `[mime_types]` rows give it; `:type` overrides that, and
`:text`, `:native` or `:stand_in` override how the part reaches the model.

Over HTTP, `POST /api/agents` and `POST /api/agents/{id}/message` take `multipart/form-data`
with any number of file parts, or a JSON `parts` list naming files already inside the workdir.
[The API page](/docs/api) has the shapes.

## Limits

```toml
[mime]
max_part_bytes = 33554432        # one part, at every ingress
inline_text_bytes = 1048576      # text kept inside the entry before it is stored by hash
max_stored_per_request = 100     # stored parts one model request carries
```

`lev doctor` reports a `[mime_types]` row that will not load, or a `check` it cannot compile;
the daemon keeps the built-in table until it is fixed.
