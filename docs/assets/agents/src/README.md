# Workflow diagram sources

The SVGs in `docs/assets/agents/` are generated from the blueprints in
`agents/` — do not edit the `.mmd` files or SVGs by hand.

```bash
# from the repo root
python3 docs/assets/agents/src/generate.py --render
```

This regenerates every `.mmd` from `agents/<name>/agent.leviath` and renders
`<name>.svg` (light) and `<name>-dark.svg` per agent via mermaid-cli (through
`npx`, downloaded on first use), themed by `theme-light.json` /
`theme-dark.json`. The README embeds both variants with
`<picture><source media="(prefers-color-scheme: dark)">`.

Conventions: the `error_recovery` stage is omitted; diamonds are LLM-routed or
human-in-the-loop decision stages; dotted edges fire on runtime conditions
(e.g. `stuck`); thick edges are fan-out/merge; the entry stage has a thicker
border.
