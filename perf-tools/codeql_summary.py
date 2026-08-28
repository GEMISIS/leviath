#!/usr/bin/env python3
"""Summarise a CodeQL SARIF file: one line per finding, with its data flows.

Usage: codeql_summary.py target/codeql.sarif

Prints the rule, the sink, and each distinct flow (source first) so a finding
can be traced to the line it starts from without opening the Security tab.
Exit status is the number of findings, capped at 125, so a shell can gate on it.
"""

import json
import sys


def where(location):
    physical = location["physicalLocation"]
    name = physical["artifactLocation"]["uri"].split("/")[-1]
    return f"{name}:{physical['region']['startLine']}"


def flows(result):
    seen = set()
    for flow in result.get("codeFlows", []):
        steps = []
        for step in flow["threadFlows"][0]["locations"]:
            here = where(step["location"])
            if not steps or steps[-1] != here:
                steps.append(here)
        chain = " > ".join(steps)
        if chain not in seen:
            seen.add(chain)
            yield chain


def main(path):
    with open(path, encoding="utf-8") as handle:
        sarif = json.load(handle)
    results = sarif["runs"][0]["results"]
    by_rule = {}
    for result in results:
        by_rule.setdefault(result["ruleId"], []).append(result)
    for rule, hits in sorted(by_rule.items()):
        print(f"{rule}: {len(hits)}")
        for hit in hits:
            physical = hit["locations"][0]["physicalLocation"]
            print(f"  SINK {physical['artifactLocation']['uri']}:{physical['region']['startLine']}")
            for chain in flows(hit):
                print(f"    {chain}")
    print(f"total: {len(results)}")
    return min(len(results), 125)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
