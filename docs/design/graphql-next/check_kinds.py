#!/usr/bin/env python3
"""Fail when the schema describes a kind of thing with a field or an enum
instead of with its types (design-notes.md, "Types, not `kind`").

Usage: python3 check_kinds.py [path/to/leviath.graphql]   (default: next to this file)

Checks:
  (a) an object or interface output field is named kind, type, category,
      variant, origin or mode;
  (b) an enum's name ends in Kind or Type;
  (c) an enum's value set matches the member names of a union, or the
      implementers of an interface (case-insensitive, underscores removed;
      also after stripping the members' shared prefix or suffix, so
      `{ANSWERED, FAILED}` matches `AnsweredAttempt | FailedAttempt`);
  (d) an object or interface has both a field returning a union or interface
      named ...State and an enum field named status.

Near misses for (c) (the enum's values cover every member plus at most one
extra, or every value names a member but at most one member is missing) are
printed as notes and do not fail the run: some are deliberate, and a reader
should see them.
"""
import os
import sys

from graphql import (GraphQLEnumType, GraphQLInterfaceType, GraphQLObjectType, GraphQLUnionType, build_schema,
                     get_named_type)

BANNED_FIELDS = {'kind', 'type', 'category', 'variant', 'origin', 'mode'}


def norm(s):
    return s.replace('_', '').lower()


def common_suffix(names):
    return os.path.commonprefix([n[::-1] for n in names])[::-1]


def member_sets(names):
    """The normalized member names, raw and with the shared prefix/suffix stripped."""
    ns = [norm(n) for n in names]
    out = [frozenset(ns)]
    if len(ns) >= 2:
        pre, suf = os.path.commonprefix(ns), common_suffix(ns)
        for strip_pre, strip_suf in ((pre, ''), ('', suf), (pre, suf)):
            stripped = [n[len(strip_pre):len(n) - len(strip_suf) if strip_suf else len(n)] for n in ns]
            if all(stripped):
                out.append(frozenset(stripped))
    return out


def groups(schema):
    """(label, member names) for every union and every interface."""
    for t in schema.type_map.values():
        if t.name.startswith('__'):
            continue
        if isinstance(t, GraphQLUnionType):
            yield f'union {t.name}', [m.name for m in t.types]
        elif isinstance(t, GraphQLInterfaceType):
            impls = schema.get_implementations(t)
            names = [o.name for o in impls.objects] + [i.name for i in impls.interfaces]
            if names:
                yield f'interface {t.name}', names


def main(path):
    with open(path, encoding='utf-8') as f:
        schema = build_schema(f.read())
    fails, notes = [], []
    types = [t for n, t in sorted(schema.type_map.items()) if not n.startswith('__')]

    for t in types:
        if isinstance(t, (GraphQLObjectType, GraphQLInterfaceType)):
            for fname in t.fields:
                if fname in BANNED_FIELDS:
                    fails.append(f'(a) {t.name}.{fname}: a field named `{fname}`; make the kinds types')
        if isinstance(t, GraphQLEnumType) and (t.name.endswith('Kind') or t.name.endswith('Type')):
            fails.append(f'(b) enum {t.name}: named like a kind; make the kinds types')

    group_list = list(groups(schema))
    for t in types:
        if not isinstance(t, GraphQLEnumType):
            continue
        values = frozenset(norm(v) for v in t.values)
        for label, names in group_list:
            sets = member_sets(names)
            if values in sets:
                fails.append(f'(c) enum {t.name} repeats the members of {label}: {sorted(names)}')
                continue
            for ms in sets:
                extra, missing = values - ms, ms - values
                if (not missing and len(extra) == 1) or (not extra and len(missing) == 1 and len(ms) > 2):
                    notes.append(f'(c?) enum {t.name} nearly repeats {label}: '
                                 f'extra {sorted(extra) or "none"}, missing {sorted(missing) or "none"}')
                    break

    for t in types:
        if not isinstance(t, (GraphQLObjectType, GraphQLInterfaceType)):
            continue
        state_fields = [n for n, f in t.fields.items()
                        if isinstance(get_named_type(f.type), (GraphQLUnionType, GraphQLInterfaceType))
                        and get_named_type(f.type).name.endswith('State')]
        status = t.fields.get('status')
        if state_fields and status is not None and isinstance(get_named_type(status.type), GraphQLEnumType):
            fails.append(f'(d) {t.name}: a state union ({", ".join(state_fields)}) beside an enum `status`')

    for n in notes:
        print('note', n)
    for f in fails:
        print('FAIL', f)
    print(f'{len(fails)} failures, {len(notes)} notes ({os.path.basename(path)})')
    return 1 if fails else 0


if __name__ == '__main__':
    here = os.path.dirname(os.path.abspath(__file__))
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, 'leviath.graphql')))
