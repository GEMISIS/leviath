//! Tests for the typed tool calls, and the lockstep that keeps them true.
//!
//! Thirty-five hand-written types over schemas declared somewhere else is
//! exactly the arrangement that rots: a tool gains an argument, nobody thinks of
//! this file, and every call of that tool quietly starts coming back untyped.
//! So the tests here do not check a sample. They read the tool catalog's own
//! declared schemas and hold every field of every type against them, and they
//! build a call of every typed tool from those schemas and check it comes back
//! as its own type.

use async_graphql::{EmptyMutation, EmptySubscription, Schema};

use super::args_rest::SubmittedArtifact;
use super::*;

/// Every tool the catalog declares, with its declared parameter schema.
fn catalog() -> Vec<(String, serde_json::Value)> {
    let builtins = leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(
        std::path::PathBuf::from("."),
    ));
    builtins
        .tool_defs()
        .into_iter()
        .chain(leviath_tools::BuiltinTools::subagent_tool_defs())
        .map(|tool| (tool.name, tool.parameters))
        .collect()
}

/// A root that hands out one tool call, so the types under test reach the SDL.
struct Probe;

#[async_graphql::Object]
impl Probe {
    /// One call, typed.
    async fn call(&self) -> ToolCall {
        tool_call("read_file", None, r#"{"path":"x.txt"}"#)
    }
}

/// The schema these types appear in.
fn sdl() -> String {
    Schema::build(Probe, EmptyMutation, EmptySubscription)
        .register_output_type::<ToolCall>()
        .finish()
        .sdl()
}

/// The field names of one SDL type, each with whether it is non-null.
///
/// A small reader rather than introspection, because what is being checked is
/// the published shape and that is what the SDL is.
fn sdl_fields(sdl: &str, type_name: &str) -> Vec<(String, bool)> {
    let header = format!("type {type_name} {{");
    let body = sdl
        .split_once(&header)
        .unwrap_or_else(|| panic!("{type_name} is not in the schema"))
        .1
        .split_once("\n}")
        .expect("a type block ends")
        .0;
    // Descriptions are block strings, and a description's own text can hold a
    // colon, so the reader has to know when it is inside one.
    let mut inside_description = false;
    let mut fields = Vec::new();
    for line in body.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix(r#"""""#) {
            // A one-line description opens and closes on the same line.
            if !rest.ends_with(r#"""""#) || rest.len() < 3 {
                inside_description = !inside_description;
            }
            continue;
        }
        if inside_description {
            continue;
        }
        if let Some((name, ty)) = line.split_once(':') {
            fields.push((name.trim().to_string(), ty.trim().ends_with('!')));
        }
    }
    fields
}

/// A GraphQL field name as async-graphql renames it from a Rust field.
fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper_next = false;
    for c in key.chars() {
        match c {
            '_' => upper_next = true,
            c if upper_next => {
                out.extend(c.to_uppercase());
                upper_next = false;
            }
            c => out.push(c),
        }
    }
    out
}

/// Every tool the catalog declares has a type here, and nothing here names a
/// tool the catalog does not declare.
///
/// The second half matters as much as the first: a type for a tool that no
/// longer exists is a promise the API cannot keep, and it would sit in the
/// published schema looking authoritative.
#[test]
fn every_tool_in_the_catalog_has_a_typed_call() {
    let declared: std::collections::BTreeSet<String> =
        catalog().into_iter().map(|(name, _)| name).collect();
    let typed: std::collections::BTreeSet<String> = TYPED_TOOLS
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();
    let missing: Vec<&String> = declared.difference(&typed).collect();
    assert!(missing.is_empty(), "tools with no typed call: {missing:?}");
    let invented: Vec<&String> = typed.difference(&declared).collect();
    assert!(
        invented.is_empty(),
        "typed calls for tools the catalog does not declare: {invented:?}"
    );
}

/// Each arguments type mirrors its tool's declared schema, field for field.
///
/// Names are compared after the rename GraphQL applies, and a field is non-null
/// exactly where the schema lists the key as required. Both halves are checked:
/// a missing field drops what the model sent, and an extra one promises
/// something no tool accepts.
#[test]
fn each_arguments_type_matches_the_declared_schema() {
    let sdl = sdl();
    for (tool, args_type) in TYPED_TOOLS {
        let (_, schema) = catalog()
            .into_iter()
            .find(|(name, _)| name == tool)
            .expect("the catalog declares it");
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        let required: Vec<&str> = schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|names| names.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();

        if args_type.is_empty() {
            // A tool that takes nothing has no arguments type, and the catalog
            // has to agree: a key here would be one the API cannot show.
            assert!(
                properties.is_empty(),
                "{tool} declares arguments but has no type for them"
            );
            continue;
        }

        let mut declared: Vec<(String, bool)> = properties
            .keys()
            .map(|key| (camel(key), required.contains(&key.as_str())))
            .collect();
        declared.sort();
        // A type outside the mirror still carries the suffix every output type
        // has, so the lookup has to ask for the name it was actually served
        // under.
        let mut served = sdl_fields(&sdl, &format!("{args_type}Output"));
        served.sort();
        assert_eq!(
            served, declared,
            "{tool}: {args_type} does not match the declared schema"
        );
    }
}

/// A call of every typed tool comes back as its own type.
///
/// The arguments are built from each tool's own declared schema, so this covers
/// every dispatch arm without a fixture per tool, and a type whose fields cannot
/// accept what its schema declares fails here rather than in production.
#[test]
fn a_call_of_every_typed_tool_reads_back_as_its_own_type() {
    for (tool, _) in TYPED_TOOLS {
        let (_, schema) = catalog()
            .into_iter()
            .find(|(name, _)| name == tool)
            .expect("the catalog declares it");
        let arguments = sample_arguments(&schema);
        let call = tool_call(tool, None, &arguments.to_string());
        assert!(
            !matches!(call, ToolCall::Untyped(_)),
            "{tool} came back untyped for its own declared shape: {arguments}"
        );
    }
}

/// A wrongly typed argument leaves every typed call untyped.
///
/// The reading is generated per tool, so "this did not fit" is a separate
/// decision for each of them: one tool refusing a number where a path belongs
/// says nothing about the next. A key nothing declares is not the test, because
/// a tool with only optional arguments accepts one, exactly as its own validator
/// does.
#[test]
fn a_wrongly_typed_argument_leaves_every_tool_untyped() {
    for (tool, args_type) in TYPED_TOOLS {
        // A tool that takes nothing has nothing to get wrong.
        if args_type.is_empty() {
            continue;
        }
        let (_, schema) = catalog()
            .into_iter()
            .find(|(name, _)| name == tool)
            .expect("the catalog declares it");
        let mut args = sample_arguments(&schema);
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        let (key, property) = properties.iter().next().expect("it takes something");
        args.as_object_mut()
            .expect("an object")
            .insert(key.clone(), wrong_type_for(property));

        let call = tool_call(tool, None, &args.to_string());
        let ToolCall::Untyped(untyped) = call else {
            panic!("{tool} accepted a {key} of the wrong type: {args}");
        };
        assert_eq!(
            untyped.reason,
            UntypedCallReason::ArgumentsDidNotMatch,
            "{tool}"
        );
    }
}

/// A value of a type this property does not declare.
fn wrong_type_for(property: &serde_json::Value) -> serde_json::Value {
    match property.get("type").and_then(serde_json::Value::as_str) {
        // A list is wrong for every scalar, and a number is wrong for a list or
        // an object.
        Some("array") | Some("object") => serde_json::json!(7),
        _ => serde_json::json!([7]),
    }
}

/// The smallest arguments object a schema accepts: one value per required key.
fn sample_arguments(schema: &serde_json::Value) -> serde_json::Value {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required: Vec<String> = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut args = serde_json::Map::new();
    for key in required {
        let property = properties.get(&key).cloned().unwrap_or_default();
        args.insert(key, sample_value(&property));
    }
    serde_json::Value::Object(args)
}

/// One value of the type a property declares.
fn sample_value(property: &serde_json::Value) -> serde_json::Value {
    match property.get("type").and_then(serde_json::Value::as_str) {
        Some("string") => serde_json::Value::String("sample".to_string()),
        Some("integer") | Some("number") => serde_json::json!(1),
        Some("boolean") => serde_json::json!(true),
        Some("array") => {
            let items = property.get("items").cloned().unwrap_or_default();
            serde_json::Value::Array(vec![sample_value(&items)])
        }
        Some("object") => {
            // An object with declared properties needs its own required keys;
            // one with none takes anything, which is what fan-out context is.
            serde_json::Value::Object(
                sample_arguments(property)
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            )
        }
        // A `oneOf` branch, which `submit_output`'s artifacts use: the string
        // branch is the one a bare path takes.
        _ => serde_json::Value::String("sample".to_string()),
    }
}

/// A tool this build has no type for keeps its arguments and says why.
#[test]
fn an_unknown_tool_stays_untyped_and_says_so() {
    let call = tool_call(
        "some_mcp_server__lookup",
        Some("looks things up".to_string()),
        r#"{"anything":[1,2,3]}"#,
    );
    let ToolCall::Untyped(untyped) = call else {
        panic!("an MCP tool has no type here");
    };
    assert_eq!(untyped.reason, UntypedCallReason::NoTypeForThisTool);
    assert_eq!(untyped.tool_name, "some_mcp_server__lookup");
    assert_eq!(
        untyped.raw_arguments.0,
        serde_json::json!({"anything": [1, 2, 3]})
    );
}

/// A known tool called with arguments that do not fit its shape is untyped for a
/// different reason, and that reason is the interesting one.
///
/// The typed view cannot show this call, and inventing a path for it would hide
/// the mistake. What comes back says the tool is known, the arguments are not
/// what it takes, and here is what the model actually sent.
#[test]
fn a_known_tool_with_arguments_that_do_not_fit_says_which_problem_it_is() {
    let call = tool_call("read_file", None, r#"{"paths":["x.txt"]}"#);
    let ToolCall::Untyped(untyped) = call else {
        panic!("read_file takes a path, not a list");
    };
    assert_eq!(untyped.reason, UntypedCallReason::ArgumentsDidNotMatch);
    assert_eq!(
        untyped.raw_arguments.0,
        serde_json::json!({"paths": ["x.txt"]})
    );
}

/// Arguments that are not JSON at all are kept as the text they were.
///
/// A torn write or a model that sent a bare word both land here. Dropping it
/// would remove the one piece of evidence about what went wrong.
#[test]
fn arguments_that_are_not_json_survive_as_text() {
    let call = tool_call("shell", None, "ls -la");
    let ToolCall::Untyped(untyped) = call else {
        panic!("a bare word is not a shell call's arguments");
    };
    assert_eq!(untyped.reason, UntypedCallReason::ArgumentsDidNotMatch);
    assert_eq!(untyped.raw_arguments.0, serde_json::json!("ls -la"));
}

/// An alias is typed as the tool it means, while the name stays as it was
/// called.
///
/// `bash` is `shell`, and a console showing the history should show the word the
/// model used and still know which tool ran.
#[test]
fn an_alias_types_as_the_tool_it_means() {
    let call = tool_call("bash", None, r#"{"command":"ls"}"#);
    let ToolCall::Shell(shell) = call else {
        panic!("bash is shell");
    };
    assert_eq!(shell.tool_name, "bash", "the word the model used");
    assert_eq!(shell.args.command, "ls");
}

/// A tool that takes nothing is typed from an empty object, and from a missing
/// one.
#[test]
fn a_tool_that_takes_nothing_is_typed_either_way() {
    assert!(matches!(
        tool_call("current_time", None, "{}"),
        ToolCall::CurrentTime(_)
    ));
    assert!(matches!(
        tool_call("runtime_info", None, "null"),
        ToolCall::RuntimeInfo(_)
    ));
}

/// The optional keys are optional: a call that leaves them out still types.
#[test]
fn a_call_without_its_optional_arguments_still_types() {
    let ToolCall::WriteFile(write) =
        tool_call("write_file", None, r#"{"path":"x.txt","content":"hello"}"#)
    else {
        panic!("that is a write_file call");
    };
    assert_eq!(write.args.append, None, "left out, not defaulted to false");
    let ToolCall::ContextAttach(attach) = tool_call(
        "context_attach",
        None,
        r#"{"region":"notes","path":"a.png","type":"image/png"}"#,
    ) else {
        panic!("that is a context_attach call");
    };
    // The key is `type` on the wire, which is neither a Rust nor a GraphQL
    // spelling problem, and it has to survive both.
    assert_eq!(attach.args.mime_type.as_deref(), Some("image/png"));
    assert_eq!(attach.args.deliver, None);
}

/// An artifact is typed as whichever of the tool's two shapes it was written
/// in, and which one that was is part of the answer.
#[test]
fn an_artifact_keeps_the_shape_the_model_wrote_it_in() {
    let ToolCall::SubmitOutput(submit) = tool_call(
        "submit_output",
        None,
        r#"{"content":"done","artifacts":["out/report.md",
             {"path":"out/chart.png","name":"Chart","type":"image/png"},
             {"path":"out/raw.bin"}]}"#,
    ) else {
        panic!("that is a submit_output call");
    };
    let artifacts = submit.args.artifacts.expect("three artifacts");
    let SubmittedArtifact::Path(bare) = &artifacts[0] else {
        panic!("a bare string is a path on its own");
    };
    assert_eq!(bare.path, "out/report.md");
    let SubmittedArtifact::Described(full) = &artifacts[1] else {
        panic!("an object is the described shape");
    };
    assert_eq!(full.path, "out/chart.png");
    assert_eq!(full.name.as_deref(), Some("Chart"));
    // `type` on the wire, `mimeType` in the schema, as everywhere else here.
    assert_eq!(full.mime_type.as_deref(), Some("image/png"));
    let SubmittedArtifact::Described(sparse) = &artifacts[2] else {
        panic!("an object with only a path is still the described shape");
    };
    assert_eq!(sparse.name, None, "left to the file name");
    assert_eq!(sparse.mime_type, None, "left to the registry");
}

/// An artifact in neither shape does not get a typed reading, and the call says
/// so rather than dropping the entry.
#[test]
fn an_artifact_in_neither_shape_leaves_the_call_untyped() {
    let call = tool_call(
        "submit_output",
        None,
        r#"{"content":"done","artifacts":[{"file":"out/report.md"}]}"#,
    );
    let ToolCall::Untyped(untyped) = call else {
        panic!("an artifact with no path is not what the tool takes");
    };
    assert_eq!(untyped.reason, UntypedCallReason::ArgumentsDidNotMatch);
    assert_eq!(
        untyped.raw_arguments.0["artifacts"][0]["file"],
        serde_json::json!("out/report.md")
    );
}
