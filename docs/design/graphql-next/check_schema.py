#!/usr/bin/env python3
"""Static checks for the proposed Leviath GraphQL schema (`leviath.graphql`).

    python3 check_schema.py [path] [--baseline PATH] [--list-breaking]

    path        the SDL to check (default: leviath.graphql next to this file)
    --baseline  the published schema to compare with, for the informational
                breaking-change report (default: ../../schema/leviath.graphql)
    --list-breaking
                print every breaking and dangerous change, not only the counts

Exits 1 when any check below finds something. The breaking-change report
never fails the run: the proposal is a deliberate breaking release, and the
report is there so a reviewer can see its size.

Checks (design-notes.md, "Design rules", says why each rule exists):

- the schema builds and passes `assert_valid_schema`;
- every type, field, argument, input field and enum value has a description;
- a name in backticks in a description resolves to a schema coordinate
  (`Type`, `Type.member`, `Type.field(arg)`, a field name used somewhere, or
  an ALL-CAPS enum value);
- filters mirror their outputs: every field of `XFilter` is a field of `X`,
  typed with the comparator for that field's type; `XListFilter` holds only
  `some`/`every`/`none`/`isNull`;
- the filter of a union or an interface has exactly one field per variant,
  named for it in lower camel case and typed with that variant's filter (a
  union's filter may add one arm per interface its members implement); a
  subscription frame selector has one Boolean field per selectable frame;
- listings: every `XConnection` has `results: [T!]!`, `cursor: Cursor` and
  `total: Int!`; every field that returns one takes `first: Int!` with a
  default and `after: Cursor`; `first` states its cap ("1 to N") in its
  description and its default is within it; no `edges`, `pageInfo`, `items`
  or `nextCursor` anywhere;
- mutations: exactly one argument, `request: VerbNounRequest!`, answered by
  `VerbNounResult!`;
- no orphan types: everything is reachable from a root.
"""
from __future__ import annotations

import argparse
import os
import re
import sys
from collections import Counter, defaultdict

from graphql import (
    GraphQLEnumType,
    GraphQLInputObjectType,
    GraphQLInterfaceType,
    GraphQLList,
    GraphQLNonNull,
    GraphQLObjectType,
    GraphQLScalarType,
    GraphQLUnionType,
    assert_valid_schema,
    build_schema,
    get_named_type,
    parse,
)
from graphql.language import (
    EnumTypeDefinitionNode,
    InputObjectTypeDefinitionNode,
    InterfaceTypeDefinitionNode,
    ListTypeNode,
    NonNullTypeNode,
    ObjectTypeDefinitionNode,
    ScalarTypeDefinitionNode,
    UnionTypeDefinitionNode,
)
from graphql.utilities import find_breaking_changes, find_dangerous_changes

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_TARGET = os.path.join(HERE, "leviath.graphql")
DEFAULT_BASELINE = os.path.normpath(os.path.join(HERE, "..", "..", "schema", "leviath.graphql"))

TYPE_DEF_NODES = (
    ObjectTypeDefinitionNode,
    InterfaceTypeDefinitionNode,
    UnionTypeDefinitionNode,
    InputObjectTypeDefinitionNode,
    EnumTypeDefinitionNode,
    ScalarTypeDefinitionNode,
)
FIELDED_NODES = (ObjectTypeDefinitionNode, InterfaceTypeDefinitionNode)
BUILTIN_SCALARS = {"String", "Int", "Float", "Boolean", "ID"}
COMBINATORS = {"and", "or", "not", "isNull"}


class Report:
    """Collects sections; a failing section with lines fails the run."""

    def __init__(self):
        self.sections = []
        self.failed = False

    def section(self, title, lines, fails):
        self.sections.append((title, lines, fails))
        if fails and lines:
            self.failed = True

    def render(self):
        out = []
        for title, lines, fails in self.sections:
            tag = "" if fails else " [informational]"
            out.append(f"== {title} ({len(lines)}){tag} ==")
            out.extend(f"  {line}" for line in lines) if lines else out.append("  (none)")
        return "\n".join(out)


def lower_camel(name):
    return name[:1].lower() + name[1:]


def type_string(node):
    if isinstance(node, NonNullTypeNode):
        return type_string(node.type) + "!"
    if isinstance(node, ListTypeNode):
        return "[" + type_string(node.type) + "]"
    return node.name.value


def named(node):
    while isinstance(node, (NonNullTypeNode, ListTypeNode)):
        node = node.type
    return node.name.value


def load(path):
    with open(path, "r", encoding="utf-8") as f:
        sdl = f.read()
    type_defs = {d.name.value: d for d in parse(sdl).definitions if isinstance(d, TYPE_DEF_NODES)}
    return sdl, type_defs


# --- descriptions ------------------------------------------------------------


def check_descriptions(type_defs):
    missing = []
    for name, defn in sorted(type_defs.items()):
        if defn.description is None:
            missing.append(name)
        if isinstance(defn, FIELDED_NODES):
            for f in defn.fields:
                if f.description is None:
                    missing.append(f"{name}.{f.name.value}")
                for a in f.arguments:
                    if a.description is None:
                        missing.append(f"{name}.{f.name.value}({a.name.value})")
        elif isinstance(defn, InputObjectTypeDefinitionNode):
            missing += [f"{name}.{f.name.value}" for f in defn.fields if f.description is None]
        elif isinstance(defn, EnumTypeDefinitionNode):
            missing += [f"{name}.{v.name.value}" for v in defn.values if v.description is None]
    return missing


# --- backtick references -----------------------------------------------------

TYPE_TOKEN_RE = re.compile(r"^[A-Z][A-Za-z0-9]+$")
CAMEL_TOKEN_RE = re.compile(r"^[a-z][A-Za-z0-9]*$")
COORD_RE = re.compile(r"^([A-Z][A-Za-z0-9]+)\.([a-zA-Z][A-Za-z0-9]*)(?:\(([a-zA-Z][A-Za-z0-9]*)\))?$")
SKIP_CHARS = ("/", "-", " ", "=", ":", "{", "$")
LITERALS = {"true", "false", "null"}  # values in code spans, not coordinates


def described(type_defs):
    """Yields (coordinate, description) for every described node."""
    for name, defn in type_defs.items():
        if defn.description is not None:
            yield name, defn.description.value
        members = []
        if isinstance(defn, FIELDED_NODES):
            for f in defn.fields:
                members.append((f"{name}.{f.name.value}", f.description))
                members += [(f"{name}.{f.name.value}({a.name.value})", a.description) for a in f.arguments]
        elif isinstance(defn, InputObjectTypeDefinitionNode):
            members = [(f"{name}.{f.name.value}", f.description) for f in defn.fields]
        elif isinstance(defn, EnumTypeDefinitionNode):
            members = [(f"{name}.{v.name.value}", v.description) for v in defn.values]
        for coord, desc in members:
            if desc is not None:
                yield coord, desc.value


def check_references(type_defs):
    enum_values, any_member, members_of = set(), set(), {}
    for name, defn in type_defs.items():
        if isinstance(defn, FIELDED_NODES):
            fields = {f.name.value: {a.name.value for a in f.arguments} for f in defn.fields}
            any_member |= set(fields) | {a for args in fields.values() for a in args}
            members_of[name] = ("fields", fields)
        elif isinstance(defn, InputObjectTypeDefinitionNode):
            fields = {f.name.value for f in defn.fields}
            any_member |= fields
            members_of[name] = ("input", fields)
        elif isinstance(defn, EnumTypeDefinitionNode):
            values = {v.name.value for v in defn.values}
            enum_values |= values
            members_of[name] = ("enum", values)
    failures = []
    for coord, text in described(type_defs):
        for tok in re.findall(r"`([^`]*)`", text):
            if tok in LITERALS or "--" in tok or any(c in tok for c in SKIP_CHARS):
                continue
            m = COORD_RE.match(tok)
            if m:
                tname, member, arg = m.groups()
                if tname not in type_defs:
                    failures.append(f"{coord}: `{tok}`: no type `{tname}`")
                    continue
                kind, members = members_of.get(tname, (None, {}))
                if member not in members:
                    failures.append(f"{coord}: `{tok}`: `{tname}` has no member `{member}`")
                elif arg is not None and (kind != "fields" or arg not in members[member]):
                    failures.append(f"{coord}: `{tok}`: `{tname}.{member}` has no argument `{arg}`")
                continue
            if TYPE_TOKEN_RE.match(tok):
                if tok in type_defs or tok in BUILTIN_SCALARS or (tok.isupper() and tok in enum_values):
                    continue
                failures.append(f"{coord}: `{tok}`: no such type or ALL-CAPS enum value")
                continue
            if CAMEL_TOKEN_RE.match(tok) and tok not in any_member:
                failures.append(f"{coord}: `{tok}`: no field, argument or input field has this name")
            # Anything else (config keys, file names, literals) is not a coordinate.
    return failures


# --- filters -------------------------------------------------------------------


def is_list(t):
    while isinstance(t, GraphQLNonNull):
        t = t.of_type
    return isinstance(t, GraphQLList)


def expected_comparators(output_type):
    """The filter type names that may filter an output field of this type."""
    inner = get_named_type(output_type)
    leaf = isinstance(inner, (GraphQLScalarType, GraphQLEnumType))
    if not is_list(output_type):
        return {inner.name + "Filter"}
    if leaf:
        return {inner.name + "ListFilter", inner.name + "Filter"}
    return {inner.name + "ListFilter"}


def variants_of(schema, t):
    if isinstance(t, GraphQLUnionType):
        return [m.name for m in t.types]
    impl = schema.get_implementations(t)
    return [o.name for o in impl.objects] + [i.name for i in impl.interfaces]


def check_filters(schema):
    failures, notes = [], []
    for name, ftype in sorted(schema.type_map.items()):
        if not isinstance(ftype, GraphQLInputObjectType) or not name.endswith("Filter"):
            continue
        if name.endswith("ListFilter"):
            base = schema.type_map.get(name[: -len("ListFilter")])
            element = schema.type_map.get(name[: -len("ListFilter")] + "Filter")
            if element is None or isinstance(base, (GraphQLScalarType, GraphQLEnumType)):
                continue  # a comparator for a list of scalars, such as StringListFilter
            for fname, field in ftype.fields.items():
                if fname == "isNull":
                    continue
                if fname not in {"some", "every", "none"} or get_named_type(field.type) is not element:
                    failures.append(f"{name}.{fname}: a list filter holds only some/every/none of `{element.name}`")
            continue
        target = schema.type_map.get(name[: -len("Filter")])
        if isinstance(target, GraphQLObjectType):
            for fname, field in ftype.fields.items():
                if fname in COMBINATORS:
                    continue
                if fname not in target.fields:
                    failures.append(f"{name}.{fname}: `{target.name}` has no field `{fname}`")
                    continue
                got = get_named_type(field.type).name
                allowed = expected_comparators(target.fields[fname].type)
                if got not in allowed:
                    failures.append(f"{name}.{fname}: is `{got}`, expected {sorted(allowed)}")
        elif isinstance(target, (GraphQLUnionType, GraphQLInterfaceType)):
            variants = variants_of(schema, target)
            arms = {lower_camel(v): v for v in variants}
            groups = {}
            if isinstance(target, GraphQLUnionType):
                for member in target.types:
                    for iface in member.interfaces:
                        groups[lower_camel(iface.name)] = iface.name
            selector = bool(ftype.fields) and all(
                get_named_type(f.type).name == "Boolean" for n, f in ftype.fields.items() if n not in COMBINATORS)
            for fname, field in ftype.fields.items():
                if fname in COMBINATORS:
                    continue
                got = get_named_type(field.type).name
                if isinstance(target, GraphQLInterfaceType) and fname in target.fields:
                    allowed = expected_comparators(target.fields[fname].type)
                    if got not in allowed:
                        failures.append(f"{name}.{fname}: is `{got}`, expected {sorted(allowed)}")
                elif fname in arms:
                    want = "Boolean" if selector else arms[fname] + "Filter"
                    if got != want:
                        failures.append(f"{name}.{fname}: the arm for `{arms[fname]}` must be `{want}`, is `{got}`")
                elif fname in groups and got == groups[fname] + "Filter":
                    continue
                else:
                    failures.append(f"{name}.{fname}: neither a field of `{target.name}` nor one of its variants")
            missing = sorted(v for v in variants if lower_camel(v) not in ftype.fields)
            if missing and selector:
                notes.append(f"{name}: frames with no selector, sent on every stream: {missing}")
            elif missing:
                failures.append(f"{name}: no arm for variant(s) {missing}")
    return failures, notes


# --- listings --------------------------------------------------------------------

CAP_RE = re.compile(r"\b1 to (\d+)\b")


def check_listings(type_defs):
    failures, extras = [], []
    connections = {n for n, d in type_defs.items()
                   if n.endswith("Connection") and isinstance(d, ObjectTypeDefinitionNode)}
    for name in sorted(connections):
        fields = {f.name.value: f for f in type_defs[name].fields}
        want = {"results": None, "cursor": "Cursor", "total": "Int!"}
        for fname, exact in want.items():
            f = fields.get(fname)
            if f is None:
                failures.append(f"{name}: missing `{fname}`")
            elif exact is None and not re.match(r"^\[[A-Za-z0-9]+!\]!$", type_string(f.type)):
                failures.append(f"{name}.results: expected `[T!]!`, got `{type_string(f.type)}`")
            elif exact is not None and type_string(f.type) != exact:
                failures.append(f"{name}.{fname}: expected `{exact}`, got `{type_string(f.type)}`")
        extra = sorted(set(fields) - set(want))
        if extra:
            extras.append(f"{name}: extra field(s) {extra}")
    for tname, defn in sorted(type_defs.items()):
        if not isinstance(defn, FIELDED_NODES):
            continue
        for f in defn.fields:
            coord = f"{tname}.{f.name.value}"
            if f.name.value in {"items", "nextCursor", "edges", "pageInfo"}:
                failures.append(f"{coord}: a paging field name from another listing shape")
            if named(f.type) not in connections:
                continue
            args = {a.name.value: a for a in f.arguments}
            first, after = args.get("first"), args.get("after")
            if first is None or type_string(first.type) != "Int!" or first.default_value is None:
                failures.append(f"{coord}: needs `first: Int!` with a default")
            else:
                m = CAP_RE.search(first.description.value if first.description else "")
                if not m:
                    failures.append(f"{coord}(first): the description does not state its cap as `1 to N`")
                elif int(first.default_value.value) > int(m.group(1)):
                    failures.append(f"{coord}(first): default {first.default_value.value} is over the cap {m.group(1)}")
            if after is None or type_string(after.type) != "Cursor":
                failures.append(f"{coord}: needs `after: Cursor`")
    return failures, extras


# --- mutations ---------------------------------------------------------------------


def check_mutations(type_defs):
    failures, bare = [], []
    mutation = type_defs.get("Mutation")
    if mutation is None:
        return ["no `Mutation` type"], bare
    for f in mutation.fields:
        verb_noun = f.name.value[:1].upper() + f.name.value[1:]
        if not f.arguments:
            bare.append(f.name.value)
            continue
        if len(f.arguments) != 1 or f.arguments[0].name.value != "request":
            failures.append(f"Mutation.{f.name.value}: takes one argument, `request`")
            continue
        if type_string(f.arguments[0].type) != f"{verb_noun}Request!":
            failures.append(f"Mutation.{f.name.value}(request): expected `{verb_noun}Request!`")
        if type_string(f.type) != f"{verb_noun}Result!":
            failures.append(f"Mutation.{f.name.value}: expected to return `{verb_noun}Result!`")
    return failures, bare


# --- orphans -------------------------------------------------------------------------


def check_orphans(schema):
    seen, queue = set(), [r for r in (schema.query_type, schema.mutation_type, schema.subscription_type) if r]
    while queue:
        t = queue.pop()
        if t.name in seen:
            continue
        seen.add(t.name)
        if isinstance(t, (GraphQLObjectType, GraphQLInterfaceType)):
            queue += list(t.interfaces)
            for field in t.fields.values():
                queue.append(get_named_type(field.type))
                queue += [get_named_type(a.type) for a in field.args.values()]
            if isinstance(t, GraphQLInterfaceType):
                queue += list(schema.get_possible_types(t))
        elif isinstance(t, GraphQLInputObjectType):
            queue += [get_named_type(f.type) for f in t.fields.values()]
        elif isinstance(t, GraphQLUnionType):
            queue += list(t.types)
    names = {n for n in schema.type_map if not n.startswith("__") and n not in BUILTIN_SCALARS}
    return sorted(names - seen)


# --- breaking changes (informational) -----------------------------------------------


def breaking_report(schema, baseline_path, list_all):
    if not os.path.exists(baseline_path):
        return [f"baseline not found: {baseline_path}"]
    with open(baseline_path, "r", encoding="utf-8") as f:
        baseline = build_schema(f.read())
    breaking = find_breaking_changes(baseline, schema)
    dangerous = find_dangerous_changes(baseline, schema)
    lines = [f"against {os.path.relpath(baseline_path, HERE)}: {len(breaking)} breaking, {len(dangerous)} dangerous"]
    for label, changes in (("breaking", breaking), ("dangerous", dangerous)):
        for kind, n in sorted(Counter(c.type.name for c in changes).items(), key=lambda kv: -kv[1]):
            lines.append(f"{label:9} {kind:40} {n}")
    if list_all:
        lines += [f"[{c.type.name}] {c.description}" for c in breaking + dangerous]
    return lines


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("target", nargs="?", default=DEFAULT_TARGET)
    parser.add_argument("--baseline", default=DEFAULT_BASELINE)
    parser.add_argument("--list-breaking", action="store_true")
    args = parser.parse_args()

    try:
        _sdl, type_defs = load(args.target)
        schema = build_schema(_sdl)
        assert_valid_schema(schema)
    except Exception as exc:  # noqa: BLE001 - report, don't crash
        print(f"FAIL: {args.target} does not build and validate: {exc}")
        return 1

    report = Report()
    user_types = [n for n in schema.type_map if not n.startswith("__") and n not in BUILTIN_SCALARS]
    print(f"Schema: {os.path.relpath(args.target, HERE)}: builds and validates, {len(user_types)} named types")
    report.section("Missing descriptions", check_descriptions(type_defs), fails=True)
    report.section("Backtick references that do not resolve", check_references(type_defs), fails=True)
    filter_failures, filter_notes = check_filters(schema)
    report.section("Filter mirror failures", filter_failures, fails=True)
    report.section("Filter notes", filter_notes, fails=False)
    listing_failures, extras = check_listings(type_defs)
    report.section("Listing shape failures", listing_failures, fails=True)
    report.section("Connections with a field beyond results/cursor/total", extras, fails=False)
    mutation_failures, bare = check_mutations(type_defs)
    report.section("Mutation grammar failures", mutation_failures, fails=True)
    report.section("Mutations with no argument", bare, fails=False)
    report.section("Orphan types", check_orphans(schema), fails=True)
    report.section("Breaking-change report", breaking_report(schema, args.baseline, args.list_breaking), fails=False)

    print(report.render())
    print("RESULT: FAIL" if report.failed else "RESULT: PASS")
    return 1 if report.failed else 0


if __name__ == "__main__":
    sys.exit(main())
