---
title: More than text
description: Agents that read, draw, speak and film. Images, audio, video, documents and 3D models move through regions, tools, stages and outputs.
group: Concepts
group_order: 2
order: 9
---

# More than text

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
| `tokens` | One of `per_byte`, `per_pixel`, `per_second`, `per_page` or `fixed` |
| `extensions` | Extensions, without the dot, that imply this type |
| `magic` | A hex prefix that identifies the bytes. `??` stands for any one byte |
| `stand_in` | What a consumer that cannot take the type sees; `{type}` `{name}` `{size}` `{dims}` `{duration}` `{pages}` |
| `check` | A [Rhai script](/docs/rhai-mime-checks) that refuses bytes which are not what they claim; `""` lifts a broader row's check |

A token rule is written out in full:

```toml
tokens = { per_byte = 0.25 }
tokens = { per_pixel = 750, max = 1600 }
tokens = { per_second = 32 }
tokens = { per_page = 2000 }
tokens = { fixed = 1000 }
```

In a `magic` prefix, `??` lets a tag past a length field be named, so
`52494646????????57454250` is RIFF, four bytes of size, WEBP.

Rows layer, and a later row wins. The compiled defaults come first. Then the rows every configured
[Rhai provider ships](/docs/rhai-providers) for the types its models are built for. Then a
`[mime_types]` table in your config, then
[`mime_types.toml`](/docs/configuration#mime_typestoml) beside it, then the
[`[mime_types]` a blueprint carries](/docs/agents#mime-types-the-agent-brings). So your own rows
and a blueprint's both win over a provider's. A row names only what it changes: adding an extension to `image/png`
keeps its family and token rule. `lev mime add <type>` writes a row from the command line, and
`lev mime list` prints the effective table with the source of every row. `lev mime check <file>`
says what type a file resolves to, what its stand-in looks like, and how it reaches a model.
`lev models list --accepts <type>` names the models that take it natively.

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
a `check`. The script runs once, where bytes are stored. An upload, a tool result, a
`read_file`, a model's reply and an artifact are all refused with the reason when they are not
what they claim. Without one, a declared type is taken at its word, as every provider takes it.

## What a model sees

A model declares what it takes, as mime types. Anthropic and OpenAI models list `image/*` and
`application/pdf`; Gemini and Meta's Muse Spark add `audio/*` and `video/*`; a local model you describe in
`[model_capabilities]` lists whatever it can do. Where a provider publishes this per model,
Leviath reads it: OpenRouter's catalogue carries each model's input and output modalities and
Ollama's `/api/show` reports vision, and both win over the built-in tables. The vendors whose
APIs stay quiet (Anthropic, OpenAI, Google direct) are carried in a compiled table.
`cargo xtask modalities` refreshes it from OpenRouter's catalogue, the way `cargo xtask prices`
refreshes list prices. So `claude-3-haiku`, which takes images but not PDFs, is described
precisely rather than by a whole-vendor guess. `lev models show` names a model's input and
output types.
When a request is built, each stored part goes one of three ways:

| Delivery | When | What is sent |
|---|---|---|
| native | the model's `input_types` cover the part's type | the bytes as that provider's image, audio or document block, or the id of an uploaded copy |
| text | the registry says `text = true`, the stage's `as_text` names the type, or the part says `deliver = "text"` | the bytes decoded as UTF-8, as an ordinary text block |
| stand-in | anything else | one line: `[image/png 1024x768, 240 KB] hero.png` |

A native part that the provider stores for you is named by id instead of resent. See
[Files and size limits](#files-and-size-limits).

The text bypass is what lets a `model/obj` file reach a model that has never heard of 3D
models, while a tool declaring `@accepts model/obj` still receives it typed. The stand-in is
what keeps every text-only model working: it always names the part, so the model can pass
that name to a tool that can read it.

Stored parts are charged to their region like text is. The registry's token rule is the
estimate; an image is billed by its pixels when the header could be read, and a provider's
own count corrects the estimate after the first call.

## What a model hands back

A model that draws or speaks answers with bytes as well as words. The image, video, speech,
transcription and music models on OpenAI, Google, AWS Bedrock, xAI and Meta hand back images, MP4
videos, audio and transcripts this way. See
[Image, video and audio models](/docs/providers#image-video-and-audio-models).
[Meshy](/docs/providers#meshy) hands back 3D models the same way.
A stage whose output routing or format names an image, video or audio type, and whose model
answers with words only, is told so and asked again, up to three times. A reply with no bytes is
usually a generation the vendor refused. An OpenAI-shaped provider
reads them off the message as data URIs, from OpenRouter's `images` list and from `image_url`
items in a content array, streamed or not; a [Rhai provider](/docs/rhai-providers) returns them
under `parts`. The runtime stores each one in the run's blob store and writes it beside the
reply's text on the assistant turn, named as the provider named it (`image-1.png` when it did
not). The next request, `lev blobs`, the dashboard's Context view and the API all see it, and a
later `context_export` or `submit_output` can hand it on as an artifact. A part the run
cannot keep (over `[mime] max_part_bytes`, or a world with no store) becomes a line in the
reply saying what was dropped. The same line goes in the stage's log, and a warning in the
daemon's. A stage left with nothing to hand back then reads as a ceiling, not a model that made
nothing. A plain URL in a reply is never fetched.

Most models that draw cannot call tools (`lev models show` says `supports_tools = false`), and
their providers refuse a request that carries a function call anywhere in it. A stage on such a
model is offered no tool, whatever it grants. It sees what earlier stages' tools did as prose,
so it can take a review stage's notes and draw again without the run dying on its second visit.

## Regions hold typed inputs

A region can say what it accepts and how many stored parts it holds:

```toml
[context.regions]
brief         = { kind = "pinned", seed = "task_input", accepts = ["text/*"] }
voice_samples = { kind = "pinned", seed = "input", accepts = ["audio/*"] }
storyboard    = { kind = "pinned", seed = "input", accepts = ["image/*"] }
```

A stage's inputs are the regions it can see, so a stage that reads those three has three typed
inputs and nothing new to declare. A write that does not match `accepts` is refused and says
what the region does take. A region is bounded by its token budget: past it the oldest entry
is evicted, or the write is refused under `admission = "reject"`.

When a stage lists several models, the one that takes what the stage's regions accept goes
first, so a stage reading a storyboard lands on the model that can see it. `lev validate` says
what each stage takes. It warns (`mime-unseen`) when its models can see none of the types its
regions take. When they see some and not the rest, it says so as information. That is how a
pipeline that draws in one stage and builds in the next looks. Two keys under
`[stages.<name>.input]` adjust this. `accepts` states the types
outright. `as_text` names types whose parts reach the model as text whatever it takes,
which is how a `model/obj` scene gets to a text model even when the registry calls it binary.

## What a tool may be handed

A tool says what it takes (`@accepts` on a script, the built-in tables), and a stage can
narrow that further for its own turn:

```toml
[stages.review.tool_accepts]
spawn_agent = ["image/*", "audio/*"]   # a sub-agent started here gets pictures and sound, never the video
context_export = ["text/*"]            # only text files may be written back into the workdir here
```

A stored part outside a tool's list is out of that tool's reach at the stage. `spawn_agent`'s
`parts` refuses it by name, and `context_export` refuses it the same way. A script's
`list_parts` does not show it, and its `read_part` says which types the tool may be handed.
Inline text
is never hidden by a limit, and a tool absent from the table keeps whatever it takes itself.
`lev validate` prints each stage's limits and warns (`tool-accepts-ungranted`) about a limit
on a tool the stage does not grant; the dashboard's agent editor sets them on the Models and
tools tab.

A [Rhai tool](/docs/rhai-tools) makes a part with `write_part`, from bytes it got one of three
ways. `read_part` takes a part the run already holds, and `http_get_bytes` takes a download.
`read_file_bytes` takes a file in the working directory, such as an image its own shell command
rendered or a design someone left there. A limit hides parts, not files: `read_file_bytes` is
gated by the `read_file` permission instead.

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

`--attach path[:region][:type][:text|native|stand_in]` puts a file in a region. An `@path` inside any text does
the same for the region the text lands in, and keeps the text exactly as written so the model
and the stand-in agree on the name. Write `\@` for a literal `@`. A token that names no file is
left alone, so an email address is never mistaken for one. On the command line, paths resolve
from where you ran the command; over the API they resolve inside the run's working directory.
A `--<region> @file` whose bytes are not text is attached to that region as a part rather than
read as its seed. The daemon types every part with its own registry, so a file the CLI could
not name still gets the type your `[mime_types]` rows give it. `:type` overrides that, and
`:text`, `:native` or `:stand_in` override how the part reaches the model.

Over HTTP, `POST /api/agents` and `POST /api/agents/{id}/message` take `multipart/form-data`
with any number of file parts, or a JSON `parts` list naming files already inside the workdir.
[The API page](/docs/api) has the shapes.

## Files and size limits

A provider with a Files API takes a part once and lets later requests name it by id. A 30 MB
PDF then crosses the network once for the whole run, rather than on every turn and every retry.
When a request carries a stored part its model takes natively, and the provider can store that type,
Leviath uploads it the first time and names it by id after. The same part in a later stage, a
retry, or the next turn reuses the upload.

| Provider | Uploaded | Largest file | Inline limits, when not uploaded |
|---|---|---|---|
| Anthropic | images, PDFs, plain text | 500 MB | 32 MB a request, 5 MB an image |
| OpenAI | images, PDFs | 512 MB | 50 MB a file, 20 MB an image |
| Google | images, audio, video, PDFs | 2 GB | 100 MB a request, 50 MB a PDF |
| xAI and Grok | PDFs, plain text | 512 MB | 20 MB an image |
| Meta | images, audio, video, PDFs | 1 GiB | 50 MB a request |
| Bedrock | nothing (no Files API) | | 3.75 MB an image, 4.5 MB a document, 25 MB a video |
| everything else | nothing | | `[mime] max_media_bytes_per_request` |

Nothing is uploaded when:

- **zero data retention is on.** An upload is data the provider keeps, so the switch turns
  uploads off whatever else says. Anthropic's Files API is not eligible for zero data retention.
- **`[providers] file_uploads = false`.** The same switch is on the Defaults screen of
  `lev setup` ("Upload media to provider file storage") and is `--file-uploads false` headlessly.
- **the run's store keeps nothing on disk**, as an embedder's in-memory store does, since there
  would be no record to delete the uploads from.

A part sent inline is held to the provider's inline limits. One over a limit reaches the model
as its stand-in with the reason, and when the provider could have stored it, why it was not
uploaded:

```
[application/pdf, 60.0 MiB] report.pdf [not sent: 60.0 MiB is over the 50.0 MiB this provider takes inline; zero data retention is on, so nothing is uploaded]
```

Each run records its uploads in `provider-files.json` in its directory. The files are deleted
from the provider when the run finishes, and when the run is deleted (from `lev serve`, the
dashboard, or `lev doctor`). They are also deleted when the daemon starts for a run that finished
while it was not running. Every
upload also asks the provider to delete it after `[mime] provider_file_ttl_secs` (a day by
default, clamped to what each provider takes; Google always keeps a file 48 hours). A request
the provider refuses because a file it names is gone uploads the part again and is retried once.

## Limits

```toml
[mime]
# max_part_bytes = 33554432             # one part, at every ingress
inline_text_bytes = 1048576             # text kept inside the entry before it is stored by hash
max_media_bytes_per_request = 20971520  # stored media one request carries, where a provider names no limit
provider_file_ttl_secs = 86400          # how long an upload lives in a provider's file storage
```

`max_part_bytes`, left unset, is the largest part a configured provider takes, by upload or
inline: 1 GiB with Meta, 500 MiB with Anthropic, 32 MiB when no configured provider names a
limit. A value you set always wins. `lev mime list` prints the ceiling in force, where it came
from, whether uploads are on, and each configured provider's limits.

`max_media_bytes_per_request` is a backstop for the vendor request-size limits a token budget
cannot see, for a provider that documents none of its own. An image costs the same few thousand
tokens whatever its byte size, so a request can sit inside its context window and still be
megabytes of media. Past it, the oldest stored parts are sent as their stand-ins.

`lev doctor` reports a `[mime_types]` row that will not load, or a `check` it cannot compile;
the daemon keeps the built-in table until it is fixed.
