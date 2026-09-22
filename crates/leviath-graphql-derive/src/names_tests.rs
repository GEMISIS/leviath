//! The suffix legend, one name at a time.

use super::{Names, Shape, camel, ident, names, pascal, screaming, snake};

/// Every name one object type produces.
#[test]
fn an_object_produces_the_whole_legend() {
    let names = names(&ident("Region"), Shape::Object, None).expect("a plain name");
    assert_eq!(
        names,
        Names {
            target: "RegionOutput".to_owned(),
            filter: ident("RegionFilter"),
            filter_gql: "RegionInput".to_owned(),
            list: ident("RegionListFilter"),
            list_gql: "RegionListInput".to_owned(),
            order: ident("RegionOrderField"),
            order_gql: "RegionOrderField".to_owned(),
            order_input: ident("RegionOrder"),
            order_input_gql: "RegionOrder".to_owned(),
        }
    );
}

/// An enum's comparator says `Filter` on both sides, because it compares a
/// value rather than mirroring an object.
#[test]
fn an_enum_compares_rather_than_mirrors() {
    let names = names(&ident("RegionKind"), Shape::Enumeration, None).expect("a plain name");
    assert_eq!(names.target, "RegionKind");
    assert_eq!(names.filter_gql, "RegionKindFilter");
    assert_eq!(names.list_gql, "RegionKindListFilter");
}

/// A union mirrors, so its input reads as one.
#[test]
fn a_union_mirrors_its_variants() {
    let names = names(&ident("RegionSeed"), Shape::Union, None).expect("a plain name");
    assert_eq!(names.target, "RegionSeed");
    assert_eq!(names.filter_gql, "RegionSeedInput");
    assert_eq!(names.list_gql, "RegionSeedListInput");
}

/// A name the source already gave is kept, and the mirror follows it.
#[test]
fn an_explicit_name_is_followed() {
    let names = names(&ident("Run"), Shape::Object, Some("RunOutput")).expect("the right suffix");
    assert_eq!(names.target, "RunOutput");
    assert_eq!(names.filter_gql, "RunInput");
    assert_eq!(
        names.filter,
        ident("RunFilter"),
        "the Rust name is the type's"
    );
}

/// A hand-written name that breaks the legend is refused.
#[test]
fn an_explicit_name_without_the_suffix_is_refused() {
    let problem = names(&ident("Run"), Shape::Object, Some("Run")).expect_err("no suffix");
    assert!(
        problem.to_string().contains("does not end in `Output`"),
        "{problem}"
    );
}

/// A type whose name says it is something else is not mirrored at all.
#[test]
fn the_types_that_are_never_filtered_say_so_in_their_names() {
    for name in ["RunConnection", "SpawnRunResult", "RunSpawnedEvent"] {
        let problem = names(&ident(name), Shape::Object, None).expect_err("out of scope");
        assert!(
            problem.to_string().contains("not a type a filter mirrors"),
            "{problem}"
        );
    }
}

/// Field names camel-case the way async-graphql does.
#[test]
fn a_field_name_is_camel_cased() {
    assert_eq!(camel("name"), "name");
    assert_eq!(camel("max_tokens"), "maxTokens");
    assert_eq!(camel("a_b_c"), "aBC");
    assert_eq!(camel("trailing_"), "trailing");
}

/// A type name becomes a field name, and back again.
#[test]
fn a_type_name_becomes_a_field_name() {
    assert_eq!(snake("SeedFromCaller"), "seed_from_caller");
    assert_eq!(snake("Tag"), "tag");
    assert_eq!(pascal("seed_from_caller"), "SeedFromCaller");
    assert_eq!(pascal("at"), "At");
    assert_eq!(pascal(""), "");
    assert_eq!(screaming("PerItem"), "PER_ITEM");
}
