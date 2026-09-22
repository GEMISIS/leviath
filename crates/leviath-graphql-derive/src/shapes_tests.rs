//! The three derive-shaped items, and what each of them refuses.

use quote::quote;
use syn::{ItemEnum, ItemStruct, parse_quote};

use super::{enumeration, structure};
use crate::attrs::{MirrorArgs, mirror_args};

/// What `#[mirror]` was given, with nothing said.
fn plain() -> MirrorArgs {
    mirror_args(quote!()).expect("no arguments")
}

/// What `#[mirror(list)]` was given.
fn with_list() -> MirrorArgs {
    mirror_args(quote!(list)).expect("one argument")
}

/// The generated code for one struct, as readable Rust.
fn from_struct(args: MirrorArgs, item: ItemStruct) -> String {
    let tokens = structure(args, item).expect("a mirrored struct");
    let written = prettyplease::unparse(&syn::parse2(tokens).expect("the output is Rust"));
    println!("{written}");
    written
}

/// The generated code for one enum, as readable Rust.
fn from_enum(args: MirrorArgs, item: ItemEnum) -> String {
    let tokens = enumeration(args, item).expect("a mirrored enum");
    let written = prettyplease::unparse(&syn::parse2(tokens).expect("the output is Rust"));
    println!("{written}");
    written
}

/// A struct's fields are read straight off the value.
#[test]
fn a_struct_field_is_read_straight_off_the_value() {
    let written = from_struct(
        with_list(),
        parse_quote! {
            #[derive(Debug, SimpleObject)]
            pub(crate) struct Tag {
                /// What the tag says.
                pub(crate) text: String,
            }
        },
    );
    assert!(
        written.contains("#[graphql(name = \"TagOutput\")]"),
        "{written}"
    );
    assert!(
        written.contains("acc.field(self.text.as_deref(), &target.text, cx);"),
        "{written}"
    );
    assert!(
        written.contains("#[graphql(name = \"TagInput\")]"),
        "{written}"
    );
    assert!(
        written.contains("#[graphql(name = \"TagListInput\")]"),
        "{written}"
    );
}

/// `no_filter` names the struct and writes nothing else.
#[test]
fn naming_only_writes_no_mirror_for_a_struct() {
    let written = from_struct(
        mirror_args(quote!(no_filter)).expect("one argument"),
        parse_quote! {
            #[derive(Debug, SimpleObject)]
            pub(crate) struct Tag {
                /// What the tag says.
                #[filter(skip)]
                pub(crate) text: String,
            }
        },
    );
    assert!(
        written.contains("#[graphql(name = \"TagOutput\")]"),
        "{written}"
    );
    assert!(!written.contains("TagFilter"), "{written}");
    assert!(!written.contains("#[filter"), "{written}");
}

/// A mark that cannot mean anything is read even where nothing is mirrored.
#[test]
fn naming_only_still_reads_a_struct_field_mark() {
    let problem = structure(
        mirror_args(quote!(no_filter)).expect("one argument"),
        parse_quote! {
            #[derive(Debug, SimpleObject)]
            struct Tag {
                /// What the tag says.
                #[filter(sorted)]
                text: String,
            }
        },
    )
    .expect_err("not a mark");
    assert!(
        problem.to_string().contains("unknown `#[filter]`"),
        "{problem}"
    );
}

/// An enum keeps its own name, so there is nothing for `no_filter` to do.
#[test]
fn naming_only_has_nothing_to_do_for_an_enum() {
    let problem = enumeration(
        mirror_args(quote!(no_filter)).expect("one argument"),
        parse_quote! {
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
            enum Material {
                /// Steel.
                Steel,
            }
        },
    )
    .expect_err("nothing to do");
    assert!(
        problem.to_string().contains("keeps its own name"),
        "{problem}"
    );
}

/// A struct field can be read through an accessor, and left out entirely.
#[test]
fn a_struct_field_can_be_skipped_or_read_another_way() {
    let written = from_struct(
        plain(),
        parse_quote! {
            #[derive(Debug, SimpleObject)]
            struct Tag {
                /// Left out.
                #[filter(skip)]
                text: String,
                /// Kept out of the schema too.
                #[graphql(skip)]
                hidden: String,
                /// Read another way.
                #[filter(with = "tags::weight_of")]
                weight: i32,
            }
        },
    );
    assert!(!written.contains("text: Option<"), "{written}");
    assert!(!written.contains("hidden: Option<"), "{written}");
    assert!(written.contains("tags::weight_of(target, cx)"), "{written}");
}

/// A struct field is already in memory, so it is never a read.
#[test]
fn a_struct_field_is_never_a_read() {
    let problem = structure(
        plain(),
        parse_quote! {
            #[derive(Debug, SimpleObject)]
            struct Tag {
                /// A field.
                #[filter(io)]
                text: String,
            }
        },
    )
    .expect_err("a field is not a resolver");
    assert!(
        problem.to_string().contains("already in memory"),
        "{problem}"
    );
}

/// A struct field is the value itself, so there is nothing to reshape.
#[test]
fn a_struct_field_is_mirrored_as_the_type_it_is() {
    let problem = structure(
        plain(),
        parse_quote! {
            #[derive(Debug, SimpleObject)]
            struct Tag {
                /// A field.
                #[filter(with = "tags::names_of", ty = "Vec<String>")]
                text: String,
            }
        },
    )
    .expect_err("a field carries its own type");
    assert!(
        problem.to_string().contains("the value itself"),
        "{problem}"
    );
}

/// A struct the schema does not read is not a struct this mirrors.
#[test]
fn a_struct_that_is_not_an_object_is_refused() {
    let problem = structure(
        plain(),
        parse_quote! {
            #[derive(Debug)]
            struct Tag {
                text: String,
            }
        },
    )
    .expect_err("no derive");
    assert!(
        problem.to_string().contains("#[derive(SimpleObject)]"),
        "{problem}"
    );

    let problem = structure(
        plain(),
        parse_quote! {
            #[derive(Debug, SimpleObject)]
            struct Tag(String);
        },
    )
    .expect_err("no named fields");
    assert!(problem.to_string().contains("named fields"), "{problem}");
}

/// An enum's values become a comparator.
#[test]
fn an_enum_becomes_a_comparator() {
    let written = from_enum(
        with_list(),
        parse_quote! {
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
            enum Material {
                /// Steel.
                Steel,
            }
        },
    );
    assert!(
        written.contains("Exact and set tests on `Material` (STEEL)."),
        "{written}"
    );
    assert!(
        written.contains("#[graphql(name = \"MaterialFilter\")]"),
        "{written}"
    );
    assert!(
        written.contains("#[graphql(name = \"MaterialListFilter\")]"),
        "{written}"
    );
    assert!(written.contains("eq: Option<Material>"), "{written}");
}

/// A union's variants become one field each, named after the member type.
#[test]
fn a_union_becomes_one_field_per_variant() {
    let written = from_enum(
        plain(),
        parse_quote! {
            #[derive(Debug, Union)]
            enum Marking {
                /// A tag.
                Tag(Tag),
                /// A number.
                Stamp(Serial),
            }
        },
    );
    assert!(
        written.contains("#[graphql(name = \"MarkingInput\")]"),
        "{written}"
    );
    assert!(written.contains("pub(crate) tag: Option<"), "{written}");
    assert!(written.contains("Box<<Tag as"), "{written}");
    assert!(written.contains("pub(crate) serial: Option<"), "{written}");
    assert!(written.contains("Box<<Serial as"), "{written}");
    assert!(
        written.contains("acc.variant(self.serial.as_deref(), value, cx, &set, 1)"),
        "{written}"
    );
    assert!(written.contains("Variants: tag, serial."), "{written}");
}

/// An enum that is neither shape is refused with both names.
#[test]
fn an_enum_that_is_neither_shape_is_refused() {
    let problem = enumeration(
        plain(),
        parse_quote! {
            #[derive(Debug)]
            enum Material {
                Steel,
            }
        },
    )
    .expect_err("no derive");
    assert!(
        problem
            .to_string()
            .contains("`#[derive(Enum)]` or `#[derive(Union)]`"),
        "{problem}"
    );
}

/// A union member is one plain named type, and nothing else.
#[test]
fn a_union_member_is_one_named_type() {
    let problem = enumeration(
        plain(),
        parse_quote! {
            #[derive(Debug, Union)]
            enum Marking {
                Tag { text: String },
            }
        },
    )
    .expect_err("not a newtype");
    assert!(
        problem.to_string().contains("exactly one type"),
        "{problem}"
    );

    let problem = enumeration(
        plain(),
        parse_quote! {
            #[derive(Debug, Union)]
            enum Marking {
                Tag(Tag, Serial),
            }
        },
    )
    .expect_err("two types");
    assert!(
        problem.to_string().contains("exactly one type"),
        "{problem}"
    );

    let problem = enumeration(
        plain(),
        parse_quote! {
            #[derive(Debug, Union)]
            enum Marking {
                Tag(&'static str),
            }
        },
    )
    .expect_err("not a named type");
    assert!(
        problem.to_string().contains("plain named type"),
        "{problem}"
    );
}

/// A mark on a union variant is read off it rather than left to leak.
#[test]
fn a_mark_on_a_union_variant_is_taken_off() {
    let written = from_enum(
        plain(),
        parse_quote! {
            #[derive(Debug, Union)]
            enum Marking {
                /// A tag.
                #[filter(orderable)]
                Tag(Tag),
            }
        },
    );
    assert!(!written.contains("#[filter"), "{written}");
}

/// Every attribute a struct carries is read, and a broken one is refused.
#[test]
fn a_broken_attribute_anywhere_in_a_struct_is_refused() {
    let broken = [
        (
            quote!(#[derive(SimpleObject)] #[graphql(name = 4)]),
            "string in quotes",
        ),
        (
            quote!(#[derive(SimpleObject)] #[graphql(name = "Tag")]),
            "does not end in `Output`",
        ),
    ];
    for (attributes, said) in broken {
        let item: ItemStruct = parse_quote! {
            #attributes
            struct Tag {
                /// The text.
                text: String,
            }
        };
        let problem = structure(plain(), item).expect_err("a refusal");
        assert!(problem.to_string().contains(said), "{problem}");
    }

    let problem = structure(
        plain(),
        parse_quote! {
            #[derive(SimpleObject)]
            struct Tag {
                /// The text.
                #[filter(sorted)]
                text: String,
            }
        },
    )
    .expect_err("not a mark");
    assert!(
        problem.to_string().contains("unknown `#[filter]`"),
        "{problem}"
    );

    let problem = structure(
        plain(),
        parse_quote! {
            #[derive(SimpleObject)]
            struct Tag {
                /// The text.
                #[graphql = "text"]
                text: String,
            }
        },
    )
    .expect_err("not arguments");
    assert!(
        problem.to_string().contains("arguments in brackets"),
        "{problem}"
    );

    let problem = structure(
        plain(),
        parse_quote! {
            #[derive(SimpleObject)]
            struct Tag {
                /// The text.
                #[graphql(name = 4)]
                text: String,
            }
        },
    )
    .expect_err("not a name");
    assert!(
        problem.to_string().contains("string in quotes"),
        "{problem}"
    );

    let problem = structure(
        plain(),
        parse_quote! {
            #[derive(SimpleObject = 4)]
            struct Tag {
                /// The text.
                text: String,
            }
        },
    )
    .expect_err("not a derive list");
    assert!(!problem.to_string().is_empty(), "{problem}");
}

/// A name the source gave a struct is kept rather than replaced.
#[test]
fn a_struct_keeps_the_name_it_was_given() {
    let written = from_struct(
        plain(),
        parse_quote! {
            #[derive(SimpleObject)]
            #[graphql(name = "TagOutput")]
            struct Tag {
                /// The text.
                text: String,
            }
        },
    );
    assert!(
        written.matches("name = \"TagOutput\"").count() == 1,
        "the name is set once:\n{written}"
    );
}

/// Every attribute an enum carries is read, and a broken one is refused.
#[test]
fn a_broken_attribute_anywhere_in_an_enum_is_refused() {
    let problem = enumeration(
        plain(),
        parse_quote! {
            #[derive(Enum)]
            #[graphql(name = 4)]
            enum Material {
                /// Steel.
                Steel,
            }
        },
    )
    .expect_err("not a name");
    assert!(
        problem.to_string().contains("string in quotes"),
        "{problem}"
    );

    let problem = enumeration(
        plain(),
        parse_quote! {
            #[derive(Enum)]
            enum MaterialEvent {
                /// Steel.
                Steel,
            }
        },
    )
    .expect_err("out of scope");
    assert!(
        problem.to_string().contains("not a type a filter mirrors"),
        "{problem}"
    );

    let problem = enumeration(
        plain(),
        parse_quote! {
            #[derive(Union)]
            enum Marking {
                /// A tag.
                #[filter(sorted)]
                Tag(Tag),
            }
        },
    )
    .expect_err("not a mark");
    assert!(
        problem.to_string().contains("unknown `#[filter]`"),
        "{problem}"
    );
}
