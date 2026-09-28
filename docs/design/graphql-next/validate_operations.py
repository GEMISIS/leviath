#!/usr/bin/env python3
"""Validate the operation corpus in operations/ against the proposed schema.

    python3 validate_operations.py [--schema leviath.graphql] [--operations operations]
                                   [--variables variables.json]

Fails when:
  - a file in operations/*.graphql does not parse, or an operation in it is
    unnamed, has no `#` comment on the line above saying what journey it serves,
    or does not validate against the schema (every rule graphql-core implements);
  - two operations share a name;
  - variables.json lacks a key for an operation, or has a key for no operation.
    A key holds one example (an object) or several (a list of objects);
  - an example does not coerce against the operation's variable definitions;
  - `@oneOf` is not honoured. graphql-core 3.2.6 does not enforce it, so this
    script does, in every coerced example and every literal in an operation: an
    input object marked `@oneOf` has exactly one field, that field is not null,
    and a variable used as that field has a non-null type;
  - a page size is over its cap: a literal `first`, a variable's default, or an
    example value passed as `first` is larger than the "1 to N" its argument's
    description states (the server refuses it rather than cutting it down).

A self-test runs first and must catch a set of known-bad inputs, so a check
that silently stopped working fails the run instead of passing it.

Nothing here executes an operation. Examples use placeholder ids and are inert.
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import re
import sys

from graphql import (
    GraphQLInputObjectType,
    GraphQLList,
    GraphQLNonNull,
    GraphQLSchema,
    OperationDefinitionNode,
    TypeInfo,
    TypeInfoVisitor,
    Visitor,
    assert_valid_schema,
    build_schema,
    get_named_type,
    parse,
    validate,
    visit,
)
from graphql.execution.values import get_variable_values
from graphql.language import IntValueNode, NullValueNode, VariableNode

HERE = os.path.dirname(os.path.abspath(__file__))
CAP_RE = re.compile(r"\b1 to (\d+)\b")


def is_one_of(t) -> bool:
    node = getattr(t, "ast_node", None)
    return bool(node and any(d.name.value == "oneOf" for d in node.directives or []))


def unwrap(t):
    while isinstance(t, GraphQLNonNull):
        t = t.of_type
    return t


def check_value(value, gql_type, path, problems):
    """Walks a coerced variable value and reports @oneOf violations."""
    t = unwrap(gql_type)
    if value is None:
        return
    if isinstance(t, GraphQLList):
        items = value if isinstance(value, list) else [value]
        for i, item in enumerate(items):
            check_value(item, t.of_type, f"{path}[{i}]", problems)
        return
    if isinstance(t, GraphQLInputObjectType) and isinstance(value, dict):
        if is_one_of(t):
            given = [k for k in value]
            if len(given) != 1:
                problems.append(f"{path}: `{t.name}` is @oneOf; exactly one field, got {given}")
            elif value[given[0]] is None:
                problems.append(f"{path}.{given[0]}: `{t.name}` is @oneOf; the field may not be null")
        for k, v in value.items():
            if k in t.fields:
                check_value(v, t.fields[k].type, f"{path}.{k}", problems)


class LiteralChecker(Visitor):
    """Checks @oneOf literals and literal page sizes; records variables passed as `first`."""

    def __init__(self, type_info, variable_types, problems, first_vars):
        super().__init__()
        self.type_info = type_info
        self.variable_types = variable_types
        self.problems = problems
        self.first_vars = first_vars

    def enter_object_value(self, node, *_):
        t = get_named_type(self.type_info.get_input_type())
        if not (isinstance(t, GraphQLInputObjectType) and is_one_of(t)):
            return
        if len(node.fields) != 1:
            self.problems.append(f"literal `{t.name}` is @oneOf; exactly one field, got "
                                 f"{[f.name.value for f in node.fields]}")
            return
        value = node.fields[0].value
        if isinstance(value, NullValueNode):
            self.problems.append(f"literal `{t.name}.{node.fields[0].name.value}`: @oneOf field may not be null")
        elif isinstance(value, VariableNode):
            var_type = self.variable_types.get(value.name.value)
            if var_type is not None and not isinstance(var_type, GraphQLNonNull):
                self.problems.append(f"`${value.name.value}` fills @oneOf field `{t.name}.{node.fields[0].name.value}`"
                                     f" and must be non-null")

    def enter_argument(self, node, *_):
        arg = self.type_info.get_argument()
        if arg is None or node.name.value != "first":
            return
        m = CAP_RE.search(arg.description or "")
        if not m:
            return
        cap = int(m.group(1))
        if isinstance(node.value, IntValueNode) and int(node.value.value) > cap:
            self.problems.append(f"`first: {node.value.value}` is over this listing's cap of {cap}")
        elif isinstance(node.value, VariableNode):
            self.first_vars.append((node.value.name.value, cap))


def variable_types(schema, op):
    from graphql.utilities import type_from_ast
    return {v.variable.name.value: type_from_ast(schema, v.type) for v in op.variable_definitions}


def check_operation(schema, op, examples):
    """Problems for one operation and its examples."""
    problems, first_vars = [], []
    types = variable_types(schema, op)
    type_info = TypeInfo(schema)
    visit(op, TypeInfoVisitor(type_info, LiteralChecker(type_info, types, problems, first_vars)))
    for var, cap in first_vars:
        default = next((v.default_value for v in op.variable_definitions if v.variable.name.value == var), None)
        if isinstance(default, IntValueNode) and int(default.value) > cap:
            problems.append(f"`${var}` defaults to {default.value}, over the cap of {cap}")
    for i, example in enumerate(examples):
        label = f"example {i + 1}"
        if not isinstance(example, dict):
            problems.append(f"{label}: not an object")
            continue
        coerced = get_variable_values(schema, op.variable_definitions, example)
        if isinstance(coerced, list):
            problems += [f"{label}: {e.message}" for e in coerced]
            continue
        for name, value in coerced.items():
            check_value(value, types[name], f"{label}: ${name}", problems)
        for var, cap in first_vars:
            if isinstance(coerced.get(var), int) and coerced[var] > cap:
                problems.append(f"{label}: ${var} = {coerced[var]} is over the cap of {cap}")
    return problems


def self_test(schema: GraphQLSchema):
    """Known-bad inputs the checks above must catch, and one good one they must pass."""
    cases = [
        ("two @oneOf fields in a literal", True,
         'query A($c: String!) { validateExtension(draft: { tool: $c, provider: $c }) { valid } }', [{"c": "x"}]),
        ("nullable variable in a @oneOf literal", True,
         'query A($c: String) { validateExtension(draft: { tool: $c }) { valid } }', [{"c": "x"}]),
        ("two @oneOf fields in a variable", True,
         'mutation A($r: AnswerInteractionRequest!) { answerInteraction(request: $r) { alreadyInState } }',
         [{"r": {"interactionId": "i", "answer": {"text": "a", "choice": 1}}}]),
        ("null @oneOf field in a variable", True,
         'mutation A($r: AnswerInteractionRequest!) { answerInteraction(request: $r) { alreadyInState } }',
         [{"r": {"interactionId": "i", "answer": {"text": None}}}]),
        ("literal page size over the cap", True, 'query A { runs(first: 5000) { total } }', [{}]),
        ("example page size over the cap", True,
         'query A($n: Int!) { runs(first: $n) { total } }', [{"n": 5000}]),
        ("an example that does not coerce", True,
         'query A($n: Int!) { runs(first: $n) { total } }', [{"n": "ten"}]),
        ("a correct @oneOf answer", False,
         'mutation A($r: AnswerInteractionRequest!) { answerInteraction(request: $r) { alreadyInState } }',
         [{"r": {"interactionId": "i", "answer": {"approve": {"scope": "ONCE"}}}}]),
    ]
    failures = []
    for label, should_fail, text, examples in cases:
        doc = parse(text)
        errors = validate(schema, doc)
        op = next(d for d in doc.definitions if isinstance(d, OperationDefinitionNode))
        problems = [e.message for e in errors] + check_operation(schema, op, examples)
        if bool(problems) != should_fail:
            failures.append(f"self-test '{label}': expected {'a problem' if should_fail else 'none'}, got {problems}")
    return failures, len(cases)


def root_coverage(schema, docs):
    used = set()
    for doc in docs:
        for op in doc.definitions:
            if isinstance(op, OperationDefinitionNode):
                root = {"query": schema.query_type, "mutation": schema.mutation_type,
                        "subscription": schema.subscription_type}[op.operation.value]
                for sel in op.selection_set.selections:
                    if hasattr(sel, "name"):
                        used.add(f"{root.name}.{sel.name.value}")
    roots = [r for r in (schema.query_type, schema.mutation_type, schema.subscription_type) if r]
    every = {f"{r.name}.{f}" for r in roots for f in r.fields}
    return sorted(used & every), sorted(every - used)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--schema", default=os.path.join(HERE, "leviath.graphql"))
    parser.add_argument("--operations", default=os.path.join(HERE, "operations"))
    parser.add_argument("--variables", default=os.path.join(HERE, "variables.json"))
    args = parser.parse_args()

    with open(args.schema, encoding="utf-8") as f:
        schema = build_schema(f.read())
    assert_valid_schema(schema)

    failures, tests = self_test(schema)
    print(f"self-test: {tests - len(failures)} of {tests} cases behave as expected")

    with open(args.variables, encoding="utf-8") as f:
        variables = json.load(f)

    ops, docs, counts = {}, [], {}
    for path in sorted(glob.glob(os.path.join(args.operations, "*.graphql"))):
        name = os.path.basename(path)
        with open(path, encoding="utf-8") as f:
            text = f.read()
        try:
            doc = parse(text)
        except Exception as exc:  # noqa: BLE001
            failures.append(f"{name}: does not parse: {exc}")
            continue
        docs.append(doc)
        lines = text.splitlines()
        for error in validate(schema, doc):
            failures.append(f"{name}: {error.message}")
        n = 0
        for op in doc.definitions:
            if not isinstance(op, OperationDefinitionNode):
                continue
            n += 1
            if op.name is None:
                failures.append(f"{name}: an operation has no name")
                continue
            op_name = op.name.value
            line = op.loc.start_token.line
            if line < 2 or not lines[line - 2].lstrip().startswith("#"):
                failures.append(f"{name}: {op_name} has no journey comment on the line above it")
            if op_name in ops:
                failures.append(f"{name}: {op_name} is also defined in {ops[op_name][0]}")
                continue
            ops[op_name] = (name, op)
        counts[name] = n

    examples_total = 0
    for op_name, (file_name, op) in sorted(ops.items()):
        if op_name not in variables:
            failures.append(f"variables.json: no example for {op_name}")
            continue
        examples = variables[op_name] if isinstance(variables[op_name], list) else [variables[op_name]]
        if not examples:
            failures.append(f"variables.json: {op_name} has an empty example list")
        examples_total += len(examples)
        failures += [f"{file_name}: {op_name}: {p}" for p in check_operation(schema, op, examples)]
    for key in sorted(set(variables) - set(ops)):
        failures.append(f"variables.json: {key} names no operation")

    kinds = {}
    for _file, op in ops.values():
        kinds[op.operation.value] = kinds.get(op.operation.value, 0) + 1
    for name, n in counts.items():
        print(f"{name:32} {n:3} operations")
    print(f"{len(ops)} operations ({', '.join(f'{v} {k}' for k, v in sorted(kinds.items()))}), "
          f"{examples_total} examples")
    used, unused = root_coverage(schema, docs)
    print(f"root fields used by an operation: {len(used)} of {len(used) + len(unused)} "
          f"[informational; unused: {', '.join(u.split('.', 1)[1] for u in unused) or 'none'}]")
    for f in failures:
        print("FAIL", f)
    print("RESULT: FAIL" if failures else "RESULT: PASS")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
