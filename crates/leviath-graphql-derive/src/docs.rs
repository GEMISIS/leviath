//! The doc comments the generated types carry.
//!
//! A GraphQL type with no description is a type a client has to guess at, and
//! this schema has a test that refuses one. So every type and every field the
//! macro writes gets a doc comment, and a mirrored field keeps the one the
//! output field already had: whoever reads `RunInput.title` sees what
//! `RunOutput.title` is, right there.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Attribute;

/// The doc comment already on an item, as one block of text.
pub(crate) fn doc_of(attrs: &[Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(pair) = &attr.meta
            && let syn::Expr::Lit(literal) = &pair.value
            && let syn::Lit::Str(text) = &literal.lit
        {
            lines.push(text.value().trim().to_owned());
        }
    }
    lines.join("\n")
}

/// A block of text as the doc attributes an item carries.
pub(crate) fn doc(text: &str) -> TokenStream {
    let lines = text.lines().map(|line| quote!(#[doc = #line]));
    quote!(#(#lines)*)
}

/// What one mirrored field's doc comment says.
///
/// The first line names the output field it mirrors, so the input reads as a
/// mirror rather than as a second definition of the same thing, and the rest
/// is the output field's own doc comment.
pub(crate) fn field_doc(target: &str, field: &str, original: &str) -> String {
    let head = format!("Filter on `{target}.{field}`.");
    if original.is_empty() {
        head
    } else {
        format!("{head}\n\n{original}")
    }
}

/// What a mirrored object's filter input says about itself.
pub(crate) fn object_doc(target: &str) -> String {
    format!(
        "Filter on `{target}`. Every field here mirrors the same-named field there; \
         set the ones that have to hold.\n\n\
         `and`, `or` and `not` compose filters of this same type, and `isNull` asks \
         about the value being absent rather than about its contents."
    )
}

/// What a union's filter input says about itself.
pub(crate) fn union_doc(target: &str, variants: &[String]) -> String {
    format!(
        "Variant tests on `{target}`. Set a variant field to match values of that \
         variant against its filter; a value of another variant never matches.\n\n\
         Variants: {}.",
        variants.join(", ")
    )
}

/// What an enum's filter input says about itself.
pub(crate) fn enum_doc(target: &str, values: &[String]) -> String {
    format!("Exact and set tests on `{target}` ({}).", values.join(", "))
}

/// What a list input says about itself.
pub(crate) fn list_doc(target: &str) -> String {
    format!("Quantified tests over a list of `{target}`.")
}

/// What an order input says about itself.
pub(crate) fn term_doc(target: &str) -> String {
    format!("One sort key a listing of `{target}` runs in, and its direction.")
}

/// What a sort-key enum says about itself.
pub(crate) fn order_doc(target: &str) -> String {
    format!("The fields a listing of `{target}` can be sorted by.")
}

#[cfg(test)]
#[path = "docs_tests.rs"]
mod tests;
