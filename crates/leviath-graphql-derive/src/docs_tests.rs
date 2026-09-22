//! The doc comments the generated types carry.

use quote::ToTokens;
use syn::{ItemStruct, parse_quote};

use super::{doc, doc_of, enum_doc, field_doc, list_doc, object_doc, order_doc, union_doc};

/// The doc comment on an item comes back as one block of text.
#[test]
fn a_doc_comment_comes_back_whole() {
    let item: ItemStruct = parse_quote! {
        /// The first line.
        ///
        /// The third.
        #[derive(Debug)]
        struct Thing {}
    };
    assert_eq!(doc_of(&item.attrs), "The first line.\n\nThe third.");

    let item: ItemStruct = parse_quote! {
        #[derive(Debug)]
        struct Thing {}
    };
    assert_eq!(doc_of(&item.attrs), "");
}

/// A block of text goes back out as one attribute per line.
#[test]
fn a_block_of_text_becomes_one_attribute_per_line() {
    assert_eq!(
        doc("one\ntwo").to_token_stream().to_string(),
        "# [doc = \"one\"] # [doc = \"two\"]"
    );
    assert_eq!(doc("").to_token_stream().to_string(), "");
}

/// A mirrored field says what it mirrors, then what that field is.
#[test]
fn a_mirrored_field_says_what_it_mirrors() {
    assert_eq!(
        field_doc("RunOutput", "title", "What the run is called."),
        "Filter on `RunOutput.title`.\n\nWhat the run is called."
    );
    assert_eq!(
        field_doc("RunOutput", "title", ""),
        "Filter on `RunOutput.title`."
    );
}

/// Each generated type says what kind of thing it is.
#[test]
fn each_generated_type_says_what_it_is() {
    assert!(object_doc("RunOutput").starts_with("Filter on `RunOutput`."));
    assert!(object_doc("RunOutput").contains("`isNull`"));
    assert!(
        union_doc("RegionSeed", &["seedFromGlob".to_owned()]).ends_with("Variants: seedFromGlob.")
    );
    assert_eq!(
        enum_doc("RegionKind", &["STABLE".to_owned(), "GROWS".to_owned()]),
        "Exact and set tests on `RegionKind` (STABLE, GROWS)."
    );
    assert_eq!(
        list_doc("StageOutput"),
        "Quantified tests over a list of `StageOutput`."
    );
    assert_eq!(
        order_doc("RunOutput"),
        "The fields a listing of `RunOutput` can be sorted by."
    );
}

/// A doc attribute that is not a string is not a doc comment.
#[test]
fn a_doc_attribute_that_is_not_text_is_passed_over() {
    let item: ItemStruct = parse_quote! {
        #[doc = 4]
        struct Thing {}
    };
    assert_eq!(doc_of(&item.attrs), "");
}
