//! Reading inputs typed on a command line by their declared types.

use leviath_runtime::spec::inputs::{InputValue, RegionBinding};
use leviath_runtime::spec::issues::IssueCode;
use leviath_runtime::spec::names::{ChoiceName, InputName, RegionName};

use super::*;

fn text_type() -> InputType {
    InputType::Text {
        multiline: false,
        min_len: None,
        max_len: None,
    }
}

fn decl(name: &str, ty: InputType) -> InputDecl {
    InputDecl {
        name: InputName::new(name).unwrap(),
        ty,
        required: false,
        default: None::<InputValue>,
        description: None,
        binds: Vec::new(),
    }
}

fn text(s: &str) -> RawInput {
    RawInput::Text(s.to_string())
}

fn input(flag: &str) -> Typed {
    Typed::from_input_flag(flag).unwrap()
}

/// `--input name=value` splits at the first `=`; anything else is refused
/// with the shape it should have.
#[test]
fn an_input_flag_is_name_equals_value() {
    let t = input("query=a=b");
    assert_eq!((t.name.as_str(), t.text.as_str()), ("query", "a=b"));
    assert_eq!(t.flag, "--input");
    assert_eq!(input(" n =").text, "");
    for bad in ["no-equals", "=value", "  =v"] {
        let err = Typed::from_input_flag(bad).unwrap_err();
        assert!(err.contains("--input <name>=<value>"), "{err}");
    }
    let named = Typed::from_named_flag("diff", "x");
    assert_eq!(named.flag, "--diff");
}

/// Every type reads the way the help says it does.
#[test]
fn each_type_reads_from_text() {
    let int = InputType::Int {
        min: None,
        max: None,
    };
    let float = InputType::Float {
        min: None,
        max: None,
    };
    let list = |item| InputType::List {
        item: Box::new(item),
        min: None,
        max: None,
    };
    assert_eq!(
        typed_value(&InputType::Bool, " Yes "),
        Ok(RawInput::Bool(true))
    );
    for (word, on) in [
        ("true", true),
        ("on", true),
        ("1", true),
        ("false", false),
        ("no", false),
        ("off", false),
        ("0", false),
    ] {
        assert_eq!(typed_value(&InputType::Bool, word), Ok(RawInput::Bool(on)));
    }
    assert!(typed_value(&InputType::Bool, "maybe").is_err());
    assert_eq!(typed_value(&int, " 42 "), Ok(RawInput::Int(42)));
    assert!(
        typed_value(&int, "4.2")
            .unwrap_err()
            .contains("whole number")
    );
    assert_eq!(typed_value(&float, "4.5"), Ok(RawInput::Float(4.5)));
    assert!(
        typed_value(&float, "x")
            .unwrap_err()
            .contains("not a number")
    );
    assert_eq!(
        typed_value(&list(int.clone()), "1, 2,3"),
        Ok(RawInput::List(vec![
            RawInput::Int(1),
            RawInput::Int(2),
            RawInput::Int(3)
        ]))
    );
    assert_eq!(
        typed_value(&list(int.clone()), "  "),
        Ok(RawInput::List(Vec::new()))
    );
    assert!(typed_value(&list(int.clone()), "1,x").is_err());
    assert_eq!(
        typed_value(&list(text_type()), r#"["a, b", "c"]"#),
        Ok(RawInput::List(vec![text("a, b"), text("c")]))
    );
    assert!(
        typed_value(&list(text_type()), "[nope")
            .unwrap_err()
            .contains("JSON array")
    );
    let record = InputType::Record { fields: Vec::new() };
    assert_eq!(
        typed_value(&record, r#"{"a": 1}"#),
        Ok(RawInput::Record(
            [("a".to_string(), RawInput::Int(1))].into()
        ))
    );
    assert!(
        typed_value(&record, "[1]")
            .unwrap_err()
            .contains("JSON object")
    );
    // Anything else is text for the daemon to check.
    let choice = InputType::Choice {
        options: vec![ChoiceName::new("a").unwrap()],
    };
    assert_eq!(typed_value(&choice, " a"), Ok(text(" a")));
    assert_eq!(typed_value(&InputType::Duration, "5m"), Ok(text("5m")));
}

/// Read against declarations: a text input reads its `@file`, a file input
/// attaches its file and names it, a list of files attaches each, and every
/// problem is collected at once with its path.
#[test]
fn inputs_read_against_their_declarations() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("brief.md"), "  be quick  ").unwrap();
    std::fs::write(dir.path().join("hero.png"), b"\x89PNG\r\n\x1a\nbody").unwrap();
    std::fs::write(dir.path().join("b.png"), b"\x89PNG\r\n\x1a\nmore").unwrap();
    let mut notes = decl("notes", text_type());
    notes.binds.push(InputSlot::OutputInstructions);
    notes.binds.push(InputSlot::Region(RegionBinding {
        region: RegionName::new("pad").unwrap(),
        template: None,
    }));
    let decls = vec![
        notes,
        decl("pics", text_type()),
        decl(
            "cover",
            InputType::File {
                accepts: Vec::new(),
            },
        ),
        decl(
            "shots",
            InputType::List {
                item: Box::new(InputType::File {
                    accepts: Vec::new(),
                }),
                min: None,
                max: None,
            },
        ),
        decl(
            "depth",
            InputType::Int {
                min: None,
                max: None,
            },
        ),
        decl(
            "tags",
            InputType::List {
                item: Box::new(text_type()),
                min: None,
                max: None,
            },
        ),
    ];
    let typed = vec![
        input("notes=@brief.md"),
        Typed::from_named_flag("pics", "@hero.png"),
        input("cover=@hero.png"),
        input("shots=hero.png, b.png"),
        input("depth=3"),
        input("tags=a,b"),
    ];
    let read = read_inputs(&decls, &typed, dir.path());
    assert_eq!(read.values.get("notes"), Some(&text("be quick")));
    assert!(!read.values.contains_key("pics"), "a picture is a part");
    assert_eq!(read.values.get("cover"), Some(&text("hero.png")));
    assert_eq!(
        read.values.get("shots"),
        Some(&RawInput::List(vec![text("hero.png"), text("b.png")]))
    );
    assert_eq!(read.values.get("depth"), Some(&RawInput::Int(3)));
    assert_eq!(
        read.values.get("tags"),
        Some(&RawInput::List(vec![text("a"), text("b")]))
    );
    let regions: Vec<Option<&str>> = read.parts.iter().map(|p| p.region.as_deref()).collect();
    assert_eq!(regions, [Some("pics"), None, None, None]);
    // The notes went to the region they fill, not the first slot they name.
    let read = read_inputs(&decls, &[input("notes=see @hero.png")], dir.path());
    assert_eq!(read.parts[0].region.as_deref(), Some("pad"));

    // Text naming no file stays text, and says so.
    let read = read_inputs(&decls, &[input("notes=see @ghost.png")], dir.path());
    assert_eq!(read.unresolved, ["ghost.png"]);

    let read = read_inputs(
        &decls,
        &[
            input("bogus=1"),
            input("depth=deep"),
            input("depth=2"),
            input("depth=4"),
            input("cover=@missing.png"),
            input("notes=@missing.md"),
            input("tags=a,b"),
        ],
        dir.path(),
    );
    // What did read is kept, so the daemon checks it with everything else.
    assert_eq!(read.values.get("depth"), Some(&RawInput::Int(2)));
    assert!(read.values.contains_key("tags"));
    assert!(!read.values.contains_key("bogus"));
    let issues = read.issues;
    let lines: Vec<String> = issues.iter().map(ToString::to_string).collect();
    assert_eq!(issues.len(), 5, "{lines:#?}");
    assert_eq!(issues.0[0].path.to_string(), "inputs.bogus");
    assert_eq!(issues.0[0].code, IssueCode::Unknown);
    assert!(issues.0[0].known.contains(&"depth".to_string()));
    assert_eq!(issues.0[1].code, IssueCode::WrongType);
    assert_eq!(issues.0[1].expected.as_deref(), Some("an integer"));
    assert_eq!(issues.0[1].got.as_deref(), Some("\"deep\""));
    assert!(issues.0[2].message.contains("more than once"), "{lines:#?}");
    assert_eq!(issues.0[3].path.to_string(), "inputs.cover");
    assert_eq!(issues.0[4].path.to_string(), "inputs.notes");
    // A file that is not there is not a value of the wrong type.
    assert_eq!(issues.0[3].code, IssueCode::Unresolvable, "{lines:#?}");
    assert_eq!(issues.0[4].code, IssueCode::Unresolvable, "{lines:#?}");
}
