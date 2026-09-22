//! The macro on all four shapes, expanded by the compiler rather than by a
//! unit test.
//!
//! The unit tests beside the source compare token streams, which says what the
//! macro writes. This says that what it writes compiles, registers a schema,
//! and answers a filter, against a runtime that is not the real one: the only
//! thing the two have in common is the contract.

mod rt;

use async_graphql::{EmptyMutation, EmptySubscription, Enum, Object, Schema, SimpleObject, Union};
use leviath_graphql_derive::mirror;

use rt::{Filterable, ListItem, MatchCx, Mirror, OrderField, Orderable, Tri};

/// How sweet something is.
#[mirror(rt = "crate::rt")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum Flavour {
    /// Sweet.
    Sweet,
    /// Sour.
    Sour,
}

/// A note somebody left.
#[mirror(list, rt = "crate::rt")]
#[derive(Debug, SimpleObject)]
pub(crate) struct Note {
    /// What the note says.
    pub(crate) text: String,
    /// How it tastes.
    pub(crate) flavour: Flavour,
}

/// A label somebody stuck on.
#[mirror(rt = "crate::rt")]
#[derive(Debug, SimpleObject)]
pub(crate) struct Label {
    /// What the label says.
    pub(crate) text: String,
}

/// What filled something in.
#[mirror(rt = "crate::rt")]
#[derive(Debug, Union)]
pub(crate) enum Source {
    /// A note.
    Note(Note),
    /// A label.
    Label(Label),
}

/// One stage of a run.
#[derive(Debug, Clone)]
pub(crate) struct Stage {
    /// The stage's name.
    pub(crate) name: String,
    /// Where it sits in the run.
    pub(crate) at: i32,
    /// What it wrote, which is read from somewhere slow.
    pub(crate) log: Option<String>,
}

/// One region of a stage's context.
#[derive(Debug, Clone)]
pub(crate) struct Region {
    /// The region's name.
    pub(crate) name: String,
    /// The stage that declared it, if a stage did.
    pub(crate) stage: Option<Stage>,
}

/// One page of a stage's regions, as a resolver shapes them for a client.
#[derive(Debug, SimpleObject)]
pub(crate) struct RegionPage {
    /// The regions this page holds.
    pub(crate) results: Vec<Region>,
}

/// Who owns a stage, read through an accessor of the caller's.
pub(crate) fn owner_of(stage: &Stage, cx: &MatchCx<'_>) -> Option<String> {
    Some(cx.named.unwrap_or(&stage.name).to_owned())
}

/// Every region a stage declares, read the slow way.
///
/// What the paged resolver pages, which is what its mirror compares: the page
/// is a shape for the client, and a filter asks about the regions themselves.
pub(crate) async fn regions_of(stage: &Stage, _cx: &MatchCx<'_>) -> Vec<Region> {
    std::future::ready(()).await;
    regions_declared(stage)
}

/// The regions one stage declares.
fn regions_declared(stage: &Stage) -> Vec<Region> {
    vec![Region {
        name: format!("{}-region", stage.name),
        stage: None,
    }]
}

/// Read a stage's log, slowly enough that it is a second-phase field.
async fn read_log(stage: &Stage) -> Option<String> {
    std::future::ready(()).await;
    stage.log.clone()
}

#[mirror(list, rt = "crate::rt")]
#[Object]
impl Stage {
    /// The stage's name, unique within the run.
    async fn name(&self) -> &str {
        &self.name
    }

    /// Where the stage sits in the run, counting from zero.
    #[filter(orderable)]
    async fn at(&self) -> i32 {
        self.at
    }

    /// The regions this stage declares.
    async fn regions(&self) -> Vec<Region> {
        regions_declared(self)
    }

    /// One page of the regions this stage declares.
    ///
    /// The answer is a page, and a filter on it is about the regions, so the
    /// mirror reads them through an accessor of its own.
    #[filter(io, with = "crate::regions_of", ty = "Vec<Region>")]
    async fn region_page(&self, _first: i32) -> RegionPage {
        RegionPage {
            results: regions_declared(self),
        }
    }

    /// What the stage wrote.
    #[filter(io)]
    async fn log(&self) -> Option<String> {
        read_log(self).await
    }

    /// Who owns the stage.
    #[filter(with = "crate::owner_of")]
    async fn owner(&self, _ctx: &async_graphql::Context<'_>) -> Option<String> {
        Some(self.name.clone())
    }

    /// Part of what the stage wrote, which takes an argument.
    #[filter(skip)]
    async fn head(&self, bytes: i32) -> String {
        self.name.chars().take(bytes.max(0) as usize).collect()
    }
}

#[mirror(list, rt = "crate::rt")]
#[Object]
impl Region {
    /// The region's name.
    async fn name(&self) -> &str {
        &self.name
    }

    /// The stage that declared this region.
    async fn stage(&self) -> Option<Stage> {
        self.stage.clone()
    }
}

/// The root of the schema the expansion is registered in.
pub(crate) struct Query;

#[Object]
impl Query {
    /// Stages matching a filter.
    async fn stages(&self, filter: Option<StageFilter>) -> Vec<Stage> {
        let cx = MatchCx::at(0);
        let stage = sample();
        match filter {
            None => vec![stage],
            Some(filter) => match stage.confirm(&filter, &cx).await {
                true => vec![stage],
                false => Vec::new(),
            },
        }
    }

    /// Stages quantified over as a list.
    async fn stage_lists(&self, filter: Option<StageListFilter>) -> Vec<Stage> {
        let _ = filter;
        Vec::new()
    }

    /// Regions matching a filter, which is the other half of the cycle.
    async fn regions(&self, filter: Option<RegionFilter>) -> Vec<Region> {
        let _ = filter;
        Vec::new()
    }

    /// Notes matching a quantifier.
    async fn notes(&self, filter: Option<NoteListFilter>) -> Vec<Note> {
        let _ = filter;
        Vec::new()
    }

    /// What filled something in.
    async fn sources(&self, filter: Option<SourceFilter>) -> Vec<Source> {
        let _ = filter;
        Vec::new()
    }

    /// Notes of one flavour.
    async fn flavours(&self, filter: Option<FlavourFilter>) -> Vec<Flavour> {
        let _ = filter;
        Vec::new()
    }

    /// The reads a run recorded, which nothing filters on.
    async fn reads(&self) -> Vec<ReadFileCall> {
        Vec::new()
    }

    /// The writes a run recorded.
    async fn writes(&self) -> Vec<WriteFileCall> {
        Vec::new()
    }
}

/// One stage to test against.
fn sample() -> Stage {
    Stage {
        name: "plan".to_owned(),
        at: 0,
        log: Some("wrote something".to_owned()),
    }
}

/// The four shapes register under the names the suffix legend gives them.
#[test]
fn the_schema_says_what_each_type_is_for() {
    assert_eq!(
        MatchCx::at(7).now,
        7,
        "the context carries the request's clock"
    );
    let sdl = Schema::new(Query, EmptyMutation, EmptySubscription).sdl();
    println!("{sdl}");
    for name in [
        "type StageOutput",
        "type RegionOutput",
        "type NoteOutput",
        "input StageInput",
        "input StageListInput",
        "input RegionInput",
        "input NoteListInput",
        "input SourceInput",
        "input FlavourFilter",
    ] {
        assert!(sdl.contains(name), "{name} is missing from\n{sdl}");
    }
    assert!(
        sdl.contains("\"\"\"\n\tFilter on `StageOutput.name`."),
        "a mirrored field keeps the output field's description:\n{sdl}"
    );
    let input = sdl
        .split("input StageInput {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("the mirror is in the schema");
    assert!(
        sdl.contains("head(bytes: Int!): String!"),
        "a skipped resolver stays in the type it was written on:\n{sdl}"
    );
    assert!(
        !input.contains("head"),
        "a skipped resolver is not in the mirror:\n{input}"
    );
    for field in [
        "name:",
        "at:",
        "regions:",
        "log:",
        "owner:",
        "regionPage: RegionListInput",
        "and:",
        "isNull:",
    ] {
        assert!(input.contains(field), "{field} is missing from\n{input}");
    }
    assert!(
        sdl.contains("regionPage(first: Int!): RegionPage!"),
        "the resolver still answers with the page it shapes:\n{sdl}"
    );
}

/// A field the mirror reads as another type is answered in the second phase,
/// through the accessor rather than through the resolver.
#[tokio::test]
async fn a_reshaped_field_is_read_through_its_own_accessor() {
    let cx = MatchCx::at(0);
    let stage = sample();
    let matching = StageFilter {
        region_page: Some(Box::new(RegionListFilter {
            some: Some(Box::new(RegionFilter {
                name: Some(Box::new(rt::StringFilter {
                    eq: Some("plan-region".to_owned()),
                    is_null: None,
                })),
                ..RegionFilter::default()
            })),
            ..RegionListFilter::default()
        })),
        ..StageFilter::default()
    };
    assert_eq!(
        stage.test(&matching, &cx),
        Tri::Io,
        "the page is not answered from memory"
    );
    assert!(stage.confirm(&matching, &cx).await);

    let missing = StageFilter {
        region_page: Some(Box::new(RegionListFilter {
            none: Some(Box::new(RegionFilter {
                name: Some(Box::new(rt::StringFilter {
                    eq: Some("plan-region".to_owned()),
                    is_null: None,
                })),
                ..RegionFilter::default()
            })),
            ..RegionListFilter::default()
        })),
        ..StageFilter::default()
    };
    assert!(!stage.confirm(&missing, &cx).await);
}

/// A cheap field answers without the second phase, and the two agree.
#[tokio::test]
async fn a_cheap_field_is_answered_before_anything_is_read() {
    let cx = MatchCx::at(0);
    let stage = sample();
    let filter = StageFilter {
        name: Some(Box::new(rt::StringFilter {
            eq: Some("plan".to_owned()),
            is_null: None,
        })),
        ..StageFilter::default()
    };
    assert_eq!(stage.test(&filter, &cx), Tri::Yes);
    assert!(stage.confirm(&filter, &cx).await);
}

/// A field that costs a read is undecided until the second phase runs.
#[tokio::test]
async fn a_read_is_deferred_to_the_second_phase() {
    let cx = MatchCx::at(0);
    let stage = sample();
    let filter = StageFilter {
        log: Some(Box::new(rt::StringFilter {
            eq: Some("wrote something".to_owned()),
            is_null: None,
        })),
        ..StageFilter::default()
    };
    assert_eq!(stage.test(&filter, &cx), Tri::Io);
    assert!(stage.confirm(&filter, &cx).await);
}

/// A cheap refusal is final, even with a read-costing field beside it.
#[tokio::test]
async fn a_cheap_refusal_is_never_revisited() {
    let cx = MatchCx::at(0);
    let stage = sample();
    let filter = StageFilter {
        name: Some(Box::new(rt::StringFilter {
            eq: Some("other".to_owned()),
            is_null: None,
        })),
        log: Some(Box::new(rt::StringFilter {
            eq: Some("wrote something".to_owned()),
            is_null: None,
        })),
        ..StageFilter::default()
    };
    assert_eq!(stage.test(&filter, &cx), Tri::No);
    assert!(!stage.confirm(&filter, &cx).await);
}

/// The recursive pair compiles, and a nested filter reaches through it.
#[tokio::test]
async fn a_cycle_of_mirrors_resolves() {
    let cx = MatchCx::at(0);
    let region = Region {
        name: "notes".to_owned(),
        stage: Some(sample()),
    };
    let filter = RegionFilter {
        stage: Some(Box::new(StageFilter {
            name: Some(Box::new(rt::StringFilter {
                eq: Some("plan".to_owned()),
                is_null: None,
            })),
            ..StageFilter::default()
        })),
        ..RegionFilter::default()
    };
    assert_eq!(region.test(&filter, &cx), Tri::Yes);
    assert!(region.confirm(&filter, &cx).await);
}

/// A list field is quantified over its items.
#[test]
fn a_list_is_quantified() {
    let cx = MatchCx::at(0);
    let notes = vec![
        Note {
            text: "first".to_owned(),
            flavour: Flavour::Sweet,
        },
        Note {
            text: "second".to_owned(),
            flavour: Flavour::Sour,
        },
    ];
    let filter = NoteListFilter {
        some: Some(Box::new(NoteFilter {
            text: Some(Box::new(rt::StringFilter {
                eq: Some("second".to_owned()),
                is_null: None,
            })),
            ..NoteFilter::default()
        })),
        ..NoteListFilter::default()
    };
    assert_eq!(Note::list_test(&notes, &filter, &cx), Tri::Yes);
}

/// A union filter matches the variant it names and refuses the others.
#[test]
fn a_union_filter_is_a_test_of_the_variant() {
    let cx = MatchCx::at(0);
    let source = Source::Note(Note {
        text: "first".to_owned(),
        flavour: Flavour::Sweet,
    });
    let asks_for_a_label = SourceFilter {
        label: Some(Box::new(LabelFilter::default())),
        ..SourceFilter::default()
    };
    assert_eq!(source.test(&asks_for_a_label, &cx), Tri::No);

    let asks_for_a_note = SourceFilter {
        note: Some(Box::new(NoteFilter::default())),
        ..SourceFilter::default()
    };
    assert_eq!(source.test(&asks_for_a_note, &cx), Tri::Yes);
}

/// An enum compares by value.
#[test]
fn an_enum_compares_by_value() {
    let cx = MatchCx::at(0);
    let filter = FlavourFilter {
        eq: Some(Flavour::Sweet),
        ..FlavourFilter::default()
    };
    assert_eq!(Flavour::Sweet.test(&filter, &cx), Tri::Yes);
    assert_eq!(Flavour::Sour.test(&filter, &cx), Tri::No);
}

/// An orderable field becomes a sort key with the Rust field's own name.
#[test]
fn an_orderable_field_is_a_sort_key() {
    let cx = MatchCx::at(0);
    assert_eq!(StageOrderField::ALL, &[StageOrderField::At]);
    assert_eq!(StageOrderField::At.wire(), "at");
    assert_eq!(
        sample().key(StageOrderField::At, &cx),
        rt::CursorKey::Int(0)
    );
    // The order input the macro writes beside the enum compiles to the term a
    // walk compares on, so no listing spells one out.
    let term = StageOrder {
        field: StageOrderField::At,
        direction: rt::OrderDirection::Asc,
    }
    .term();
    assert_eq!(term.field, StageOrderField::At);
    assert_eq!(term.direction, rt::OrderDirection::Asc);
}

/// The combinators compose filters of the same type.
#[tokio::test]
async fn and_or_not_compose() {
    let cx = MatchCx::at(0);
    let stage = sample();
    let named = |name: &str| StageFilter {
        name: Some(Box::new(rt::StringFilter {
            eq: Some(name.to_owned()),
            is_null: None,
        })),
        ..StageFilter::default()
    };
    let filter = StageFilter {
        or: Some(vec![named("other"), named("plan")]),
        not: Some(Box::new(named("third"))),
        and: Some(vec![named("plan")]),
        ..StageFilter::default()
    };
    assert_eq!(stage.test(&filter, &cx), Tri::Yes);
    assert!(stage.confirm(&filter, &cx).await);
}

/// The parts a mirror hands over are the ones it was given.
#[test]
fn a_mirror_hands_over_what_it_carries() {
    let filter = StageFilter {
        is_null: Some(false),
        ..StageFilter::default()
    };
    let parts = filter.parts();
    assert_eq!(parts.is_null, Some(false));
    assert!(parts.and.is_none() && parts.or.is_none() && parts.not.is_none());
}

/// One tool call, named the way every output type is named and mirrored by
/// nothing, written the way the schema writes them: from a `macro_rules!`.
macro_rules! calls {
    ($($name:ident: $doc:expr,)+) => {
        $(
            #[doc = $doc]
            #[mirror(no_filter, rt = "crate::rt")]
            #[derive(Debug, SimpleObject)]
            pub(crate) struct $name {
                /// What it was called with.
                pub(crate) argument: String,
            }
        )+
    };
}

calls! {
    ReadFileCall: "Read one file.",
    WriteFileCall: "Write one file.",
}

/// A type outside the mirror still carries the suffix every output type has.
///
/// Written through a `macro_rules!` because that is how the tool calls are
/// written, and an attribute that only worked on hand-written items would not
/// reach them.
#[test]
fn a_type_outside_the_mirror_is_still_named_for_the_schema() {
    let sdl = Schema::new(Query, EmptyMutation, EmptySubscription).sdl();
    assert!(sdl.contains("type ReadFileCallOutput"), "{sdl}");
    assert!(sdl.contains("type WriteFileCallOutput"), "{sdl}");
    assert!(
        !sdl.contains("ReadFileCallInput"),
        "nothing filters on it:\n{sdl}"
    );
    assert!(
        sdl.contains("Read one file."),
        "the doc comment survives the expansion:\n{sdl}"
    );
}
