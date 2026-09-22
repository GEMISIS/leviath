//! The macro and this runtime, together, on types written for the purpose.
//!
//! Everything else in this module is tested against hand-written impls. This
//! file is the one that says the two halves fit: small types carrying every
//! shape the schema has, put through the real `#[mirror]`, registered in a
//! real schema, and asked real questions.

use std::sync::atomic::{AtomicUsize, Ordering as Counting};

use async_graphql::{EmptyMutation, EmptySubscription, Enum, Object, Schema, SimpleObject, Union};
use leviath_graphql_derive::mirror;

use super::testkit::{exercise, exercise_enum, exercise_list, exercise_order};
use super::{Filterable, MatchCx, OrderField, Orderable, Tri};

/// How many times a second-phase field has been read.
///
/// The whole point of the cheap pass is that a value it refuses is never read,
/// so the count is the assertion.
static READS: AtomicUsize = AtomicUsize::new(0);

/// What a part is made of.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum Material {
    /// Steel.
    Steel,
    /// Brass.
    Brass,
}

/// A tag somebody stuck on a gadget.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct Tag {
    /// What the tag says.
    pub(crate) text: String,
    /// What it is made of.
    pub(crate) material: Material,
}

/// A serial number stamped on a gadget.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct Serial {
    /// The number itself.
    pub(crate) number: i32,
}

/// How a gadget was labelled.
#[mirror]
#[derive(Debug, Union)]
pub(crate) enum Marking {
    /// A tag tied to it.
    Tag(Tag),
    /// A number stamped on it.
    Serial(Serial),
}

/// One gadget, which may hold another.
#[derive(Debug, Clone)]
pub(crate) struct Gadget {
    /// What it is called.
    pub(crate) name: String,
    /// How heavy it is.
    pub(crate) weight: i32,
    /// What is inside it, if anything is.
    pub(crate) inner: Option<Box<Gadget>>,
    /// What it says, read from somewhere slow.
    pub(crate) engraving: Option<String>,
}

/// Who owns a gadget, read through an accessor rather than a resolver.
pub(crate) fn owner_of(gadget: &Gadget, _cx: &MatchCx<'_>) -> Option<String> {
    gadget.name.split('-').next().map(str::to_owned)
}

/// Read a gadget's engraving, counting the read.
async fn read_engraving(gadget: &Gadget) -> Option<String> {
    READS.fetch_add(1, Counting::SeqCst);
    std::future::ready(()).await;
    gadget.engraving.clone()
}

#[mirror(list)]
#[Object]
impl Gadget {
    /// What the gadget is called.
    async fn name(&self) -> &str {
        &self.name
    }

    /// How heavy the gadget is, in grams.
    #[filter(orderable)]
    async fn weight(&self) -> i32 {
        self.weight
    }

    /// What is inside this gadget.
    async fn inner(&self) -> Option<Gadget> {
        self.inner.as_deref().cloned()
    }

    /// Every tag tied to this gadget.
    async fn tags(&self) -> Vec<Tag> {
        vec![Tag {
            text: self.name.clone(),
            material: Material::Steel,
        }]
    }

    /// What is engraved on the gadget.
    #[filter(io)]
    async fn engraving(&self) -> Option<String> {
        read_engraving(self).await
    }

    /// Who owns the gadget.
    #[filter(with = "super::mirror_e2e_tests::owner_of")]
    async fn owner(&self, _ctx: &async_graphql::Context<'_>) -> Option<String> {
        owner_of(self, &MatchCx::at(0))
    }

    /// The first few letters of the name, which takes an argument.
    #[filter(skip)]
    async fn head(&self, letters: i32) -> String {
        self.name.chars().take(letters.max(0) as usize).collect()
    }
}

/// The root the mirrored inputs are registered through.
struct Query;

#[Object]
impl Query {
    /// Gadgets matching a filter.
    async fn gadgets(&self, filter: Option<GadgetFilter>) -> Vec<Gadget> {
        let _ = filter;
        Vec::new()
    }

    /// Gadgets quantified over as a list.
    async fn gadget_lists(&self, filter: Option<GadgetListFilter>) -> Vec<Gadget> {
        let _ = filter;
        Vec::new()
    }

    /// Tags matching a quantifier.
    async fn tags(&self, filter: Option<TagListFilter>) -> Vec<Tag> {
        let _ = filter;
        Vec::new()
    }

    /// Markings matching a variant filter.
    async fn markings(&self, filter: Option<MarkingFilter>) -> Vec<Marking> {
        let _ = filter;
        Vec::new()
    }

    /// Materials matching a comparator.
    async fn materials(&self, filter: Option<MaterialFilter>) -> Vec<Material> {
        let _ = filter;
        Vec::new()
    }
}

/// One gadget holding another, to test against.
fn sample() -> Gadget {
    Gadget {
        name: "lamp-one".to_owned(),
        weight: 40,
        inner: Some(Box::new(Gadget {
            name: "bulb-two".to_owned(),
            weight: 5,
            inner: None,
            engraving: None,
        })),
        engraving: Some("made here".to_owned()),
    }
}

/// A filter on nothing but the gadget's name.
fn named(name: &str) -> GadgetFilter {
    GadgetFilter {
        name: Some(Box::new(super::scalars::StringFilter {
            eq: Some(name.to_owned()),
            ..super::scalars::StringFilter::default()
        })),
        ..GadgetFilter::default()
    }
}

/// The schema names every generated type the way the suffix legend says.
#[test]
fn the_suffix_legend_is_what_the_schema_says() {
    let sdl = Schema::new(Query, EmptyMutation, EmptySubscription).sdl();
    for name in [
        "type GadgetOutput",
        "type TagOutput",
        "type SerialOutput",
        "input GadgetInput",
        "input GadgetListInput",
        "input TagInput",
        "input TagListInput",
        "input MarkingInput",
        "input MaterialFilter",
    ] {
        assert!(sdl.contains(name), "{name} is missing from\n{sdl}");
    }
    assert!(
        sdl.contains("\"\"\"\n\tFilter on `GadgetOutput.name`.\n\t\n\tWhat the gadget is called."),
        "a mirrored field carries the output field's own description:\n{sdl}"
    );
}

/// Every mirrored field is in the mirror, and the skipped one is not.
#[test]
fn the_mirror_has_one_field_per_mirrored_resolver() {
    let sdl = Schema::new(Query, EmptyMutation, EmptySubscription).sdl();
    let input = sdl
        .split("input GadgetInput {")
        .nth(1)
        .and_then(|rest| rest.split('}').next())
        .expect("the mirror is in the schema");
    for field in [
        "name: StringFilter",
        "weight: IntFilter",
        "inner: GadgetInput",
        "tags: TagListInput",
        "engraving: StringFilter",
        "owner: StringFilter",
        "and: [GadgetInput!]",
        "or: [GadgetInput!]",
        "not: GadgetInput",
        "isNull: Boolean",
    ] {
        assert!(input.contains(field), "{field} is missing from\n{input}");
    }
    assert!(
        !input.contains("head"),
        "a resolver with arguments stays out of the mirror:\n{input}"
    );
    assert!(
        sdl.contains("head(letters: Int!): String!"),
        "a skipped resolver stays on the type it was written on:\n{sdl}"
    );
}

/// A cheap field is answered before anything is read, and the answer stands.
#[tokio::test]
async fn a_cheap_field_needs_no_read() {
    let cx = MatchCx::at(0);
    let gadget = sample();
    let before = READS.load(Counting::SeqCst);
    assert_eq!(gadget.test(&named("lamp-one"), &cx), Tri::Yes);
    assert_eq!(gadget.test(&named("other"), &cx), Tri::No);
    assert_eq!(
        READS.load(Counting::SeqCst),
        before,
        "the cheap pass opens nothing"
    );
    assert!(gadget.confirm(&named("lamp-one"), &cx).await);
    assert!(!gadget.confirm(&named("other"), &cx).await);
}

/// A field that costs a read is undecided until the second phase runs.
#[tokio::test]
async fn a_read_is_deferred_and_then_answered() {
    let cx = MatchCx::at(0);
    let gadget = sample();
    let filter = GadgetFilter {
        engraving: Some(Box::new(super::scalars::StringFilter {
            eq: Some("made here".to_owned()),
            ..super::scalars::StringFilter::default()
        })),
        ..GadgetFilter::default()
    };
    assert_eq!(gadget.test(&filter, &cx), Tri::Io);
    let before = READS.load(Counting::SeqCst);
    assert!(gadget.confirm(&filter, &cx).await);
    assert_eq!(
        READS.load(Counting::SeqCst),
        before + 1,
        "the second phase reads once"
    );
}

/// A cheap refusal beside a read-costing field never reaches the read.
#[tokio::test]
async fn a_cheap_refusal_stops_the_read() {
    let cx = MatchCx::at(0);
    let gadget = sample();
    let filter = GadgetFilter {
        engraving: Some(Box::new(super::scalars::StringFilter {
            eq: Some("made here".to_owned()),
            ..super::scalars::StringFilter::default()
        })),
        ..named("other")
    };
    assert_eq!(gadget.test(&filter, &cx), Tri::No);
    let before = READS.load(Counting::SeqCst);
    assert!(!gadget.confirm(&filter, &cx).await);
    assert_eq!(
        READS.load(Counting::SeqCst),
        before,
        "a field already refused is not read"
    );
}

/// A type that holds itself filters through itself.
#[tokio::test]
async fn a_recursive_field_filters_through_itself() {
    let cx = MatchCx::at(0);
    let gadget = sample();
    let filter = GadgetFilter {
        inner: Some(Box::new(named("bulb-two"))),
        ..GadgetFilter::default()
    };
    assert_eq!(gadget.test(&filter, &cx), Tri::Yes);
    assert!(gadget.confirm(&filter, &cx).await);

    let missing = GadgetFilter {
        inner: Some(Box::new(GadgetFilter {
            is_null: Some(true),
            ..GadgetFilter::default()
        })),
        ..GadgetFilter::default()
    };
    assert_eq!(gadget.test(&missing, &cx), Tri::No);
    assert_eq!(
        gadget
            .inner
            .as_deref()
            .expect("the sample holds one")
            .test(&missing, &cx),
        Tri::Yes,
        "the gadget with nothing inside is the one `isNull` matches"
    );
}

/// A list field is quantified over its items.
#[tokio::test]
async fn a_list_field_is_quantified() {
    let cx = MatchCx::at(0);
    let gadget = sample();
    let tagged = |text: &str| GadgetFilter {
        tags: Some(Box::new(TagListFilter {
            some: Some(Box::new(TagFilter {
                text: Some(Box::new(super::scalars::StringFilter {
                    eq: Some(text.to_owned()),
                    ..super::scalars::StringFilter::default()
                })),
                ..TagFilter::default()
            })),
            ..TagListFilter::default()
        })),
        ..GadgetFilter::default()
    };
    assert_eq!(gadget.test(&tagged("lamp-one"), &cx), Tri::Yes);
    assert_eq!(gadget.test(&tagged("nothing"), &cx), Tri::No);
    assert!(gadget.confirm(&tagged("lamp-one"), &cx).await);
}

/// A union filter tests the variant as well as its contents.
#[test]
fn a_union_filter_refuses_another_variant() {
    let cx = MatchCx::at(0);
    let marking = Marking::Serial(Serial { number: 12 });
    let wants_a_tag = MarkingFilter {
        tag: Some(Box::new(TagFilter::default())),
        ..MarkingFilter::default()
    };
    assert_eq!(marking.test(&wants_a_tag, &cx), Tri::No);

    let wants_a_serial = MarkingFilter {
        serial: Some(Box::new(SerialFilter {
            number: Some(Box::new(super::scalars::IntFilter {
                eq: Some(12),
                ..super::scalars::IntFilter::default()
            })),
            ..SerialFilter::default()
        })),
        ..MarkingFilter::default()
    };
    assert_eq!(marking.test(&wants_a_serial, &cx), Tri::Yes);
}

/// An enum comparator compares by value.
#[test]
fn an_enum_comparator_compares_by_value() {
    let cx = MatchCx::at(0);
    let filter = MaterialFilter {
        within: Some(vec![Material::Brass]),
        ..MaterialFilter::default()
    };
    assert_eq!(Material::Brass.test(&filter, &cx), Tri::Yes);
    assert_eq!(Material::Steel.test(&filter, &cx), Tri::No);
}

/// An orderable field becomes a sort key under the Rust field's own name.
#[test]
fn an_orderable_field_becomes_a_sort_key() {
    let cx = MatchCx::at(0);
    assert_eq!(GadgetOrderField::ALL, &[GadgetOrderField::Weight]);
    assert_eq!(GadgetOrderField::Weight.wire(), "weight");
    assert_eq!(
        sample().key(GadgetOrderField::Weight, &cx),
        super::CursorKey::Int(40)
    );
}

/// The combinators compose filters of the same type, in both phases.
#[tokio::test]
async fn the_combinators_compose() {
    let cx = MatchCx::at(0);
    let gadget = sample();
    let filter = GadgetFilter {
        and: Some(vec![named("lamp-one")]),
        or: Some(vec![named("other"), named("lamp-one")]),
        not: Some(Box::new(named("third"))),
        ..GadgetFilter::default()
    };
    assert_eq!(gadget.test(&filter, &cx), Tri::Yes);
    assert!(gadget.confirm(&filter, &cx).await);
}

/// One call runs every function the macro wrote for each of these types.
#[tokio::test]
async fn every_generated_function_runs() {
    let gadget = sample();
    let inner = gadget
        .inner
        .as_deref()
        .expect("the sample holds one")
        .clone();
    exercise(&[gadget.clone(), inner]).await;
    exercise_list(&[gadget]).await;
    exercise_order(&sample(), GadgetOrderField::ALL);

    let tags = vec![Tag {
        text: "one".to_owned(),
        material: Material::Brass,
    }];
    exercise(&tags).await;
    exercise_list(&tags).await;
    exercise(&[Serial { number: 1 }]).await;
    exercise(&[
        Marking::Tag(Tag {
            text: "one".to_owned(),
            material: Material::Steel,
        }),
        Marking::Serial(Serial { number: 2 }),
    ])
    .await;
    exercise_enum(&[Material::Steel, Material::Brass]).await;
}

/// A filter written the way a client writes it reaches the same answer.
#[tokio::test]
async fn a_filter_off_the_wire_is_the_same_filter() {
    let schema = Schema::new(Query, EmptyMutation, EmptySubscription);
    let answer = schema
        .execute(
            r#"{
                gadgets(filter: {
                    name: { startsWith: "lamp" }
                    weight: { gte: 10, lt: 100 }
                    inner: { name: { eq: "bulb-two" } }
                    tags: { some: { material: { in: [STEEL] } } }
                    engraving: { isNull: false }
                    or: [{ name: { eq: "other" } }, { name: { eq: "lamp-one" } }]
                    not: { name: { eq: "third" } }
                }) { name }
                markings(filter: { serial: { number: { eq: 12 } } }) { __typename }
                materials(filter: { eq: STEEL })
                gadgetLists(filter: { every: { name: { eq: "lamp-one" } } }) { name }
            }"#,
        )
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
}
