#!/usr/bin/env python3
"""Check MIGRATION.md against both schemas.

Usage: python3 check_migration.py [MIGRATION.md] [published schema] [proposed schema]
(defaults, relative to this file: MIGRATION.md, ../../schema/leviath.graphql, leviath.graphql)

MIGRATION.md is a set of tables. A row is `| v1 coordinate | v2 cell | verdict | why |`,
with the v1 coordinate in backticks. Headings only group rows. "v1" is the published
schema; "v2" is this proposal.

Fails when:
  - a coordinate of the published schema (a type, an output field, an argument, an
    input field, an enum value, a union member) has no row;
  - a row names a v1 coordinate that does not exist, or appears twice;
  - a verdict is not one of kept, renamed, reshaped, merged, dropped;
  - a row that is not "dropped" has an empty v2 cell;
  - a v2 coordinate in backticks in the v2 cell does not exist in the proposed schema
    (`__typename` is allowed: it is how a client reads a type that replaced a kind field).

The table was written against the published schema whose SHA-256 is TABLE_BASELINE.
When the published schema has moved on, the output says so first: rows for the new
coordinates are then the missing ones.
"""
import hashlib
import os
import re
import sys

from graphql import parse
from graphql.language import (EnumTypeDefinitionNode, InputObjectTypeDefinitionNode, InterfaceTypeDefinitionNode,
                              ObjectTypeDefinitionNode, ScalarTypeDefinitionNode, UnionTypeDefinitionNode)

TABLE_BASELINE = '68688952aabb4d9c683d368ae19df556638f3cf531da3ba8c0f4f480e182e628'
TYPE_NODES = (ObjectTypeDefinitionNode, InterfaceTypeDefinitionNode, UnionTypeDefinitionNode,
              InputObjectTypeDefinitionNode, EnumTypeDefinitionNode, ScalarTypeDefinitionNode)
COORD = re.compile(r'^[A-Z][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*(\([A-Za-z_][A-Za-z0-9_]*:\))?)?$')
VERDICTS = {'kept', 'renamed', 'reshaped', 'merged', 'dropped'}
BUILTIN = {'String', 'Int', 'Float', 'Boolean', 'ID'}


def coordinates(path):
    """Every coordinate of an SDL file, as a set of strings."""
    out = set()
    with open(path, encoding='utf-8') as f:
        document = parse(f.read())
    for d in document.definitions:
        if not isinstance(d, TYPE_NODES):
            continue
        n = d.name.value
        out.add(n)
        if isinstance(d, (ObjectTypeDefinitionNode, InterfaceTypeDefinitionNode)):
            for f in d.fields:
                out.add(f'{n}.{f.name.value}')
                for a in f.arguments:
                    out.add(f'{n}.{f.name.value}({a.name.value}:)')
        elif isinstance(d, InputObjectTypeDefinitionNode):
            out.update(f'{n}.{f.name.value}' for f in d.fields)
        elif isinstance(d, EnumTypeDefinitionNode):
            out.update(f'{n}.{v.name.value}' for v in d.values)
        elif isinstance(d, UnionTypeDefinitionNode):
            out.update(f'{n}.{m.name.value}' for m in d.types)
    return out


def main(md, v1_path, v2_path):
    with open(v1_path, 'rb') as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    if digest != TABLE_BASELINE:
        print(f'note: the published schema ({digest[:12]}) is not the one this table was written '
              f'against ({TABLE_BASELINE[:12]}); coordinates added since then are reported as missing')
    v1 = coordinates(v1_path)
    v2 = coordinates(v2_path) | BUILTIN
    seen, dupes, unknown_v1, bad_v2, bad_verdict, empty, newer = set(), [], [], [], [], [], []
    with open(md, encoding='utf-8') as f:
        lines = list(enumerate(f, 1))
    for lineno, line in lines:
        if not line.startswith('| `'):
            continue
        cells = [c.strip() for c in re.split(r'(?<!\\)\|', line.strip())[1:-1]]
        if len(cells) != 4:
            bad_verdict.append((lineno, f'{len(cells)} cells'))
            continue
        c = cells[0].strip('`')
        if c not in v1:
            if 'newer than this repo' in cells[3]:
                newer.append((lineno, c))
            else:
                unknown_v1.append((lineno, c))
        elif c in seen:
            dupes.append((lineno, c))
        seen.add(c)
        if cells[2] not in VERDICTS:
            bad_verdict.append((lineno, cells[2]))
        if cells[2] != 'dropped' and cells[1] in ('', '—'):
            empty.append((lineno, c))
        for tok in re.findall(r'`([^`]+)`', cells[1]):
            if tok == '__typename':
                continue
            if COORD.match(tok) and tok not in v2:
                bad_v2.append((lineno, tok))
    missing = sorted(v1 - seen)
    for label, items in (('missing v1 coordinate', missing), ('not a v1 coordinate', unknown_v1),
                         ('listed twice', dupes), ('v2 coordinate does not exist', bad_v2),
                         ('bad verdict or row', bad_verdict), ('no v2 cell on a row that is not dropped', empty)):
        for it in items[:50]:
            print(f'FAIL {label}: {it}')
        if len(items) > 50:
            print(f'FAIL ... and {len(items) - 50} more {label}')
    fails = len(missing) + len(unknown_v1) + len(dupes) + len(bad_v2) + len(bad_verdict) + len(empty)
    for it in newer:
        print(f'note: row for a coordinate newer than the v1 given: {it}')
    print(f'{len(seen & v1)} of {len(v1)} v1 coordinates have a row; {fails} failures')
    return 1 if fails else 0


if __name__ == '__main__':
    here = os.path.dirname(os.path.abspath(__file__))
    args = sys.argv[1:] + [None] * 3
    sys.exit(main(args[0] or os.path.join(here, 'MIGRATION.md'),
                  args[1] or os.path.join(here, '..', '..', 'schema', 'leviath.graphql'),
                  args[2] or os.path.join(here, 'leviath.graphql')))
