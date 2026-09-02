#!/usr/bin/env python3
"""Generate the agent workflow diagrams in docs/assets/agents/ from the
blueprints in agents/ — the diagrams are derived, never hand-drawn, so they
cannot drift from the graphs the runtime actually executes.

Usage (from the repo root):

    python3 docs/assets/agents/src/generate.py            # .mmd sources only
    python3 docs/assets/agents/src/generate.py --render    # + SVGs via mmdc

Rendering uses mermaid-cli through npx (first run downloads it). Each agent
gets <name>.svg (light) and <name>-dark.svg, both with transparent
backgrounds, themed from theme-light.json / theme-dark.json next to this
script.

Conventions (mirrored in the README's Pre-built Agents intro):
  - the error_recovery stage and edges into it are omitted from every graph
  - diamonds are LLM-routed or human-in-the-loop decision stages
  - dotted edges fire automatically on a runtime condition (e.g. `stuck`)
  - thick edges are fan-out: one worker per work item, merged back
"""

from __future__ import annotations

import subprocess
import sys
import tomllib
from pathlib import Path

SRC_DIR = Path(__file__).resolve().parent
OUT_DIR = SRC_DIR.parent
AGENTS_DIR = SRC_DIR.parents[3] / "crates" / "leviath-cli" / "agents"

OMIT = {"error_recovery"}
# Graphs with more than this many visible stages render top-down so they fit
# the README's two-column table without becoming a thin horizontal strip.
TD_THRESHOLD = 5


def blueprint_to_mermaid(path: Path) -> str:
    data = tomllib.loads(path.read_text())
    entry = data["agent"]["entry_stage"]
    stages = {k: v for k, v in data["stages"].items() if k not in OMIT}

    # (from, to, label, style) with style in {solid, condition, fanout}
    edges: list[tuple[str, str, str, str]] = []
    decisions: set[str] = set()
    # Nodes that are not stages of this blueprint: a fan-out worker that is a
    # separate installed agent. Keyed by node id, valued by its label.
    external: dict[str, str] = {}

    for name, stage in stages.items():
        if stage.get("interaction_points"):
            decisions.add(name)
        if stage.get("mode") == "fan_out":
            # A fan-out stage without `merge_stage` (researcher's `dig`) takes
            # its own transitions after the workers return, so it has no merge
            # edge to draw.
            merge = stage.get("merge_stage")
            per = f"×{stage['max_workers']} workers" if "max_workers" in stage else "workers"
            # A worker is either a stage of this blueprint (`worker_stage`) or a
            # separate installed agent (`worker_agent`). The first is a real node
            # in this graph; the second is not, so it gets a labelled node of its
            # own rather than being drawn as a stage that does not exist here.
            worker = stage.get("worker_stage")
            if worker is None:
                agent = stage.get("worker_agent") or stage.get("worker_query", "workers")
                worker = f"{name}_workers"
                external[worker] = f"{agent} (sub-agent)"
            edges.append((name, worker, per, "fanout"))
            if merge is not None:
                edges.append((worker, merge, "merge", "fanout"))

        plain = 0
        for target, spec in stage.get("transitions", {}).items():
            if target in OMIT:
                continue
            cond = spec.get("condition", "") if isinstance(spec, dict) else ""
            if cond == "error":
                continue
            if cond:
                edges.append((name, target, cond, "condition"))
            else:
                edges.append((name, target, "", "solid"))
                plain += 1
        if plain >= 2:
            decisions.add(name)

    direction = "TD" if len(stages) > TD_THRESHOLD else "LR"
    lines = [f"flowchart {direction}"]
    for name in stages:
        label = name.replace("_", " ")
        if name in decisions:
            lines.append(f"    {name}{{{{{label}}}}}")
        else:
            lines.append(f"    {name}({label})")
    # Drawn with a different shape, because it is another agent rather than a
    # stage of this one.
    for node, label in external.items():
        lines.append(f'    {node}[["{label}"]]')
    for frm, to, label, style in edges:
        if style == "condition":
            lines.append(f"    {frm} -. {label} .-> {to}")
        elif style == "fanout":
            lines.append(f"    {frm} ==>|{label}| {to}")
        else:
            lines.append(f"    {frm} --> {to}")
    lines.append("    classDef entry stroke-width:3px")
    lines.append(f"    class {entry} entry")
    return "\n".join(lines) + "\n"


def render(mmd: Path, svg: Path, theme: Path) -> None:
    subprocess.run(
        ["npx", "-y", "@mermaid-js/mermaid-cli", "-i", str(mmd), "-o", str(svg),
         "-c", str(theme), "-b", "transparent", "--svgId", f"wf-{mmd.stem}"],
        check=True,
    )


def main() -> None:
    do_render = "--render" in sys.argv
    blueprints = sorted(AGENTS_DIR.glob("*/agent.leviath"))
    if not blueprints:
        sys.exit(f"no blueprints found under {AGENTS_DIR}")
    for bp in blueprints:
        name = bp.parent.name
        mmd = SRC_DIR / f"{name}.mmd"
        mmd.write_text(blueprint_to_mermaid(bp))
        print(f"wrote {mmd.relative_to(SRC_DIR.parents[2])}")
        if do_render:
            render(mmd, OUT_DIR / f"{name}.svg", SRC_DIR / "theme-light.json")
            render(mmd, OUT_DIR / f"{name}-dark.svg", SRC_DIR / "theme-dark.json")
            print(f"rendered {name}.svg + {name}-dark.svg")


if __name__ == "__main__":
    main()
