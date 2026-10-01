use super::*;

fn name(s: &str) -> InputName {
    InputName::new(s).unwrap()
}

fn decl(n: &str, ty: InputType) -> InputDecl {
    InputDecl {
        name: name(n),
        ty,
        required: false,
        default: None,
        description: None,
        binds: vec![],
    }
}

fn text() -> InputType {
    InputType::Text {
        multiline: false,
        min_len: None,
        max_len: None,
    }
}

fn raw(pairs: &[(&str, RawInput)]) -> BTreeMap<String, RawInput> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn one(ty: &InputType, value: RawInput) -> Result<InputValue, SpawnIssues> {
    one_with(ty, value, &[])
}

fn one_with(
    ty: &InputType,
    value: RawInput,
    attachments: &[String],
) -> Result<InputValue, SpawnIssues> {
    let mut issues = SpawnIssues::new();
    let cx = CheckCtx { attachments };
    let v = ty.check(
        &value,
        &SpecPath::root().field("inputs").key("x"),
        &cx,
        &mut issues,
    );
    match v {
        Some(v) => Ok(v),
        None => Err(issues),
    }
}

fn first(issues: SpawnIssues) -> SpawnIssue {
    issues.0.into_iter().next().unwrap()
}

#[test]
fn text_is_bounded_in_characters() {
    let ty = InputType::Text {
        multiline: true,
        min_len: Some(2),
        max_len: Some(3),
    };
    assert_eq!(
        one(&ty, RawInput::Text("héé".into())),
        Ok(InputValue::Text("héé".into()))
    );
    let short = first(one(&ty, RawInput::Text("a".into())).unwrap_err());
    assert_eq!(short.code, IssueCode::OutOfRange);
    assert_eq!(short.expected.as_deref(), Some("text of 2 to 3 characters"));
    assert_eq!(short.path.to_string(), "inputs.x");
    let wrong = first(one(&ty, RawInput::Int(3)).unwrap_err());
    assert_eq!(wrong.code, IssueCode::WrongType);
    assert_eq!(wrong.got.as_deref(), Some("the integer 3"));
}

#[test]
fn bools_are_bools() {
    assert_eq!(
        one(&InputType::Bool, RawInput::Bool(true)),
        Ok(InputValue::Bool(true))
    );
    let e = first(one(&InputType::Bool, RawInput::Text("true".into())).unwrap_err());
    assert_eq!(e.expected.as_deref(), Some("true or false"));
}

#[test]
fn ints_refuse_fractions_and_respect_bounds() {
    let ty = InputType::Int {
        min: Some(1),
        max: Some(5),
    };
    assert_eq!(one(&ty, RawInput::Int(5)), Ok(InputValue::Int(5)));
    let high = first(one(&ty, RawInput::Int(6)).unwrap_err());
    assert_eq!(high.code, IssueCode::OutOfRange);
    assert_eq!(high.expected.as_deref(), Some("an integer 1 to 5"));
    assert_eq!(
        first(one(&ty, RawInput::Float(2.5)).unwrap_err()).code,
        IssueCode::WrongType
    );
    let low_only = InputType::Int {
        min: Some(1),
        max: None,
    };
    assert_eq!(low_only.describe(), "an integer at least 1");
    let high_only = InputType::Int {
        min: None,
        max: Some(9),
    };
    assert_eq!(high_only.describe(), "an integer at most 9");
}

#[test]
fn floats_take_whole_numbers_and_refuse_nan() {
    let ty = InputType::Float {
        min: Some(0.0),
        max: Some(1.0),
    };
    assert_eq!(one(&ty, RawInput::Int(1)), Ok(InputValue::Float(1.0)));
    assert_eq!(one(&ty, RawInput::Float(0.5)), Ok(InputValue::Float(0.5)));
    assert_eq!(
        first(one(&ty, RawInput::Float(1.5)).unwrap_err()).code,
        IssueCode::OutOfRange
    );
    let open = InputType::Float {
        min: None,
        max: None,
    };
    assert_eq!(
        first(one(&open, RawInput::Float(f64::NAN)).unwrap_err()).code,
        IssueCode::OutOfRange
    );
    assert!(RawInput::Text("x".into()).as_float().is_nan());
}

#[test]
fn choices_list_their_options_when_missed() {
    let ty = InputType::Choice {
        options: vec![
            ChoiceName::new("perf").unwrap(),
            ChoiceName::new("safety").unwrap(),
        ],
    };
    assert_eq!(ty.describe(), "one of \"perf\", \"safety\"");
    assert_eq!(
        one(&ty, RawInput::Text("perf".into())),
        Ok(InputValue::Choice(ChoiceName::new("perf").unwrap()))
    );
    let miss = first(one(&ty, RawInput::Text("speed".into())).unwrap_err());
    assert_eq!(miss.code, IssueCode::Invalid);
    assert_eq!(miss.known, vec!["perf", "safety"]);
}

#[test]
fn lists_check_length_and_every_item_with_its_index() {
    let ty = InputType::List {
        item: Box::new(InputType::Int {
            min: None,
            max: None,
        }),
        min: Some(1),
        max: Some(2),
    };
    assert_eq!(ty.describe(), "a list of 1 to 2 items of an integer");
    assert_eq!(
        one(&ty, RawInput::List(vec![RawInput::Int(1)])),
        Ok(InputValue::List(vec![InputValue::Int(1)]))
    );
    let issues = one(
        &ty,
        RawInput::List(vec![
            RawInput::Int(1),
            RawInput::Text("a".into()),
            RawInput::Bool(true),
        ]),
    )
    .unwrap_err();
    let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(paths, vec!["inputs.x", "inputs.x[1]", "inputs.x[2]"]);
    assert_eq!(issues.0[0].code, IssueCode::OutOfRange);
}

#[test]
fn records_check_their_fields_by_name() {
    let mut depth = decl(
        "depth",
        InputType::Int {
            min: None,
            max: None,
        },
    );
    depth.required = true;
    let ty = InputType::Record {
        fields: vec![decl("topic", text()), depth],
    };
    assert_eq!(ty.describe(), "a table with fields topic, depth");
    let good = one(&ty, RawInput::Record(raw(&[("depth", RawInput::Int(2))]))).unwrap();
    assert_eq!(
        good,
        InputValue::Record([(name("depth"), InputValue::Int(2))].into())
    );
    let issues = one(
        &ty,
        RawInput::Record(raw(&[
            ("topic", RawInput::Int(1)),
            ("extra", RawInput::Bool(true)),
        ])),
    )
    .unwrap_err();
    let lines: Vec<String> = issues
        .iter()
        .map(|i| format!("{} {:?}", i.path, i.code))
        .collect();
    assert_eq!(
        lines,
        vec![
            "inputs.x.extra Unknown",
            "inputs.x.topic WrongType",
            "inputs.x.depth Missing"
        ]
    );
}

#[test]
fn files_name_an_attachment() {
    let ty = InputType::File { accepts: vec![] };
    assert_eq!(ty.describe(), "the name of an attached file");
    let typed = InputType::File {
        accepts: vec![
            MimePattern::new("image/*").unwrap(),
            MimePattern::new("text/csv").unwrap(),
        ],
    };
    assert_eq!(
        typed.describe(),
        "the name of an attached file of type image/* or text/csv"
    );
    let names = vec!["a.png".to_string()];
    assert_eq!(
        one_with(&ty, RawInput::Text("a.png".into()), &names),
        Ok(InputValue::File("a.png".into()))
    );
    let missing = first(one_with(&ty, RawInput::Text("b.png".into()), &names).unwrap_err());
    assert_eq!(missing.code, IssueCode::Dangling);
    assert_eq!(missing.known, vec!["a.png"]);
}

#[test]
fn paths_stay_in_the_workdir() {
    for (kind, word) in [
        (PathKind::File, "a file"),
        (PathKind::Dir, "a directory"),
        (PathKind::Any, "a path"),
    ] {
        let ty = InputType::Path {
            kind,
            must_exist: true,
        };
        assert_eq!(ty.describe(), format!("{word} inside the workdir"));
    }
    let ty = InputType::Path {
        kind: PathKind::Any,
        must_exist: false,
    };
    assert_eq!(
        one(&ty, RawInput::Text("src/a.rs".into())),
        Ok(InputValue::Path(WorkdirPath::new("src/a.rs").unwrap()))
    );
    assert_eq!(
        first(one(&ty, RawInput::Text("../x".into())).unwrap_err()).code,
        IssueCode::Invalid
    );
}

#[test]
fn models_come_as_text_or_a_table() {
    let m = |p: Option<&str>, id: &str| {
        InputValue::Model(
            ModelRef::parse(&match p {
                Some(p) => format!("{p}/{id}"),
                None => id.to_string(),
            })
            .unwrap(),
        )
    };
    assert_eq!(
        one(&InputType::Model, RawInput::Text("anthropic/claude".into())),
        Ok(m(Some("anthropic"), "claude"))
    );
    let table = RawInput::Record(raw(&[
        ("provider", RawInput::Text("a".into())),
        ("model", RawInput::Text("b".into())),
    ]));
    assert_eq!(one(&InputType::Model, table), Ok(m(Some("a"), "b")));
    let bare = RawInput::Record(raw(&[("model", RawInput::Text("b".into()))]));
    assert_eq!(one(&InputType::Model, bare), Ok(m(None, "b")));
    let odd = RawInput::Record(raw(&[("model", RawInput::Int(1))]));
    assert_eq!(
        first(one(&InputType::Model, odd).unwrap_err()).code,
        IssueCode::WrongType
    );
    let bad_table = RawInput::Record(raw(&[("model", RawInput::Text("a b".into()))]));
    assert_eq!(
        first(one(&InputType::Model, bad_table).unwrap_err()).code,
        IssueCode::Invalid
    );
    assert_eq!(
        first(one(&InputType::Model, RawInput::Text("a b".into())).unwrap_err()).code,
        IssueCode::Invalid
    );
}

#[test]
fn blueprints_durations_and_urls_parse() {
    assert!(matches!(
        one(&InputType::Blueprint, RawInput::Text("coder".into())),
        Ok(InputValue::Blueprint(_))
    ));
    assert_eq!(
        first(one(&InputType::Blueprint, RawInput::Text("".into())).unwrap_err()).code,
        IssueCode::Invalid
    );
    assert_eq!(
        one(&InputType::Duration, RawInput::Text("1h30m".into())),
        Ok(InputValue::Duration(5400))
    );
    assert_eq!(
        first(one(&InputType::Duration, RawInput::Text("soon".into())).unwrap_err()).code,
        IssueCode::Invalid
    );
    assert!(matches!(
        one(&InputType::Url, RawInput::Text("https://x.dev".into())),
        Ok(InputValue::Url(_))
    ));
    assert_eq!(
        first(one(&InputType::Url, RawInput::Text("x.dev".into())).unwrap_err()).code,
        IssueCode::Invalid
    );
    for ty in [InputType::Blueprint, InputType::Duration, InputType::Url] {
        assert!(!ty.describe().is_empty());
    }
}

#[test]
fn durations_sum_their_parts() {
    assert_eq!(parse_duration("90s"), Some(90));
    assert_eq!(parse_duration("2d"), Some(172_800));
    assert_eq!(parse_duration(" 5m "), Some(300));
    for bad in [
        "",
        "5",
        "m",
        "5x",
        "1h5",
        "99999999999999999999s",
        "999999999999999d",
        "18446744073709551615s1s",
    ] {
        assert_eq!(parse_duration(bad), None, "{bad}");
    }
}

#[test]
fn check_inputs_reports_everything_at_once() {
    let mut need = decl("need", text());
    need.required = true;
    let mut with_default = decl(
        "level",
        InputType::Int {
            min: Some(1),
            max: None,
        },
    );
    with_default.default = Some(InputValue::Int(3));
    let optional = decl("note", text());
    let decls = vec![need.clone(), with_default.clone(), optional];
    let cx = CheckCtx::default();

    let ok = check_inputs(&decls, &raw(&[("need", RawInput::Text("x".into()))]), &cx).unwrap();
    assert_eq!(ok.get("level"), Some(&InputValue::Int(3)));
    assert_eq!(ok.get("note"), None);

    let issues = check_inputs(
        &decls,
        &raw(&[("lvl", RawInput::Int(2)), ("level", RawInput::Int(0))]),
        &cx,
    )
    .unwrap_err();
    let lines: Vec<String> = issues
        .iter()
        .map(|i| format!("{} {:?}", i.path, i.code))
        .collect();
    assert_eq!(
        lines,
        vec![
            "inputs.lvl Unknown",
            "inputs.need Missing",
            "inputs.level OutOfRange"
        ]
    );
    assert_eq!(issues.0[0].known, vec!["need", "level", "note"]);
}

#[test]
fn values_render_as_region_text() {
    let list = InputValue::List(vec![InputValue::Text("a".into()), InputValue::Int(2)]);
    assert_eq!(list.render_text(), "- a\n- 2");
    let rec = InputValue::Record([(name("k"), InputValue::Bool(true))].into());
    assert_eq!(rec.render_text(), "k: true");
    assert_eq!(InputValue::Float(1.5).render_text(), "1.5");
    assert_eq!(
        InputValue::Duration(90).render_text(),
        leviath_core::duration::compact(90)
    );
    let all = [
        InputValue::Text("t".into()),
        InputValue::Bool(false),
        InputValue::Int(1),
        InputValue::Float(0.5),
        InputValue::Choice(ChoiceName::new("c").unwrap()),
        list,
        rec,
        InputValue::File("f.png".into()),
        InputValue::Path(WorkdirPath::new("a/b").unwrap()),
        InputValue::Model(ModelRef::parse("p/m").unwrap()),
        InputValue::Blueprint(BlueprintRef::parse("coder").unwrap()),
        InputValue::Duration(60),
        InputValue::Url(HttpUrl::new("https://x.dev").unwrap()),
    ];
    for v in &all {
        assert!(!v.render_text().is_empty(), "{v:?}");
    }
    // Every value survives its own round trip through the wire form.
    let types = [
        text(),
        InputType::Bool,
        InputType::Int {
            min: None,
            max: None,
        },
        InputType::Float {
            min: None,
            max: None,
        },
        InputType::Choice {
            options: vec![ChoiceName::new("c").unwrap()],
        },
        InputType::List {
            item: Box::new(InputType::Model),
            min: None,
            max: None,
        },
        InputType::Record {
            fields: vec![decl("k", InputType::Bool)],
        },
        InputType::File { accepts: vec![] },
        InputType::Path {
            kind: PathKind::Any,
            must_exist: false,
        },
        InputType::Model,
        InputType::Blueprint,
        InputType::Duration,
        InputType::Url,
    ];
    let names = vec!["f.png".to_string()];
    for (ty, v) in types.iter().zip(all.iter()) {
        let ty = match (ty, v) {
            (InputType::List { .. }, InputValue::List(_)) => &InputType::List {
                item: Box::new(InputType::Text {
                    multiline: false,
                    min_len: None,
                    max_len: None,
                }),
                min: None,
                max: None,
            },
            _ => ty,
        };
        let back = one_with(ty, v.to_raw(), &names);
        match v {
            InputValue::List(_) => assert!(back.is_err(), "mixed list needs a matching item type"),
            _ => assert_eq!(back.as_ref(), Ok(v), "{ty:?}"),
        }
    }
}

#[test]
fn templates_fill_placeholders_and_escape_braces() {
    let t = Template::parse("Focus on {focus} {{literal}} at {depth}").unwrap();
    let names: Vec<&str> = t.inputs().map(InputName::as_str).collect();
    assert_eq!(names, vec!["focus", "depth"]);
    let values = InputValues([(name("focus"), InputValue::Text("perf".into()))].into());
    assert_eq!(t.render(&values), "Focus on perf {literal} at ");
    assert_eq!(t.to_string(), "Focus on {focus} {{literal}} at {depth}");
    assert!(Template::parse("a } b").is_err());
    let leading = Template::parse("{focus}!").unwrap();
    assert_eq!(leading.render(&values), "perf!");
    assert!(Template::parse("{9bad}").is_err());
    let json = serde_json::to_string(&t).unwrap();
    assert_eq!(json, "\"Focus on {focus} {{literal}} at {depth}\"");
    let back: Template = serde_json::from_str(&json).unwrap();
    assert_eq!(back, t);
}

#[test]
fn raw_values_describe_themselves_briefly() {
    assert_eq!(RawInput::Float(1.5).describe(), "the number 1.5");
    assert_eq!(RawInput::List(vec![]).describe(), "a list of 0");
    assert_eq!(
        RawInput::Record(BTreeMap::new()).describe(),
        "a table with 0 field(s)"
    );
    let long = RawInput::Text("x".repeat(50)).describe();
    assert!(long.ends_with("...\""), "{long}");
}

#[test]
fn raw_inputs_decode_from_json_by_shape() {
    let v: RawInput = serde_json::from_str(r#"{"a": [1, 2.5, "t", true]}"#).unwrap();
    assert_eq!(
        v,
        RawInput::Record(raw(&[(
            "a",
            RawInput::List(vec![
                RawInput::Int(1),
                RawInput::Float(2.5),
                RawInput::Text("t".into()),
                RawInput::Bool(true)
            ])
        )]))
    );
}

#[test]
fn declarations_and_values_round_trip_through_every_codec() {
    let mut d = decl(
        "topic",
        InputType::List {
            item: Box::new(InputType::Choice {
                options: vec![ChoiceName::new("a").unwrap()],
            }),
            min: Some(1),
            max: None,
        },
    );
    d.default = Some(InputValue::List(vec![InputValue::Choice(
        ChoiceName::new("a").unwrap(),
    )]));
    d.binds = vec![
        InputSlot::Region(RegionBinding {
            region: RegionName::new("task").unwrap(),
            template: Some(Template::parse("Topics:\n{topic}").unwrap()),
        }),
        InputSlot::StageModel(StageName::new("plan").unwrap()),
        InputSlot::OutputFormat,
    ];
    let json = serde_json::to_string(&d).unwrap();
    assert_eq!(serde_json::from_str::<InputDecl>(&json).unwrap(), d);
    let bin = postcard::to_stdvec(&d).unwrap();
    assert_eq!(postcard::from_bytes::<InputDecl>(&bin).unwrap(), d);
    let toml_text = toml::to_string(&d).unwrap();
    assert_eq!(toml::from_str::<InputDecl>(&toml_text).unwrap(), d);
}
