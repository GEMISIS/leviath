# Documentation

The public docs at [leviath.dev/docs](https://leviath.dev/docs) are built from the Markdown in
`docs/content/`. This file explains how that works and how to keep the docs current as Leviath
gains features.

## How it is generated

```
docs/content/*.md   (this repo, the source of truth)
        |
        v
  docs-ssg           (the renderer, lives in the GEMISIS/leviath.dev repo:
        |             mermaid, syntax highlighting, on-page TOC, callouts, search)
        v
  s3://<site>/docs/<channel>/*.html   (published per release)
        |
        v
  https://leviath.dev/docs/<channel>/<slug>
```

The `publish-docs` workflow runs on every alpha, beta, and prod release. It checks out the
leviath.dev repo, renders `docs/content/` for that channel, and syncs the result to the site's S3
bucket, then invalidates CloudFront. A new build refreshes the docs with no site rebuild. Channels
map to tags: `alpha`, `beta`, and `stable` (the `latest` release).

Because rendering happens in the leviath.dev repo, this repo owns the words and leviath.dev owns the
look. You do not run a build here; you write Markdown, and it ships on the next release.

## Adding docs for a new feature

1. Add or edit a file in `docs/content/`. The file name is the URL slug (`stages.md` becomes
   `/docs/<channel>/stages`).
2. Give it frontmatter:

   ```markdown
   ---
   title: My Feature
   description: What a reader gets from this page, in one sentence.
   group: Concepts
   group_order: 2
   order: 9
   ---
   ```

   - `group` is the sidebar section. `group_order` sets the order of the sections; use the same
     number for every page in a section. Current sections: `Get started` (1), `Concepts` (2),
     `Reference` (3), `Guides` (4), `Integrations` (5).
   - `order` is the page's position within its section.
   - `description` is one sentence, under 160 characters. It is the line beside this page's link in
     [`llms.txt`](https://leviath.dev/llms.txt), which is how a coding agent decides whether to fetch
     the page at all. Say what the reader gets, not what the page is called. A description that
     restates the title costs the agent a wasted round trip.

3. Write the page. You can use:
   - Fenced code blocks with a language for highlighting (```bash, ```toml, ```rhai).
   - ```` ```mermaid ```` blocks for diagrams (flowchart, sequenceDiagram, stateDiagram-v2). They
     render in the browser, so a syntax error breaks the diagram. Check yours before committing.
   - Callouts: `> [!NOTE]`, `> [!TIP]`, `> [!IMPORTANT]`, `> [!WARNING]`, `> [!CAUTION]` (put the
     marker on its own line).
   - `<details>`/`<summary>` blocks for optional depth (extra install methods, scriptable
     variants). Leave a blank line after `</summary>` so the markdown inside still renders, and
     keep headings out of them, since collapsed headings vanish from the on-page TOC.
   - Cross-links as `/docs/<slug>` (no channel). The renderer rewrites them to the reader's channel.

4. Write it to the rules below.

## Writing rules

Someone should be able to read any page here without already knowing what Leviath is. That is the
whole bar. These rules exist because the docs drifted away from it once already.

1. **Open with the problem, not the mechanism.** First sentence: what goes wrong without this.
   Second sentence: what Leviath does about it. Save how it works for later.
2. **Define a term in the sentence that first uses it**, or link it to `/docs/glossary`. Never
   define a term after its first use, and never inside a callout the reader may have skipped.
3. **Sentences stay under 25 words.** Hard ceiling 35. If a sentence has an "and", a semicolon, or
   a dash holding two halves together, it is usually two sentences.
4. **One idea per paragraph**, four sentences at most.
5. **Active voice, second person.** "You set X", not "X is set".
6. **A runnable example inside the first screenful**, before any table of options.
7. **No em dashes, and no ` - ` afterthought clauses either.** Both are the same habit. Make it a
   new sentence.
8. **Table cells stay under 20 words.** Anything longer belongs in prose under the table. A cell
   with five sentences in it defeats the point of a table.
9. **No in-group phrasing.** No "this surprises people", no "not vibes", no calling a fleet of
   agents a "factory". Write it the way you would explain it to someone on their first day.
10. **A decision tree gets a table or a diagram, never a paragraph.**
11. **Reference pages hold facts, concept pages hold reasons.** Design rationale in a flag table is
    in the wrong place.

## Tone rules

These pages describe what Leviath does well, not what other tools do badly. Someone who uses Gas
City, OpenHands, Claude Code, CrewAI, or LangGraph should finish a page here feeling their tool got
more useful, not that it got insulted.

1. **Describe, do not rate.** Say what a tool does and where it sits. Compare models, not merit.
2. **Every claim about another project comes from that project's docs**, and links them. If we
   cannot source it upstream, it does not ship.
3. **No strawman openers.** Rule 1 above says open with the problem, and the lazy way to do that is
   to make the problem someone else's tool. "Most agent tools hand an LLM a flat array and hope for
   the best" is a jab. "A flat message array pushes your system prompt out of the window when a big
   file lands" is the same point with nobody diminished.
4. **Different bets, not better bets.** Where designs differ, say what each one buys and what it
   costs. A process per agent buys real isolation and a blast radius of one. A shared world buys
   density and pooled rate limits, and gives up that isolation. Say both halves.
5. **Name our own weaknesses first.** The "when to use something else" material and any row we do
   not pass on the 12-factor scorecard stay, and stay prominent. Say it because a reader deciding
   between tools deserves it, not as a device to sound credible. Never write a sentence that points
   at our own honesty; state the limit and move on. If a limit is being worked on, say so and link
   the issue.
6. **Integration pages exist to make the other tool work better.** Including saying plainly when
   Leviath is the wrong choice for a job.
7. **No dismissive vocabulary.** Not "just", "merely", "naive", "toy", "the old way", "unlike X",
   "hope for the best". A design you disagree with is a tradeoff, not an oversight.

## Conventions

- One page per concept or reference surface. Concept pages explain why and link out to the
  reference for the exact flags and fields, rather than duplicating them.
- Match a claim to the source. If a flag, field, or route name changes in the code, update the doc
  in the same change.
- Diagrams are mermaid `flowchart` or `sequenceDiagram`, with quoted labels and no `%%{init}%%`
  directives or `click` handlers. The site renderer is more forgiving than the VS Code preview, so
  check yours in the editor preview too, not only on the site.
- Generated agent workflow diagrams live in `docs/assets/agents/`.
