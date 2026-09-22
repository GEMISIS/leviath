//! Which shape the macro was put on, and what a refusal leaves behind.

use quote::quote;

use super::mirror;

/// The expansion, as one string of tokens.
fn expanded(attr: proc_macro2::TokenStream, item: proc_macro2::TokenStream) -> String {
    mirror(attr, item).to_string()
}

/// Each of the four shapes is dispatched to the code that reads it.
#[test]
fn each_shape_reaches_its_reader() {
    let from_impl = expanded(
        quote!(),
        quote! {
            #[Object]
            impl Region {
                /// The name.
                async fn name(&self) -> &str { &self.name }
            }
        },
    );
    assert!(from_impl.contains("RegionFilter"), "{from_impl}");

    let from_struct = expanded(
        quote!(),
        quote! {
            #[derive(SimpleObject)]
            struct Tag {
                /// The text.
                text: String,
            }
        },
    );
    assert!(from_struct.contains("TagFilter"), "{from_struct}");

    let from_enum = expanded(
        quote!(),
        quote! {
            #[derive(Enum)]
            enum Material {
                /// Steel.
                Steel,
            }
        },
    );
    assert!(from_enum.contains("MaterialFilter"), "{from_enum}");

    let from_union = expanded(
        quote!(),
        quote! {
            #[derive(Union)]
            enum Marking {
                /// A tag.
                Tag(Tag),
            }
        },
    );
    assert!(from_union.contains("MarkingFilter"), "{from_union}");
}

/// Anything else says where the macro does belong.
#[test]
fn another_kind_of_item_is_refused() {
    let said = expanded(
        quote!(),
        quote!(
            fn run() {}
        ),
    );
    assert!(said.contains("compile_error"), "{said}");
    assert!(said.contains("goes on an `#[Object]` impl"), "{said}");
}

/// A refusal keeps the item, so one mistake is one error.
#[test]
fn a_refusal_keeps_the_item_and_drops_the_marks() {
    let said = expanded(
        quote!(),
        quote! {
            #[derive(SimpleObject)]
            struct Tag {
                /// The text.
                #[filter(io)]
                text: String,
            }
        },
    );
    assert!(
        said.contains("struct Tag"),
        "the type survives its own error:\n{said}"
    );
    assert!(
        !said.contains("filter (io)"),
        "the helper attribute does not:\n{said}"
    );
    assert!(said.contains("compile_error"), "{said}");
}

/// A marked resolver and a marked variant lose their marks the same way.
#[test]
fn every_kind_of_mark_is_dropped_from_a_refusal() {
    let said = expanded(
        quote!(),
        quote! {
            #[Object]
            impl Run {
                /// The logs.
                #[filter(orderable)]
                async fn logs(&self, stage: i32) -> String { read(stage) }
            }
        },
    );
    assert!(said.contains("impl Run"), "{said}");
    assert!(!said.contains("filter (orderable)"), "{said}");

    let said = expanded(
        quote!(),
        quote! {
            #[derive(Union)]
            enum Marking {
                /// A tag.
                #[filter(orderable)]
                Tag { text: String },
            }
        },
    );
    assert!(said.contains("enum Marking"), "{said}");
    assert!(!said.contains("filter (orderable)"), "{said}");
}

/// An item that is not Rust at all leaves only the error.
#[test]
fn something_that_is_not_an_item_leaves_only_the_error() {
    let said = expanded(quote!(), quote!(this is not an item));
    assert!(said.contains("compile_error"), "{said}");
    assert!(!said.contains("this is not"), "{said}");
}

/// An argument the macro does not take is refused before anything is read.
#[test]
fn an_unknown_argument_is_refused_first() {
    let said = expanded(
        quote!(lists),
        quote!(
            struct Tag {}
        ),
    );
    assert!(said.contains("unknown `#[mirror]` argument"), "{said}");
}
