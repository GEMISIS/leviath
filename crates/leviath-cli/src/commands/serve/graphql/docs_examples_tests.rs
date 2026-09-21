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
//!
//! An argument's value is walked the same way, against the type the schema
//! declares for it. The line it holds is the server's: a value the server would
//! coerce is accepted, and one the server would refuse is a failure naming the
//! coordinate. So a single value stands for the list of one, a scalar this
//! schema defines reads whatever it is handed, and a variable fits wherever it
//! is written - while a string where an input object belongs, a list where one
//! value belongs, an enum value the enum does not name and a null the schema
//! refuses are each caught here rather than by a reader.
//!
//! Among the five scalars the spec defines, the literal itself is read too:
//! [`coercion`] holds what each of them takes, and a pairing no coercion
//! reaches - `"50"` for an `Int`, `4.0` for an `ID` - is a failure. Only those.
//! A walk stricter than the server would refuse correct documentation and teach
//! people to distrust it, which is worse than one with a gap, so every pairing
//! the server would coerce is walked through.

use std::collections::{HashMap, HashSet};
use std::fmt;

use async_graphql::parser::types::{
    BaseType, ExecutableDocument, Field, FieldDefinition, FragmentSpread, InputValueDefinition,
    OperationType, Selection, SelectionSet, Type, TypeKind, TypeSystemDefinition,
};
use async_graphql::parser::{Pos, parse_query, parse_schema};
use async_graphql::{Name, Positioned, Value};

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

/// Exactly the examples the pages carry, so losing one fails the build.
///
/// An extractor that stops finding blocks is how a test like this rots: it keeps
/// passing, over nothing. A floor set under what is really there is the same rot
/// more slowly, because the gap is how many examples may quietly stop being
/// checked. This one is the count itself.
const FEWEST_EXAMPLES: usize = 40;

/// Exactly the queries the pages carry inside a request body.
///
/// The `curl` line under Auth is the first example a reader copies, and it is a
/// query in a shell string rather than in a fence of its own. Rewriting it into
/// a shape the payload reader no longer knows has to fail the build, or the
/// worst example on the page becomes the one nothing checks.
const FEWEST_EMBEDDED: usize = 1;

/// How far a count may rise above its floor before the floor is raised.
///
/// Zero would mean every added example fails the build until somebody edits a
/// constant, which teaches people to distrust the check. This much slack lets a
/// page grow by a few examples in peace, and caps at a handful how many can be
/// lost again without anybody hearing about it.
const HEADROOM: usize = 5;

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
    /// One of the five scalars the spec defines. What each takes is written
    /// down, so a value handed to one can be judged.
    Builtin,
    /// A scalar the schema defines itself. It reads whatever its own parser
    /// accepts - `JSON` takes an object whose keys are the caller's business -
    /// so a value handed to one is taken as it comes.
    Custom,
    /// An enum, with the values it names.
    Enum(HashSet<String>),
    /// An object, an interface or a union: a selection set belongs here.
    Composite,
    /// An input object: the shape an argument value is checked against.
    Input,
}

/// A type exactly as an argument or an input field declares it.
///
/// The wrappers are the part that matters on the way in: a list where one value
/// belongs is an error, one value where a list belongs is not, and a null is an
/// error only where the schema refuses one.
enum TypeRef {
    /// A named type, and whether this position refuses null.
    Named { name: String, required: bool },
    /// A list of another type, and whether this position refuses null.
    List { item: Box<TypeRef>, required: bool },
}

impl TypeRef {
    /// The name left after every `!` and `[]` comes off.
    fn named(&self) -> &str {
        match self {
            Self::Named { name, .. } => name,
            Self::List { item, .. } => item.named(),
        }
    }

    /// Whether this position refuses a null.
    fn required(&self) -> bool {
        match self {
            Self::Named { required, .. } | Self::List { required, .. } => *required,
        }
    }
}

/// One declared type, read into the form a value walk uses.
fn type_ref(ty: &Type) -> TypeRef {
    let required = !ty.nullable;
    match &ty.base {
        BaseType::Named(name) => TypeRef::Named {
            name: name.to_string(),
            required,
        },
        BaseType::List(item) => TypeRef::List {
            item: Box::new(type_ref(item)),
            required,
        },
    }
}

/// What a variable becomes on the way into a value walk.
///
/// An example writes a variable where a client would put a value, and what that
/// value is is the client's business, so it has to fit wherever it is written.
/// The name is one no schema can declare: the spec reserves the `__` prefix.
const VARIABLE_STANDIN: &str = "__variable";

/// One type of the schema, reduced to what a walk asks of it.
struct Shape {
    kind: Kind,
    /// Field name to what that field offers. Empty for a scalar, an enum and a
    /// union, which is why selecting a field on any of them fails.
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
    /// Argument name to the type that argument declares, so a value given for
    /// one is checked against the same table of types.
    arguments: HashMap<String, TypeRef>,
    /// The field's own declared type. A selection set is checked against the
    /// name at the bottom of it, and an input field's value against the whole
    /// of it, wrappers included.
    ty: TypeRef,
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
                        type_ref(&argument.node.ty.node),
                    )
                })
                .collect();
            (
                field.node.name.node.to_string(),
                FieldShape {
                    arguments,
                    ty: type_ref(&field.node.ty.node),
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
                    ty: type_ref(&field.node.ty.node),
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
                    kind: Kind::Builtin,
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
                        TypeKind::Enum(enumeration) => {
                            let values = enumeration
                                .values
                                .iter()
                                .map(|value| value.node.value.node.to_string())
                                .collect();
                            (Kind::Enum(values), HashMap::new())
                        }
                        // A scalar written in the SDL is one this schema
                        // defines, whatever it is called: the five the spec
                        // gives are registered above and never redeclared.
                        TypeKind::Scalar => (Kind::Custom, HashMap::new()),
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
            // A variable becomes the stand-in, so the shape around it is still
            // walked while the value it carries is left to the client.
            let constant = value
                .node
                .clone()
                .into_const_with(|_| Ok::<_, ()>(Value::Enum(Name::new(VARIABLE_STANDIN))))
                .unwrap_or_default();
            self.surface.walk_value(
                argument_type,
                &constant,
                &format!("{here}({given}:)"),
                value.pos,
            )?;
        }
        let selection = &field.node.selection_set.node;
        let returns = definition.ty.named();
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
                    ty: returns.to_string(),
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

/// Whether a built-in scalar takes this literal, and what it does take.
///
/// The spec's input coercion rules for the five scalars it defines, which are
/// asymmetric and worth writing out. `Float` takes an integer, with the empty
/// fraction added, and `Int` does not take a float. `ID` takes a string or an
/// integer, because a service is free to key its ids either way, and refuses a
/// float outright. Nothing else crosses: a string holding digits is a string,
/// and the only value a `Boolean` takes is a boolean.
///
/// The second half of the answer is the whole point of the first: it is what a
/// refusal has to say, and holding the two together is what keeps the words in
/// a message from drifting away from the rule that produced it.
fn coercion(name: &str, value: &Value) -> (bool, &'static str) {
    let number = matches!(value, Value::Number(_));
    let integer = matches!(value, Value::Number(given) if !given.is_f64());
    let string = matches!(value, Value::String(_));
    match name {
        "Int" => (integer, "an integer"),
        "Float" => (number, "a number"),
        "String" => (string, "a string"),
        "Boolean" => (matches!(value, Value::Boolean(_)), "a boolean"),
        // `ID` is the one left: the five are registered together, and no
        // schema may redeclare any of them.
        _ => (string || integer, "a string or an integer"),
    }
}

/// What a literal is, in the words a refusal uses for it.
fn literal_kind(value: &Value) -> &'static str {
    match value {
        Value::Number(given) if given.is_f64() => "a float",
        Value::Number(_) => "an integer",
        Value::String(_) => "a string",
        Value::Boolean(_) => "a boolean",
        // A null, a list and an object are each answered before a literal
        // reaches this, and a variable never reaches it at all. What is left of
        // what the parser can write is the enum value.
        _ => "an enum value",
    }
}

impl Surface {
    /// Walk one argument value against the type it is given for.
    ///
    /// What the server would coerce, this accepts: a single value stands for the
    /// list of one, a null is fine wherever the schema allows one, and a scalar
    /// the schema defines itself reads whatever it is handed. What the server
    /// would refuse, this refuses, and says where.
    fn walk_value(&self, ty: &TypeRef, value: &Value, path: &str, pos: Pos) -> Result<(), Fault> {
        if matches!(value, Value::Enum(name) if name.as_str() == VARIABLE_STANDIN) {
            return Ok(());
        }
        if value == &Value::Null {
            return match ty.required() {
                true => Err(Fault::at(
                    pos,
                    format!("a null is not allowed here (at {path})"),
                )),
                false => Ok(()),
            };
        }
        match ty {
            TypeRef::List { item, .. } => match value {
                Value::List(items) => {
                    for (index, inner) in items.iter().enumerate() {
                        self.walk_value(item, inner, &format!("{path}[{index}]"), pos)?;
                    }
                    Ok(())
                }
                // One value where a list belongs is the list of one. The spec
                // coerces it, so it is read as an item of that list.
                single => self.walk_value(item, single, path, pos),
            },
            TypeRef::Named { name, .. } => self.named_value(name, value, path, pos),
        }
    }

    /// Walk one value against a named type, with no wrappers left on it.
    fn named_value(&self, name: &str, value: &Value, path: &str, pos: Pos) -> Result<(), Fault> {
        let shape = self.shape(name, pos, path)?;
        match (&shape.kind, value) {
            (Kind::Custom, _) => Ok(()),
            (_, Value::List(_)) => Err(Fault::at(
                pos,
                format!("`{name}` takes one value, and `{value}` is a list (at {path})"),
            )),
            (Kind::Input, Value::Object(given)) => {
                for (field, inner) in given {
                    let declared = shape.fields.get(field.as_str()).ok_or_else(|| {
                        Fault::at(
                            pos,
                            format!("`{name}` has no input field `{field}` (at {path})"),
                        )
                    })?;
                    self.walk_value(&declared.ty, inner, &format!("{path}.{field}"), pos)?;
                }
                Ok(())
            }
            (Kind::Input, other) => Err(Fault::at(
                pos,
                format!("`{name}` is an input object, and `{other}` is not one (at {path})"),
            )),
            (Kind::Enum(values), Value::Enum(given)) => match values.contains(given.as_str()) {
                true => Ok(()),
                false => Err(Fault::at(
                    pos,
                    format!("`{name}` has no value `{given}` (at {path})"),
                )),
            },
            (Kind::Enum(_), other) => Err(Fault::at(
                pos,
                format!("`{name}` is an enum, and `{other}` is not one of its values (at {path})"),
            )),
            (Kind::Builtin, Value::Object(_)) => Err(Fault::at(
                pos,
                format!("`{name}` is a scalar, and `{value}` is an object (at {path})"),
            )),
            (Kind::Builtin, other) => {
                let (coerces, takes) = coercion(name, other);
                match coerces {
                    true => Ok(()),
                    false => Err(Fault::at(
                        pos,
                        format!(
                            "`{name}` takes {takes}, and `{other}` is {} (at {path})",
                            literal_kind(other)
                        ),
                    )),
                }
            }
            (Kind::Composite, _) => Err(Fault::at(
                pos,
                format!("`{name}` is not an input type (at {path})"),
            )),
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
    let mut walked = Counted::default();
    for page in PAGES {
        let found = walk_page(&surface, page.path, page.text);
        walked.examples += found.examples;
        walked.payloads += found.payloads;
    }
    in_band(
        "FEWEST_EXAMPLES",
        "examples",
        walked.checked(),
        FEWEST_EXAMPLES,
    )
    .unwrap_or_else(|reason| panic!("{reason}"));
    in_band(
        "FEWEST_EMBEDDED",
        "queries inside a request body",
        walked.payloads,
        FEWEST_EMBEDDED,
    )
    .unwrap_or_else(|reason| panic!("{reason}"));
}

/// What one page contributed to a run of the check.
#[derive(Default)]
struct Counted {
    /// Examples in a `graphql` fence.
    examples: usize,
    /// Queries carried inside a request body.
    payloads: usize,
}

impl Counted {
    /// Every query walked, wherever it was written.
    fn checked(&self) -> usize {
        self.examples + self.payloads
    }
}

/// Walk every query one page carries, and say how many there were.
///
/// A fault is a panic rather than a returned error: it names the page, the line
/// in that page, the column, and which block of the page it was, because that is
/// what fixing one takes, and there is nothing for a caller to do with it but
/// print it.
fn walk_page(surface: &Surface, path: &str, text: &str) -> Counted {
    let blocks = fenced_blocks(text);
    let examples: Vec<&Block> = blocks
        .iter()
        .filter(|block| block.language == "graphql")
        .collect();
    assert!(
        !examples.is_empty(),
        "{path} contributed no examples to check"
    );
    let mut counted = Counted::default();
    for (index, block) in examples.iter().enumerate() {
        if let Err(fault) = surface.check(&block.body) {
            panic!(
                "{}:{}:{}: block {} of the page: {}",
                path,
                block.fence_line + fault.pos.line,
                fault.pos.column,
                index + 1,
                fault.message
            );
        }
        counted.examples += 1;
    }
    for payload in embedded_queries(&blocks) {
        if let Err(fault) = surface.check(&payload.document) {
            panic!(
                "{}:{}: the request body on this line carries a query the schema refuses, \
                 at {}:{} of that query: {}",
                path, payload.line, fault.pos.line, fault.pos.column, fault.message
            );
        }
        counted.payloads += 1;
    }
    counted
}

/// Whether the number found still sits in the band its floor opens.
///
/// Below the floor, something that was being checked no longer is. Further above
/// it than [`HEADROOM`], the floor has fallen behind far enough to stop being a
/// guard, and the message says which constant to move and what to.
fn in_band(constant: &str, what: &str, found: usize, floor: usize) -> Result<(), String> {
    if found < floor {
        return Err(format!(
            "only {found} {what} were checked, and these pages carry {floor}. Some have been \
             lost, or the reader that finds them no longer recognises how they are written. \
             Put them back rather than lowering {constant}."
        ));
    }
    if found > floor + HEADROOM {
        return Err(format!(
            "{found} {what} are checked and {constant} is {floor}, which is more than \
             {HEADROOM} behind. Raise {constant} to {found}, so that losing one is still \
             noticed."
        ));
    }
    Ok(())
}

/// The served schema, for the tests that check what the walk refuses.
fn served() -> Surface {
    Surface::parse(&sdl())
}

/// One page's text with its first `graphql` fence taken out of it.
///
/// Line by line, the way the extractor itself reads a page, so that what is
/// removed is exactly one of the things the extractor would have found.
fn without_its_first_example(text: &str) -> String {
    let mut kept = String::new();
    let mut cutting = false;
    let mut cut_one = false;
    for line in text.lines() {
        if cutting {
            cutting = !line.trim_start().starts_with("```");
            continue;
        }
        if !cut_one && line.trim() == "```graphql" {
            cutting = true;
            cut_one = true;
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    assert!(cut_one, "the page carries an example to remove");
    kept
}

/// Deleting one example fails the build, which is what the floor is for.
///
/// The pages are the real ones with a single `graphql` fence cut out, so what is
/// counted is what the check would count on the day somebody drops an example -
/// not a number invented for a test.
#[test]
fn an_example_removed_from_a_page_is_noticed() {
    let surface = served();
    let mut walked = Counted::default();
    for page in PAGES {
        let text = match page.path {
            "docs/content/graphql.md" => without_its_first_example(page.text),
            _ => page.text.to_string(),
        };
        let found = walk_page(&surface, page.path, &text);
        walked.examples += found.examples;
        walked.payloads += found.payloads;
    }
    assert_eq!(
        walked.checked(),
        FEWEST_EXAMPLES - 1,
        "exactly one example fewer"
    );
    let reason = in_band(
        "FEWEST_EXAMPLES",
        "examples",
        walked.checked(),
        FEWEST_EXAMPLES,
    )
    .expect_err("one short of the floor");
    assert!(
        reason.contains(&format!("only {} examples", FEWEST_EXAMPLES - 1)),
        "it says how many are left: {reason}"
    );
    assert!(
        reason.contains("lost") && reason.contains("Put them back"),
        "it says what to do about it: {reason}"
    );
}

/// A floor left behind by a page that grew says what to raise it to, rather
/// than sitting there checking a shrinking share of the examples.
#[test]
fn a_floor_that_has_fallen_behind_says_what_to_raise_it_to() {
    let reason = in_band("FEWEST_EXAMPLES", "examples", 40, 30).expect_err("ten past the floor");
    assert!(
        reason.contains("Raise FEWEST_EXAMPLES to 40"),
        "it names the constant and the number: {reason}"
    );
}

/// A page that grows by a few examples is not a failing build.
#[test]
fn a_count_inside_the_headroom_is_accepted() {
    assert!(in_band("FLOOR", "examples", 30, 30).is_ok(), "the floor");
    assert!(
        in_band("FLOOR", "examples", 30 + HEADROOM, 30).is_ok(),
        "the top of the band"
    );
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

/// A bare value where an input object belongs.
///
/// This is the shape a renamed argument leaves behind: `blueprint:` took a name
/// once and takes an object now, and an example still handing it a string reads
/// like the documented one until somebody sends it.
#[test]
fn a_value_where_an_input_object_belongs_is_refused() {
    let example =
        "mutation { spawnRun(input: { blueprint: \"coder\", task: \"t\" })\n  { run { id } } }";
    let message = refusal(&served(), example);
    assert_eq!(
        message,
        "1:28: `BlueprintInput` is an input object, and `\"coder\"` is not one \
         (at Mutation.spawnRun(input:).blueprint)",
        "{message}"
    );
}

/// An object where a scalar belongs, which is the same mistake mirrored.
#[test]
fn an_object_where_a_scalar_belongs_is_refused() {
    let message = refusal(&served(), "{ runs(first: { n: 5 }) { total } }");
    assert_eq!(
        message, "1:15: `Int` is a scalar, and `{n: 5}` is an object (at Query.runs(first:))",
        "{message}"
    );
}

/// A list where one value belongs.
///
/// The coercion runs one way only: the spec wraps a single value into a list,
/// and never unwraps a list into a single value.
#[test]
fn a_list_where_one_value_belongs_is_refused() {
    let message = refusal(&served(), "{ runs(first: [1, 2]) { total } }");
    assert_eq!(
        message, "1:15: `Int` takes one value, and `[1, 2]` is a list (at Query.runs(first:))",
        "{message}"
    );
}

/// One value where a list belongs is the list of one, which the walk accepts
/// because the server does.
#[test]
fn one_value_where_a_list_belongs_is_accepted() {
    let surface = served();
    assert!(
        surface
            .check("{ runs(filter: { ids: \"run-1\" }) { total } }")
            .is_ok()
    );
    assert!(
        surface
            .check("{ runs(filter: { ids: [\"run-1\", \"run-2\"] }) { total } }")
            .is_ok()
    );
}

/// A fault inside a list says which item it was in.
#[test]
fn a_fault_inside_a_list_names_the_item_it_was_in() {
    let example = "mutation { spawnRun(input: { task: \"t\", regions: [\n  { region: { name: \"plan\" }, text: \"x\" },\n  { regin: { name: \"plan\" }, text: \"x\" }\n] }) { run { id } } }";
    let message = refusal(&served(), example);
    assert_eq!(
        message,
        "1:28: `RegionSeedInput` has no input field `regin` \
         (at Mutation.spawnRun(input:).regions[1])",
        "{message}"
    );
}

/// An enum value the enum does not name.
#[test]
fn an_enum_value_the_schema_does_not_have_is_refused() {
    let message = refusal(
        &served(),
        "{ runs(filter: { status: FINISHED }) { total } }",
    );
    assert_eq!(
        message, "1:16: `RunStatus` has no value `FINISHED` (at Query.runs(filter:).status)",
        "{message}"
    );
}

/// A quoted string where an enum value belongs, which the server refuses even
/// when the letters inside the quotes are a member's.
#[test]
fn a_string_where_an_enum_value_belongs_is_refused() {
    let message = refusal(
        &served(),
        "{ runs(filter: { status: \"RUNNING\" }) { total } }",
    );
    assert_eq!(
        message,
        "1:16: `RunStatus` is an enum, and `\"RUNNING\"` is not one of its values \
         (at Query.runs(filter:).status)",
        "{message}"
    );
}

/// An enum value the enum does name is walked through.
#[test]
fn an_enum_value_the_schema_has_is_accepted() {
    assert!(
        served()
            .check("{ runs(filter: { status: RUNNING, sort: STARTED_AT }) { total } }")
            .is_ok()
    );
}

/// A null where the schema will not take one.
#[test]
fn a_null_where_the_schema_requires_a_value_is_refused() {
    let example = "mutation { spawnRun(input: { blueprint: { name: \"coder\" }, task: null })\n  { run { id } } }";
    let message = refusal(&served(), example);
    assert_eq!(
        message, "1:28: a null is not allowed here (at Mutation.spawnRun(input:).task)",
        "{message}"
    );
}

/// A null where the schema does take one, at both an argument and a list.
#[test]
fn a_null_where_null_is_allowed_is_accepted() {
    let surface = served();
    assert!(surface.check("{ runs(filter: null) { total } }").is_ok());
    assert!(
        surface
            .check("{ runs(filter: { ids: null }) { total } }")
            .is_ok()
    );
}

/// A scalar the schema defines itself reads whatever it is handed, so a value
/// for one is taken as it comes.
#[test]
fn a_value_for_a_scalar_the_schema_defines_is_left_alone() {
    let surface = served();
    assert!(
        surface
            .check("{ runs(after: \"cursor-1\") { total } }")
            .is_ok()
    );
    assert!(
        surface
            .check("{ runs(filter: { startedAt: { gte: 1757894400 } }) { total } }")
            .is_ok()
    );
}

/// A variable stands in for a value the example never shows, so it fits
/// wherever it is written - including where a null would not.
#[test]
fn a_variable_fits_wherever_it_is_written() {
    let example = "mutation Spawn($task: String!) {\n  \
        spawnRun(input: { blueprint: { name: \"coder\" }, task: $task }) { run { id } }\n}";
    assert!(
        served().check(example).is_ok(),
        "{:?}",
        served().check(example).err().map(|f| f.to_string())
    );
}

/// An argument whose type is an output type, which no value can satisfy.
///
/// Written against a schema of its own: the served one is generated from Rust
/// types that cannot express it, and a walk still has to say something rather
/// than wave the value through.
#[test]
fn an_output_type_as_an_argument_is_refused() {
    let surface =
        Surface::parse("type Query { ping(at: Thing): String } type Thing { id: String }");
    let message = refusal(&surface, "{ ping(at: { id: \"x\" }) }");
    assert_eq!(
        message, "1:12: `Thing` is not an input type (at Query.ping(at:))",
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

/// Every built-in scalar, and the literals a server can coerce into it.
///
/// Written out because the list is short and asymmetric. `Float` takes an
/// integer and `Int` does not take a float; `ID` takes a string or an integer
/// and nothing else, so `4.0` is refused where `4` and `"4"` are both fine. A
/// pairing this table does not name is one no coercion reaches, whatever the
/// letters inside the literal spell.
const COERCIONS: [(&str, &[&str]); 5] = [
    ("Int", &["1"]),
    ("Float", &["1", "1.5"]),
    ("String", &["\"x\""]),
    ("Boolean", &["true"]),
    ("ID", &["1", "\"x\""]),
];

/// One literal of every kind a constant value can be written as.
const LITERALS: [&str; 5] = ["1", "1.5", "\"x\"", "true", "NAME"];

/// The walk reaches the same verdict as the table on all twenty-five pairings.
///
/// Every disagreement is collected rather than the first one asserted, so a run
/// against a walk that does not read literals at all names each pairing it lets
/// through instead of stopping at the first.
#[test]
fn every_literal_reaches_only_the_scalars_that_can_take_it() {
    let mut wrong: Vec<String> = Vec::new();
    for (scalar, taken) in COERCIONS {
        let surface = Surface::parse(&format!("type Query {{ ping(at: {scalar}): String }}"));
        for literal in LITERALS {
            let accepted = surface.check(&format!("{{ ping(at: {literal}) }}")).is_ok();
            if accepted != taken.contains(&literal) {
                let verdict = match accepted {
                    true => "accepted",
                    false => "refused",
                };
                wrong.push(format!("`{scalar}` given `{literal}` was {verdict}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "the walk and the coercion table disagree: {}",
        wrong.join(", ")
    );
}

/// A string of digits where a number belongs, which is what a JSON habit
/// leaves behind.
#[test]
fn a_string_where_an_int_belongs_is_refused() {
    let message = refusal(&served(), "{ runs(first: \"50\") { total } }");
    assert_eq!(
        message, "1:15: `Int` takes an integer, and `\"50\"` is a string (at Query.runs(first:))",
        "{message}"
    );
}

/// A float where a whole number belongs.
#[test]
fn a_float_where_an_int_belongs_is_refused() {
    let message = refusal(&served(), "{ runs(first: 2.5) { total } }");
    assert_eq!(
        message, "1:15: `Int` takes an integer, and `2.5` is a float (at Query.runs(first:))",
        "{message}"
    );
}

/// A boolean where a number belongs.
#[test]
fn a_boolean_where_an_int_belongs_is_refused() {
    let message = refusal(&served(), "{ runs(first: true) { total } }");
    assert_eq!(
        message, "1:15: `Int` takes an integer, and `true` is a boolean (at Query.runs(first:))",
        "{message}"
    );
}

/// A bare name where a scalar belongs, which is what an enum value written for
/// the wrong argument looks like.
#[test]
fn an_enum_value_where_a_scalar_belongs_is_refused() {
    let message = refusal(&served(), "{ runs(first: RUNNING) { total } }");
    assert_eq!(
        message,
        "1:15: `Int` takes an integer, and `RUNNING` is an enum value (at Query.runs(first:))",
        "{message}"
    );
}

/// A number where a string belongs, the mirror of the first one.
#[test]
fn a_number_where_a_string_belongs_is_refused() {
    let message = refusal(&served(), "{ runs(filter: { query: 7 }) { total } }");
    assert_eq!(
        message,
        "1:16: `String` takes a string, and `7` is an integer (at Query.runs(filter:).query)",
        "{message}"
    );
}

/// A float where an id belongs. An id takes a string or an integer, and the
/// float is the one number both the spec and this server refuse.
#[test]
fn a_float_where_an_id_belongs_is_refused() {
    let message = refusal(&served(), "{ runs(filter: { parent: 1.5 }) { total } }");
    assert_eq!(
        message,
        "1:16: `ID` takes a string or an integer, and `1.5` is a float \
         (at Query.runs(filter:).parent)",
        "{message}"
    );
}

/// The pairings the server does coerce, walked on the served schema rather than
/// on one written for a test.
///
/// This is the half of the rule that matters: a walk stricter than the server
/// refuses correct documentation, so each of these has to stay accepted.
#[test]
fn the_pairings_the_server_coerces_are_accepted() {
    let surface = served();
    for example in [
        // An integer where a float belongs, and a float there too.
        "mutation { putMimeRow(row: { mimeType: \"image/png\", tokens: { perByte: 1 } }) \
         { mimeType } }",
        "mutation { putMimeRow(row: { mimeType: \"image/png\", tokens: { perByte: 0.25 } }) \
         { mimeType } }",
        // An integer and a string are both ids.
        "{ runs(filter: { parent: 7 }) { total } }",
        "{ runs(filter: { parent: \"run-1\" }) { total } }",
        "{ runs(filter: { ids: [\"run-1\"] }) { total } }",
        // A scalar the schema defines reads whatever it is handed, and a
        // `Decimal` in this schema travels as a string.
        "{ runs(filter: { costUsd: { gte: \"1.00\" } }) { total } }",
        "{ runs(filter: { costUsd: { gte: 1.5 } }) { total } }",
        // `JSON` takes an object, which no built-in scalar would.
        "{ testYoloProfile(call: { profile: \"p\", tool: \"shell\", \
         arguments: { path: \"x\" } }) { profile } }",
        // A boolean where a boolean belongs.
        "{ runs(filter: { ascending: true }) { total } }",
    ] {
        assert!(
            surface.check(example).is_ok(),
            "{example}: {:?}",
            surface.check(example).err().map(|fault| fault.to_string())
        );
    }
}
