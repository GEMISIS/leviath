use leviath_runtime::spec::graph::{Budget, RegionDef, RegionKind};
use leviath_runtime::spec::names::RegionName;

use crate::BlueprintFile;

/// A graph with a shared layout of three regions and two stages, with `extra`
/// written into the `plan` stage's table.
fn file_with(extra: &str) -> String {
    format!(
        r#"[blueprint]
name = "adder"
version = "1.0.0"

[graph.layout]
total_budget_tokens = 0
eviction_order = ["notes"]

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 2000

[[graph.layout.regions]]
name = "notes"
kind = "clearable"
budget = "10%"

[[graph.layout.regions]]
name = "sources"
kind = "clearable"
budget = "20%"

[[graph.stages]]
name = "plan"
system_prompt = "Plan."
{extra}

[[graph.stages]]
name = "build"
system_prompt = "Build."
"#
    )
}

fn names(regions: &[RegionDef]) -> Vec<&str> {
    regions.iter().map(|r| r.name.as_str()).collect()
}

fn parse(text: &str) -> BlueprintFile {
    BlueprintFile::parse(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

/// The layout the `plan` stage runs with.
fn plan_regions(file: &BlueprintFile) -> Vec<RegionDef> {
    let graph = file.run_graph();
    graph.layout_for(&graph.stages[0]).regions.clone()
}

#[test]
fn a_stage_inherits_the_shared_layout_and_adds_a_region_inline() {
    let file = parse(&file_with(
        r#"add = { scratch = { kind = "pinned", budget = "10%" } }"#,
    ));
    let regions = plan_regions(&file);
    assert_eq!(names(&regions), ["task", "notes", "sources", "scratch"]);
    assert_eq!(regions[3].kind, RegionKind::Pinned);
    assert_eq!(
        regions[3].budget,
        Budget::Percent {
            percent: 0.1,
            min: None,
            max: None
        }
    );
    // The stage's layout keeps the shared budget and eviction order.
    let layout = file.graph.stages[0].layout.as_ref().unwrap();
    assert_eq!(layout.total_budget_tokens, 0);
    assert_eq!(layout.eviction_order, [RegionName::new("notes").unwrap()]);
    // The other stage is untouched.
    assert!(file.graph.stages[1].layout.is_none());
    assert_eq!(
        names(&file.graph.layout.regions),
        ["task", "notes", "sources"]
    );
}

#[test]
fn a_stage_adds_regions_in_the_table_form_in_the_order_written() {
    let file = parse(&file_with(
        "[graph.stages.add.scratch]\nkind = \"pinned\"\nbudget = \"10%\"\n\n\
         [graph.stages.add.draft]\nkind = \"temporary\"\nbudget = 500\n",
    ));
    let regions = plan_regions(&file);
    assert_eq!(
        names(&regions),
        ["task", "notes", "sources", "scratch", "draft"]
    );
    assert_eq!(regions[4].budget, Budget::Tokens(500));
    // The second stage keeps its own table: the `add` tables belong to `plan`.
    assert_eq!(file.graph.stages[1].name.as_str(), "build");
    assert!(file.graph.stages[1].layout.is_none());
}

#[test]
fn add_and_hide_compose() {
    let file = parse(&file_with(
        "hide = [\"sources\"]\nadd = { scratch = { kind = \"pinned\", budget = 100 } }",
    ));
    let stage = &file.graph.stages[0];
    assert_eq!(
        names(&plan_regions(&file)),
        ["task", "notes", "sources", "scratch"]
    );
    // `sources` is still hidden: kept in the layout, left out of the prompt.
    assert_eq!(stage.hide, [RegionName::new("sources").unwrap()]);
}

#[test]
fn hiding_a_shared_region_and_adding_it_again_overrides_it_for_the_stage() {
    let file = parse(&file_with(
        "hide = [\"notes\"]\nadd = { notes = { kind = \"pinned\", budget = 4000 } }",
    ));
    let regions = plan_regions(&file);
    // In the shared region's place, with the stage's definition.
    assert_eq!(names(&regions), ["task", "notes", "sources"]);
    assert_eq!(regions[1].kind, RegionKind::Pinned);
    assert_eq!(regions[1].budget, Budget::Tokens(4000));
    // And shown: the override is not hidden by the `hide` that made it.
    assert!(file.graph.stages[0].hide.is_empty());
    // The shared definition is unchanged for every other stage.
    assert_eq!(file.graph.layout.regions[1].kind, RegionKind::Clearable);
}

#[test]
fn the_merged_layout_validates_like_a_declared_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.toml");
    std::fs::write(
        &path,
        file_with(r#"add = { scratch = { kind = "pinned", budget = "150%" } }"#),
    )
    .unwrap();
    let err = crate::validate(&path).unwrap_err().to_string();
    assert!(
        err.contains("graph.stages.plan.layout.regions[3].budget"),
        "{err}"
    );
    assert!(
        err.contains("not a fraction above 0 and at most 1"),
        "{err}"
    );
}

#[test]
fn add_beside_layout_is_refused() {
    let err = BlueprintFile::parse(&file_with(
        "add = { scratch = { kind = \"pinned\", budget = 100 } }\n\
         layout = { total_budget_tokens = 0, regions = [{ name = \"x\", kind = \"pinned\", budget = 1 }] }",
    ))
    .unwrap_err();
    assert!(err.contains("stage \"plan\""), "{err}");
    assert!(err.contains("both `layout` and `add`"), "{err}");
    assert!(err.contains("whole layout"), "{err}");
}

#[test]
fn adding_a_shared_region_without_hiding_it_is_refused() {
    let err = BlueprintFile::parse(&file_with(
        r#"add = { notes = { kind = "pinned", budget = 100 } }"#,
    ))
    .unwrap_err();
    assert!(
        err.contains("stage \"plan\" adds region \"notes\""),
        "{err}"
    );
    assert!(err.contains("`hide`"), "{err}");
}

#[test]
fn adding_an_always_visible_region_is_refused() {
    for name in leviath_runtime::spec::graph::ALWAYS_VISIBLE_REGIONS {
        let err = BlueprintFile::parse(&file_with(&format!(
            "add = {{ {name} = {{ kind = \"pinned\", budget = 100 }} }}"
        )))
        .unwrap_err();
        assert!(
            err.contains(&format!("cannot add region \"{name}\"")),
            "{err}"
        );
    }
}

#[test]
fn an_added_region_takes_its_name_from_its_key() {
    let err = BlueprintFile::parse(&file_with(
        r#"add = { scratch = { name = "other", kind = "pinned", budget = 100 } }"#,
    ))
    .unwrap_err();
    assert!(err.contains("add.scratch"), "{err}");
    assert!(err.contains("`name`"), "{err}");
}

#[test]
fn a_bad_added_region_is_refused_with_its_line() {
    let err = BlueprintFile::parse(&file_with(
        r#"add = { scratch = { kind = "pinned", budgte = 100 } }"#,
    ))
    .unwrap_err();
    assert!(err.contains("unknown field `budgte`"), "{err}");
    assert!(err.contains("line 27"), "{err}");
}

#[test]
fn add_must_be_a_table_of_regions() {
    let err = BlueprintFile::parse(&file_with(r#"add = ["scratch"]"#)).unwrap_err();
    assert!(err.contains("stage \"plan\""), "{err}");
    assert!(err.contains("keyed by name"), "{err}");
}

#[test]
fn other_errors_keep_their_line() {
    let err = BlueprintFile::parse(&file_with(
        "add = { scratch = { kind = \"pinned\", budget = 100 } }\nmax_iteratons = 3",
    ))
    .unwrap_err();
    assert!(err.contains("unknown field `max_iteratons`"), "{err}");
    assert!(err.contains("line 28"), "{err}");
}

#[test]
fn an_added_region_that_is_not_a_table_is_refused_by_the_region_reader() {
    let err = BlueprintFile::parse(&file_with("add = { scratch = 5 }")).unwrap_err();
    assert!(err.contains("invalid type"), "{err}");
    assert!(err.contains("line 27"), "{err}");
}

#[test]
fn an_unnamed_stage_is_named_as_such() {
    let text = file_with("").replace(
        "name = \"plan\"\nsystem_prompt = \"Plan.\"",
        "system_prompt = \"Plan.\"\nadd = 3",
    );
    let err = BlueprintFile::parse(&text).unwrap_err();
    assert!(err.contains("stage \"(unnamed)\""), "{err}");
}

#[test]
fn a_file_of_the_wrong_shape_reads_as_it_did_without_add() {
    // No graph, a graph that is not a table, stages that are not a list and a
    // stage that is not a table: each fails as the graph reader says, not
    // on the way to `add`.
    for (text, says) in [
        (
            "[blueprint]\nname = \"a\"\nversion = \"1\"\n",
            "missing field `graph`",
        ),
        ("graph = 1\n", "invalid type"),
        ("[graph]\nstages = 1\n", "invalid type"),
        ("[graph]\nstages = [1]\n", "invalid type"),
    ] {
        let err = BlueprintFile::parse(text).unwrap_err();
        assert!(err.contains(says), "{text}: {err}");
    }
    let bad = BlueprintFile::parse("[graph\n").unwrap_err();
    assert!(bad.contains("line 1"), "{bad}");
}

#[test]
fn merging_keeps_the_shared_order_and_appends_the_rest() {
    let merged = crate::merge_regions(
        &[("a", 1), ("b", 1), ("c", 1)],
        vec![("d", 2), ("b", 2)],
        |r| r.0,
    );
    assert_eq!(merged, [("a", 1), ("b", 2), ("c", 1), ("d", 2)]);
}
