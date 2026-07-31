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
   group: Concepts
   group_order: 2
   order: 9
   ---
   ```

   - `group` is the sidebar section. `group_order` sets the order of the sections; use the same
     number for every page in a section. Current sections: `Get started` (1), `Concepts` (2),
     `Reference` (3), `Guides` (4).
   - `order` is the page's position within its section.

3. Write the page. You can use:
   - Fenced code blocks with a language for highlighting (```bash, ```toml, ```rhai).
   - ```` ```mermaid ```` blocks for diagrams (flowchart, sequenceDiagram, stateDiagram-v2). They
     render in the browser, so a syntax error breaks the diagram. Check yours before committing.
   - Callouts: `> [!NOTE]`, `> [!TIP]`, `> [!IMPORTANT]`, `> [!WARNING]`, `> [!CAUTION]` (put the
     marker on its own line).
   - Cross-links as `/docs/<slug>` (no channel). The renderer rewrites them to the reader's channel.

4. Keep the prose plain and direct. No em dashes.

## Conventions

- One page per concept or reference surface. Concept pages explain why and link out to the
  reference for the exact flags and fields, rather than duplicating them.
- Match a claim to the source. If a flag, field, or route name changes in the code, update the doc
  in the same change.
- Generated agent workflow diagrams live in `docs/assets/agents/`.
