//! The documented examples, walked field by field against the served schema.
//!
//! `docs/schema/leviath.graphql` cannot disagree with this module: it is
//! generated from these Rust types and a test compares the two byte for byte.
//! The examples in the prose have no such tie. A field renamed here leaves
//! every query in `docs/content/graphql.md` quietly wrong until a reader sends
//! one and gets an error back, so this walks all of them on every run.
//!
//! Nothing here executes a query. Resolving one wants a daemon on the other end,
//! and the answer would say nothing extra about whether the query was well
//! formed. So the check is the walk the spec itself describes: start at the root
//! type for the operation, look each selected field up on the type that carries
//! it, and descend into whatever that field returns.

use std::collections::{HashMap, HashSet};
use std::fmt;

use async_graphql::parser::types::{
    BaseType, ExecutableDocument, Field, FieldDefinition, FragmentSpread, InputValueDefinition,
    OperationType, Selection, SelectionSet, TypeKind, TypeSystemDefinition,
};
use async_graphql::parser::{Pos, parse_query, parse_schema};
use async_graphql::{Positioned, Value};

use super::sdl;

/// A documentation page and the examples cut out of it.
struct Page {
    /// Repository-relative, because that is what a failure has to print for the
    /// line number beside it to be worth anything.
    path: &'static str,
    text: &'static str,
}

/// The pages that carry GraphQL examples.
const PAGES: &[Page] = &[
    Page {
        path: "docs/content/graphql.md",
        text: include_str!("../../../../../../docs/content/graphql.md"),
    },
    Page {
        path: "docs/content/api.md",
        text: include_str!("../../../../../../docs/content/api.md"),
    },
];

/// The fewest examples this check is willing to call a run.
///
/// An extractor that stops finding blocks is how a test like this rots: it keeps
/// passing, over nothing. The number sits under what the pages carry today, so
/// that adding an example never fails the build, and far enough over zero that
/// losing the examples does.
const FEWEST_EXAMPLES: usize = 26;

/// The fewest queries this check expects to find inside a request body.
///
/// The `curl` line under Auth is the first example a reader copies, and it is a
/// query in a shell string rather than in a fence of its own. Rewriting it into
/// a shape the payload reader no longer knows has to fail the build, or the
/// worst example on the page becomes the one nothing checks.
const FEWEST_EMBEDDED: usize = 1;

/// One fenced block, with where it sits in its page.
struct Block {
    /// The word after the opening fence: `graphql`, `bash`, `json`, or nothing.
    language: String,
    /// Line of the opening fence, counting from one, so the line a failure
    /// prints is the line an editor jumps to.
    fence_line: usize,
    body: String,
}

/// Every fenced block of a page, in order, whatever its language.
///
/// Fences are tracked by opening and closing rather than by what they say, so a
/// `graphql` line inside a JSON block is body text and not the start of an
/// example.
fn fenced_blocks(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut open: Option<(String, usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        let fence = line.trim_start().starts_with("```");
        match &mut open {
            None => {
                if fence {
                    let language = line.trim().trim_start_matches('`').trim().to_string();
                    open = Some((language, index + 1, String::new()));
                }
            }
            Some((language, fence_line, body)) => {
                if fence {
                    blocks.push(Block {
                        language: std::mem::take(language),
                        fence_line: *fence_line,
                        body: std::mem::take(body),
                    });
                    open = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    assert!(
        open.is_none(),
        "a fence is never closed, so the rest of the page reads as code"
    );
    blocks
}

/// A query that rides inside a request body rather than a `graphql` fence.
struct Embedded {
    /// The line of the page the body sits on.
    line: usize,
    /// The query, with its JSON escaping undone.
    document: String,
}

/// Every query carried by a request body in any fence of a page.
///
/// A request body is JSON wherever it turns up, so this looks for the `"query"`
/// key and not for the language of the fence around it: the one on this page is
/// a `curl` argument in a shell string.
fn embedded_queries(blocks: &[Block]) -> Vec<Embedded> {
    let mut found = Vec::new();
    for block in blocks {
        for (index, line) in block.body.lines().enumerate() {
            if let Some(document) = query_value(line) {
                found.push(Embedded {
                    line: block.fence_line + index + 1,
                    document,
                });
            }
        }
    }
    found
}

/// The query a line's `"query"` key is set to, unescaped.
///
/// Scanned by hand rather than parsed as a document, because the line this has to
/// read is a shell command with the JSON quoted inside it. A body whose string
/// runs past the end of the line is not recognised, which is what the count of
/// payloads found is there to catch.
fn query_value(line: &str) -> Option<String> {
    let (_, rest) = line.split_once("\"query\"")?;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let mut quoted = String::from('"');
    let mut chars = rest.strip_prefix('"')?.chars();
    loop {
        let next = chars.next()?;
        quoted.push(next);
        // A backslash takes the character after it with it, so an escaped quote
        // does not read as the end of the string.
        if next == '\\' {
            quoted.push(chars.next()?);
        } else if next == '"' {
            break;
        }
    }
    Some(
        serde_json::from_str(&quoted)
            .unwrap_or_else(|error| panic!("a request body is not JSON: {line}: {error}")),
    )
}

/// What kind of type this is, as far as a walk cares.
#[derive(PartialEq, Eq)]
enum Kind {
    /// A scalar or an enum: the end of a selection, with nothing to look inside.
    Leaf,
    /// An object, an interface or a union: a selection set belongs here.
    Composite,
    /// An input object: the shape an argument value is checked against.
    Input,
}

/// One type of the schema, reduced to what a walk asks of it.
struct Shape {
    kind: Kind,
    /// Field name to what that field offers. Empty for a leaf, and for a union,
    /// which is why selecting a field on either fails.
    fields: HashMap<String, FieldShape>,
    /// The types a fragment may name while standing on this one.
    ///
    /// Held in both directions: an object lists the interfaces it implements and
    /// the unions it belongs to, and each of those lists the object. Narrowing
    /// (`... on ShellCall` inside a `ToolCall`) and widening (`... on ToolCall`
    /// inside a `ShellCall`) are both legal, and one set holding both keeps this
    /// from refusing an example the server would answer.
    covers: HashSet<String>,
}

/// One field, or one input field, reduced the same way.
struct FieldShape {
    /// Argument name to the name of that argument's type, so an input object
    /// nested inside a value is checked against the same table of types.
    arguments: HashMap<String, String>,
    /// The field's type with the `!` and the `[]` taken off. What a selection set
    /// or an argument value is checked against.
    named_type: String,
}

/// The schema an example is checked against.
struct Surface {
    types: HashMap<String, Shape>,
    query: Option<String>,
    mutation: Option<String>,
    subscription: Option<String>,
}

/// Where a walk stopped, and where in the example that was.
struct Fault {
    pos: Pos,
    message: String,
}

impl Fault {
    fn at(pos: Pos, message: String) -> Self {
        Self { pos, message }
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.pos.line, self.pos.column, self.message)
    }
}

/// The name left after the `!` and the `[]` wrappers come off a type.
fn named_type(ty: &BaseType) -> String {
    match ty {
        BaseType::Named(name) => name.to_string(),
        BaseType::List(inner) => named_type(&inner.base),
    }
}

/// The fields of an object or an interface, with their arguments.
fn output_fields(fields: &[Positioned<FieldDefinition>]) -> HashMap<String, FieldShape> {
    fields
        .iter()
        .map(|field| {
            let arguments = field
                .node
                .arguments
                .iter()
                .map(|argument| {
                    (
                        argument.node.name.node.to_string(),
                        named_type(&argument.node.ty.node.base),
                    )
                })
                .collect();
            (
                field.node.name.node.to_string(),
                FieldShape {
                    arguments,
                    named_type: named_type(&field.node.ty.node.base),
                },
            )
        })
        .collect()
}

/// The fields of an input object. An input field takes no arguments of its own.
fn input_fields(fields: &[Positioned<InputValueDefinition>]) -> HashMap<String, FieldShape> {
    fields
        .iter()
        .map(|field| {
            (
                field.node.name.node.to_string(),
                FieldShape {
                    arguments: HashMap::new(),
                    named_type: named_type(&field.node.ty.node.base),
                },
            )
        })
        .collect()
}

/// One root type, with the spec's naming convention standing in where an SDL
/// document leaves that root implicit.
///
/// The published schema names all three outright. A schema written by hand for a
/// test is allowed to leave them out, and reading it the way a client would keeps
/// the walk under test the same one either way.
fn root(named: Option<String>, convention: &str, types: &HashMap<String, Shape>) -> Option<String> {
    named.or_else(|| {
        types
            .contains_key(convention)
            .then(|| convention.to_string())
    })
}

impl Surface {
    /// Read an SDL document into the tables a walk needs.
    fn parse(document: &str) -> Self {
        let parsed = parse_schema(document).expect("the schema parses");
        let mut types: HashMap<String, Shape> = HashMap::new();
        // The built-in scalars are not written in an SDL file, and a walk still
        // has to know that `id { anything }` is a mistake.
        for scalar in ["String", "Int", "Float", "Boolean", "ID"] {
            types.insert(
                scalar.to_string(),
                Shape {
                    kind: Kind::Leaf,
                    fields: HashMap::new(),
                    covers: HashSet::new(),
                },
            );
        }
        let (mut query, mut mutation, mut subscription) = (None, None, None);
        // Which type belongs inside which, collected as we go and joined up
        // afterwards, because a member can be defined before its union is.
        let mut belongs: Vec<(String, String)> = Vec::new();
        for definition in &parsed.definitions {
            match definition {
                TypeSystemDefinition::Schema(schema) => {
                    let roots = &schema.node;
                    query = roots.query.as_ref().map(|root| root.node.to_string());
                    mutation = roots.mutation.as_ref().map(|root| root.node.to_string());
                    subscription = roots
                        .subscription
                        .as_ref()
                        .map(|root| root.node.to_string());
                }
                TypeSystemDefinition::Type(ty) => {
                    let name = ty.node.name.node.to_string();
                    let (kind, fields) = match &ty.node.kind {
                        TypeKind::Object(object) => {
                            for interface in &object.implements {
                                belongs.push((name.clone(), interface.node.to_string()));
                            }
                            (Kind::Composite, output_fields(&object.fields))
                        }
                        TypeKind::Interface(interface) => {
                            for outer in &interface.implements {
                                belongs.push((name.clone(), outer.node.to_string()));
                            }
                            (Kind::Composite, output_fields(&interface.fields))
                        }
                        TypeKind::Union(union) => {
                            for member in &union.members {
                                belongs.push((member.node.to_string(), name.clone()));
                            }
                            (Kind::Composite, HashMap::new())
                        }
                        TypeKind::InputObject(input) => (Kind::Input, input_fields(&input.fields)),
                        TypeKind::Scalar | TypeKind::Enum(_) => (Kind::Leaf, HashMap::new()),
                    };
                    let covers = HashSet::from([name.clone()]);
                    types.insert(
                        name,
                        Shape {
                            kind,
                            fields,
                            covers,
                        },
                    );
                }
                TypeSystemDefinition::Directive(_) => {}
            }
        }
        for (inner, outer) in belongs {
            if let Some(shape) = types.get_mut(&outer) {
                shape.covers.insert(inner.clone());
            }
            if let Some(shape) = types.get_mut(&inner) {
                shape.covers.insert(outer);
            }
        }
        Self {
            query: root(query, "Query", &types),
            mutation: root(mutation, "Mutation", &types),
            subscription: root(subscription, "Subscription", &types),
            types,
        }
    }

    /// The shape of a named type, or a failure naming where it was wanted.
    fn shape(&self, name: &str, pos: Pos, path: &str) -> Result<&Shape, Fault> {
        self.types.get(name).ok_or_else(|| {
            Fault::at(
                pos,
                format!("no type named `{name}` in the schema (at {path})"),
            )
        })
    }

    /// Check one example: parse it, then walk every operation it holds.
    fn check(&self, example: &str) -> Result<(), Fault> {
        let document = parse_query(example).map_err(|error| {
            Fault::at(
                error.positions().next().unwrap_or_default(),
                error.to_string(),
            )
        })?;
        for (_, operation) in document.operations.iter() {
            let ty = operation.node.ty;
            let root = match ty {
                OperationType::Query => self.query.as_deref(),
                OperationType::Mutation => self.mutation.as_deref(),
                OperationType::Subscription => self.subscription.as_deref(),
            };
            let root = root
                .ok_or_else(|| Fault::at(operation.pos, format!("the schema has no {ty} root")))?;
            let at = Spot {
                shape: self.shape(root, operation.pos, root)?,
                ty: root.to_string(),
                path: root.to_string(),
            };
            let mut walk = Walk {
                surface: self,
                document: &document,
                spreading: Vec::new(),
            };
            walk.selection_set(&at, &operation.node.selection_set.node)?;
        }
        Ok(())
    }
}

/// Where a walk currently stands: a type, and the route taken to reach it.
///
/// The route is what makes a failure readable. A field five levels inside a
/// connection is otherwise just a name.
struct Spot<'a> {
    shape: &'a Shape,
    ty: String,
    path: String,
}

/// One walk of one example.
struct Walk<'a> {
    surface: &'a Surface,
    document: &'a ExecutableDocument,
    /// The fragments being expanded right now, so a fragment that reaches itself
    /// is a failure rather than a hang.
    spreading: Vec<String>,
}

impl Walk<'_> {
    /// Walk one selection set against the type it is selected on.
    fn selection_set(&mut self, at: &Spot<'_>, set: &SelectionSet) -> Result<(), Fault> {
        for item in &set.items {
            match &item.node {
                Selection::Field(field) => self.field(at, field)?,
                Selection::InlineFragment(fragment) => {
                    let inner = &fragment.node.selection_set.node;
                    match &fragment.node.type_condition {
                        Some(condition) => {
                            self.narrow(at, &condition.node.on.node, condition.pos, inner)?;
                        }
                        // A fragment with no condition stays on the same type.
                        // It is there to carry a directive, and its fields are
                        // selected exactly where the fragment sits.
                        None => self.selection_set(at, inner)?,
                    }
                }
                Selection::FragmentSpread(spread) => self.spread(at, spread)?,
            }
        }
        Ok(())
    }

    /// Walk one selected field: its arguments, then whatever it returns.
    fn field(&mut self, at: &Spot<'_>, field: &Positioned<Field>) -> Result<(), Fault> {
        let name = field.node.name.node.to_string();
        // Every type answers `__typename`, and no type declares it.
        if name == "__typename" {
            return Ok(());
        }
        let Spot { shape, ty, path } = at;
        let definition = shape.fields.get(name.as_str()).ok_or_else(|| {
            Fault::at(
                field.pos,
                format!("`{ty}` has no field `{name}` (at {path})"),
            )
        })?;
        let here = format!("{path}.{name}");
        for (argument, value) in &field.node.arguments {
            let given = argument.node.to_string();
            let argument_type = definition.arguments.get(given.as_str()).ok_or_else(|| {
                Fault::at(
                    argument.pos,
                    format!("`{ty}.{name}` takes no argument `{given}` (at {here})"),
                )
            })?;
            // A variable stands in for a value the example never shows, and the
            // names inside a value are all this checks, so every variable
            // becomes a null and the shape around it survives.
            let constant = value
                .node
                .clone()
                .into_const_with(|_| Ok::<_, ()>(Value::Null))
                .unwrap_or_default();
            self.surface.walk_value(
                argument_type,
                &constant,
                &format!("{here}({given}:)"),
                value.pos,
            )?;
        }
        let selection = &field.node.selection_set.node;
        let returns = &definition.named_type;
        let target = self.surface.shape(returns, field.pos, &here)?;
        match (target.kind == Kind::Composite, selection.items.is_empty()) {
            (true, true) => Err(Fault::at(
                field.pos,
                format!(
                    "`{ty}.{name}` returns `{returns}`, so it needs a selection set (at {here})"
                ),
            )),
            (false, false) => Err(Fault::at(
                field.pos,
                format!(
                    "`{ty}.{name}` returns `{returns}`, which has nothing to select inside (at {here})"
                ),
            )),
            (true, false) => {
                let inner = Spot {
                    shape: target,
                    ty: returns.clone(),
                    path: here,
                };
                self.selection_set(&inner, selection)
            }
            (false, true) => Ok(()),
        }
    }

    /// Expand one named fragment where it is spread.
    fn spread(&mut self, at: &Spot<'_>, spread: &Positioned<FragmentSpread>) -> Result<(), Fault> {
        let name = spread.node.fragment_name.node.to_string();
        let path = &at.path;
        // The document outlives this walk, so the fragment is read through a
        // copy of that borrow rather than through `self`, which the walk below
        // needs mutably.
        let document = self.document;
        let fragment = document.fragments.get(name.as_str()).ok_or_else(|| {
            Fault::at(
                spread.pos,
                format!("the example defines no fragment `{name}` (at {path})"),
            )
        })?;
        if self.spreading.contains(&name) {
            return Err(Fault::at(
                spread.pos,
                format!("fragment `{name}` spreads itself (at {path})"),
            ));
        }
        self.spreading.push(name);
        let result = self.narrow(
            at,
            &fragment.node.type_condition.node.on.node,
            spread.pos,
            &fragment.node.selection_set.node,
        );
        self.spreading.pop();
        result
    }

    /// Walk a selection set taken on a different type than the one around it.
    ///
    /// Shared by `... on X` and by a named fragment's `on X`, which differ only
    /// in where the selection set was written.
    fn narrow(
        &mut self,
        at: &Spot<'_>,
        condition: &str,
        pos: Pos,
        set: &SelectionSet,
    ) -> Result<(), Fault> {
        let shape = self.surface.shape(condition, pos, &at.path)?;
        if !at.shape.covers.contains(condition) {
            return Err(Fault::at(
                pos,
                format!("`{condition}` is not a `{}` (at {})", at.ty, at.path),
            ));
        }
        let inner = Spot {
            shape,
            ty: condition.to_string(),
            path: format!("{} ... on {condition}", at.path),
        };
        self.selection_set(&inner, set)
    }
}

impl Surface {
    /// Walk one argument value against the type it is given for.
    ///
    /// Only names are checked, and only where the schema says a name is what
    /// comes next: a `JSON` argument holds an object whose keys are the caller's
    /// own business, so a value is opened up only when its type is an input
    /// object.
    fn walk_value(&self, ty: &str, value: &Value, path: &str, pos: Pos) -> Result<(), Fault> {
        match value {
            Value::List(items) => {
                for item in items {
                    self.walk_value(ty, item, path, pos)?;
                }
                Ok(())
            }
            Value::Object(given) => {
                let shape = self.shape(ty, pos, path)?;
                if shape.kind != Kind::Input {
                    return Ok(());
                }
                for (name, inner) in given {
                    let field = shape.fields.get(name.as_str()).ok_or_else(|| {
                        Fault::at(
                            pos,
                            format!("`{ty}` has no input field `{name}` (at {path})"),
                        )
                    })?;
                    self.walk_value(&field.named_type, inner, &format!("{path}.{name}"), pos)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Every documented example is a query this schema will answer, whether it is
/// written in a `graphql` fence or carried inside a request body.
///
/// The failure names the page, the line in that page, the column, and which block
/// of the page it was, because that is what fixing one takes.
#[test]
fn every_documented_example_matches_the_schema() {
    let surface = Surface::parse(&sdl());
    let mut checked = 0;
    let mut payloads = 0;
    for page in PAGES {
        let blocks = fenced_blocks(page.text);
        let examples: Vec<&Block> = blocks
            .iter()
            .filter(|block| block.language == "graphql")
            .collect();
        assert!(
            !examples.is_empty(),
            "{} contributed no examples to check",
            page.path
        );
        for (index, block) in examples.iter().enumerate() {
            if let Err(fault) = surface.check(&block.body) {
                panic!(
                    "{}:{}:{}: block {} of the page: {}",
                    page.path,
                    block.fence_line + fault.pos.line,
                    fault.pos.column,
                    index + 1,
                    fault.message
                );
            }
            checked += 1;
        }
        for payload in embedded_queries(&blocks) {
            if let Err(fault) = surface.check(&payload.document) {
                panic!(
                    "{}:{}: the request body on this line carries a query the schema refuses, \
                     at {}:{} of that query: {}",
                    page.path, payload.line, fault.pos.line, fault.pos.column, fault.message
                );
            }
            checked += 1;
            payloads += 1;
        }
    }
    assert!(
        checked >= FEWEST_EXAMPLES,
        "only {checked} examples were checked, so the extractor is missing blocks"
    );
    assert!(
        payloads >= FEWEST_EMBEDDED,
        "{payloads} queries were found inside a request body, fewer than the \
         {FEWEST_EMBEDDED} expected, so the payload reader no longer recognises the \
         shape the documented bodies are written in"
    );
}

/// The served schema, for the tests that check what the walk refuses.
fn served() -> Surface {
    Surface::parse(&sdl())
}

/// What the walk says about an example it will not accept.
///
/// A validator nobody has watched fail is not known to work, so each of the
/// tests below hands the walk something wrong on purpose and reads the message
/// it gets back, coordinate included.
fn refusal(surface: &Surface, example: &str) -> String {
    match surface.check(example) {
        Ok(()) => panic!("the walk accepted an example it should have refused: {example}"),
        Err(fault) => fault.to_string(),
    }
}

/// A field the root type does not have.
///
/// This is the mistake that is easiest to make by hand: the field is `runs`, it
/// takes `ids`, and `run(id:)` reads like it ought to work.
#[test]
fn a_field_the_root_does_not_have_is_refused() {
    let message = refusal(&served(), "{ run(id: \"coder-1\") { id } }");
    assert_eq!(
        message, "1:3: `Query` has no field `run` (at Query)",
        "{message}"
    );
}

/// A field that is missing several levels in, on a type the example never names.
#[test]
fn a_field_the_connection_does_not_have_is_refused() {
    let message = refusal(&served(), "{\n  runs {\n    totalCount\n  }\n}");
    assert_eq!(
        message, "3:5: `RunConnection` has no field `totalCount` (at Query.runs)",
        "{message}"
    );
}

/// An argument the field does not declare.
#[test]
fn an_argument_the_field_does_not_declare_is_refused() {
    let message = refusal(&served(), "{ runs(limit: 10) { total } }");
    assert_eq!(
        message, "1:8: `Query.runs` takes no argument `limit` (at Query.runs)",
        "{message}"
    );
}

/// A field selected inside `... on X` that `X` does not carry.
///
/// The interface members are where a rename hides best: the query still reads
/// like the documented one, and only one of the branches is wrong.
#[test]
fn a_field_the_fragment_type_does_not_have_is_refused() {
    let example = "{ runs { edges { node { executions { edges { node {\n  call { ... on ShellCall { commandLine } }\n} } } } } } }";
    let message = refusal(&served(), example);
    assert_eq!(
        message,
        "2:29: `ShellCall` has no field `commandLine` \
         (at Query.runs.edges.node.executions.edges.node.call ... on ShellCall)",
        "{message}"
    );
}

/// A fragment on a type that has nothing to do with the one it is written on.
#[test]
fn a_fragment_on_an_unrelated_type_is_refused() {
    let example = "{ runs { edges { node { executions { edges { node {\n  call { ... on LogLine { line } }\n} } } } } } }";
    let message = refusal(&served(), example);
    assert_eq!(
        message,
        "2:14: `LogLine` is not a `ToolCall` \
         (at Query.runs.edges.node.executions.edges.node.call)",
        "{message}"
    );
}

/// A field inside an argument's input object that the input type does not have.
#[test]
fn an_input_field_the_schema_does_not_have_is_refused() {
    let example = "mutation { spawnRun(input: { blueprnt: \"coder\" }) { run { id } } }";
    let message = refusal(&served(), example);
    assert_eq!(
        message,
        "1:28: `SpawnRunInput` has no input field `blueprnt` \
         (at Mutation.spawnRun(input:))",
        "{message}"
    );
}

/// A selection set on something that has no fields to select.
#[test]
fn a_selection_set_on_a_scalar_is_refused() {
    let message = refusal(&served(), "{ runs { total { value } } }");
    assert_eq!(
        message,
        "1:10: `RunConnection.total` returns `Int`, which has nothing to select inside \
         (at Query.runs.total)",
        "{message}"
    );
}

/// An object asked for with no selection set at all.
#[test]
fn an_object_with_no_selection_set_is_refused() {
    let message = refusal(&served(), "{ runs }");
    assert_eq!(
        message,
        "1:3: `Query.runs` returns `RunConnection`, so it needs a selection set (at Query.runs)",
        "{message}"
    );
}

/// A spread naming a fragment the example never defines.
#[test]
fn a_fragment_the_example_never_defines_is_refused() {
    let message = refusal(&served(), "{ runs { ...page } }");
    assert_eq!(
        message, "1:10: the example defines no fragment `page` (at Query.runs)",
        "{message}"
    );
}

/// A fragment that reaches itself, which is a failure rather than a hang.
#[test]
fn a_fragment_that_spreads_itself_is_refused() {
    let example = "{ runs { ...page } }\nfragment page on RunConnection { total ...page }";
    let message = refusal(&served(), example);
    assert_eq!(
        message, "2:40: fragment `page` spreads itself (at Query.runs ... on RunConnection)",
        "{message}"
    );
}

/// An example that is not a GraphQL document at all, reported where it broke.
#[test]
fn an_example_that_does_not_parse_is_refused() {
    let message = refusal(&served(), "{ runs { total ");
    assert!(
        message.starts_with("1:16:"),
        "the coordinate is where the parser stopped: {message}"
    );
}

/// An operation whose root the schema does not have.
///
/// Written against a schema of its own, because the served one has all three
/// roots and a walk has to say something useful about one that does not.
#[test]
fn an_operation_with_no_root_type_is_refused() {
    let surface = Surface::parse("type Query { ping: String }");
    let message = refusal(&surface, "mutation { ping }");
    assert_eq!(message, "1:1: the schema has no mutation root", "{message}");
}

/// Aliases, named fragments and `__typename` are all things the walk follows
/// rather than trips over.
#[test]
fn aliases_fragments_and_typename_are_walked() {
    let example = "query Fleet($after: Cursor) {\n  \
        active: runs(first: 5, after: $after) { __typename ...page edges { node { id } } }\n}\n\
        fragment page on RunConnection { pageInfo { hasNextPage } }";
    assert!(
        served().check(example).is_ok(),
        "{:?}",
        served().check(example).err().map(|f| f.to_string())
    );
}

/// A fragment may narrow to a member or widen back to the interface, and the
/// walk accepts both.
#[test]
fn a_fragment_may_narrow_or_widen() {
    let example = "{ tools { tools { ... on ScriptTool { path ... on Tool { name } } } } }";
    assert!(
        served().check(example).is_ok(),
        "{:?}",
        served().check(example).err().map(|f| f.to_string())
    );
}

/// A fragment with no type condition carries a directive, not a type change.
#[test]
fn a_fragment_with_no_type_condition_stays_on_the_same_type() {
    assert!(served().check("{ runs { ... { total } } }").is_ok());
}

/// The extractor keeps each fence with its language and its line.
#[test]
fn the_extractor_keeps_every_fence_with_its_language() {
    let page = "# Title\n\n```json\n{\"a\": 1}\n```\n\nprose\n\n```graphql\n{ ping }\n```\n";
    let blocks = fenced_blocks(page);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].language, "json");
    assert_eq!(blocks[1].language, "graphql");
    assert_eq!(blocks[1].body, "{ ping }\n");
    assert_eq!(blocks[1].fence_line, 9);
}

/// A page whose fence never closes is a page whose examples cannot be trusted,
/// so say so rather than quietly check fewer of them.
#[test]
#[should_panic(expected = "a fence is never closed")]
fn a_page_with_an_unclosed_fence_is_refused() {
    fenced_blocks("```graphql\n{ ping }\n");
}

/// A query inside a request body is found wherever the body is written, and the
/// line it is reported on is the line of the body.
#[test]
fn a_query_inside_a_request_body_is_found() {
    let page = "```bash\ncurl -s localhost:3000/graphql \\\n          -d '{\"query\":\"{ runs(first: 5) { total } }\"}'\n```\n";
    let payloads = embedded_queries(&fenced_blocks(page));
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].line, 3);
    assert_eq!(payloads[0].document, "{ runs(first: 5) { total } }");
    assert!(served().check(&payloads[0].document).is_ok());
}

/// The body is JSON, so its escapes are undone before the query is read.
#[test]
fn a_request_body_is_unescaped_before_it_is_walked() {
    let page = "```json\n{\"query\": \"query Fleet {\\n  runs(filter: { query: \\\"coder\\\" }) { total }\\n}\", \"variables\": {}}\n```\n";
    let payloads = embedded_queries(&fenced_blocks(page));
    assert_eq!(payloads.len(), 1);
    assert_eq!(
        payloads[0].document,
        "query Fleet {\n  runs(filter: { query: \"coder\" }) { total }\n}"
    );
    assert!(served().check(&payloads[0].document).is_ok());
}

/// A line with no request body on it carries no query.
#[test]
fn a_line_without_a_request_body_carries_no_query() {
    assert_eq!(query_value("curl -s localhost:3000/graphql \\"), None);
    assert_eq!(
        query_value("  -d '{\"variables\": {\"after\": null}}'"),
        None
    );
    // A key that is never given a string is a body this cannot read, and saying
    // nothing is what the count of payloads found then catches.
    assert_eq!(query_value("  -d '{\"query\": {}}'"), None);
}

/// A field the schema does not have is refused inside a request body too, with
/// the coordinate inside the query it carries.
#[test]
fn a_request_body_with_a_field_the_schema_lacks_is_refused() {
    let page = "```bash\ncurl -d '{\"query\":\"{ runs { edges { node { nope } } } }\"}'\n```\n";
    let payloads = embedded_queries(&fenced_blocks(page));
    assert_eq!(payloads.len(), 1);
    let message = refusal(&served(), &payloads[0].document);
    assert_eq!(
        message, "1:25: `Run` has no field `nope` (at Query.runs.edges.node)",
        "{message}"
    );
}
