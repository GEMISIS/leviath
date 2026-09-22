//! Reading the two attributes, and refusing the combinations that cannot mean
//! anything.

use quote::{ToTokens, quote};
use syn::{Attribute, ItemStruct, parse_quote};

use super::{DEFAULT_RT, explicit_name, graphql_skip, mirror_args, set_name, take_filter};

/// The attributes on a struct written for one test.
fn attrs(item: ItemStruct) -> Vec<Attribute> {
    item.attrs
}

/// The attributes on one field of a struct written for one test.
fn field_attrs(item: ItemStruct) -> Vec<Attribute> {
    item.fields
        .iter()
        .next()
        .expect("the test struct has a field")
        .attrs
        .clone()
}

/// With nothing said, the macro writes no list and calls the real runtime.
#[test]
fn the_defaults_are_the_schema_this_was_written_for() {
    let args = mirror_args(quote!()).expect("no arguments");
    assert!(!args.list);
    assert_eq!(
        args.rt.to_token_stream().to_string().replace(' ', ""),
        DEFAULT_RT.replace(' ', "")
    );
}

/// Both arguments are read, in either order.
#[test]
fn the_arguments_are_read() {
    let args = mirror_args(quote!(list, rt = "crate::rt")).expect("both arguments");
    assert!(args.list);
    assert_eq!(args.rt.to_token_stream().to_string(), "crate :: rt");
}

/// Anything else is refused by name.
#[test]
fn an_unknown_argument_is_refused() {
    let problem = mirror_args(quote!(lists)).expect_err("not an argument");
    assert!(
        problem.to_string().contains("unknown `#[mirror]`"),
        "{problem}"
    );
    let problem = mirror_args(quote!(rt = 4)).expect_err("not a string");
    assert!(
        problem.to_string().contains("string in quotes"),
        "{problem}"
    );
    let problem = mirror_args(quote!(rt = some::path)).expect_err("not a literal");
    assert!(
        problem.to_string().contains("string in quotes"),
        "{problem}"
    );
}

/// `no_filter` names the type and writes nothing else, and it has nothing to
/// say beside `list`.
#[test]
fn naming_only_is_one_argument_and_not_two() {
    let args = mirror_args(quote!(no_filter)).expect("one argument");
    assert!(args.no_filter);
    assert!(!args.list);

    let problem = mirror_args(quote!(list, no_filter)).expect_err("a contradiction");
    assert!(
        problem.to_string().contains("drop one of the two"),
        "{problem}"
    );
}

/// A field nobody marked is mirrored as it is.
#[test]
fn an_unmarked_field_asks_for_nothing() {
    let mut attrs = field_attrs(parse_quote! {
        struct Thing {
            /// A field.
            name: String,
        }
    });
    let marks = take_filter(&mut attrs).expect("nothing to read");
    assert!(
        !marks.skip && !marks.io && !marks.orderable && marks.with.is_none() && marks.ty.is_none()
    );
    assert_eq!(attrs.len(), 1, "the doc comment is left alone");
}

/// Each mark is read, and taken off what is emitted.
#[test]
fn every_mark_is_read_and_then_removed() {
    let mut attrs = field_attrs(parse_quote! {
        struct Thing {
            #[filter(orderable)]
            #[filter(with = "run_relations::blueprint_of")]
            name: String,
        }
    });
    let marks = take_filter(&mut attrs).expect("two marks");
    assert!(marks.orderable);
    assert!(marks.with.is_some());
    assert!(attrs.is_empty(), "the helper attribute does not survive");

    let mut attrs = field_attrs(parse_quote! {
        struct Thing {
            #[filter(io)]
            name: String,
        }
    });
    assert!(take_filter(&mut attrs).expect("one mark").io);

    // A field the mirror reads as something else, which is the one place the
    // type is written out rather than read off the field.
    let mut attrs = field_attrs(parse_quote! {
        struct Thing {
            #[filter(io, with = "run_relations::regions_of", ty = "Vec<Region>")]
            name: String,
        }
    });
    let marks = take_filter(&mut attrs).expect("three marks");
    assert!(marks.io && marks.with.is_some());
    assert_eq!(
        marks.ty.map(|ty| quote!(#ty).to_string()),
        Some("Vec < Region >".to_owned())
    );

    let mut attrs = field_attrs(parse_quote! {
        struct Thing {
            #[filter(skip)]
            name: String,
        }
    });
    assert!(take_filter(&mut attrs).expect("one mark").skip);
}

/// A mark nobody wrote is refused by name.
#[test]
fn an_unknown_mark_is_refused() {
    let mut attrs = field_attrs(parse_quote! {
        struct Thing {
            #[filter(sorted)]
            name: String,
        }
    });
    let problem = take_filter(&mut attrs).expect_err("not a mark");
    assert!(
        problem.to_string().contains("unknown `#[filter]`"),
        "{problem}"
    );

    let mut attrs = field_attrs(parse_quote! {
        struct Thing {
            #[filter]
            name: String,
        }
    });
    assert!(take_filter(&mut attrs).is_err(), "a bare mark says nothing");
}

/// The combinations that cannot mean anything are refused where written.
#[test]
fn the_impossible_combinations_are_refused() {
    let cases = [
        (
            quote!(#[filter(skip, orderable)]),
            "leaves the field out of the mirror",
        ),
        (
            quote!(#[filter(io, orderable)]),
            "cannot sit on an `io` field",
        ),
        (
            quote!(#[filter(skip, ty = "Vec<Region>")]),
            "leaves the field out of the mirror",
        ),
        (
            quote!(#[filter(ty = "Vec<Region>")]),
            "needs a `with = \"path::fn\"`",
        ),
    ];
    for (mark, said) in cases {
        let item: ItemStruct = parse_quote! {
            struct Thing {
                #mark
                name: String,
            }
        };
        let mut attrs = field_attrs(item);
        let problem = take_filter(&mut attrs).expect_err("a contradiction");
        assert!(problem.to_string().contains(said), "{problem}");
    }
}

/// The name a type was already given is read back, however it was written.
#[test]
fn an_explicit_name_is_found_wherever_it_sits() {
    assert_eq!(
        explicit_name(
            &attrs(parse_quote!(
                struct Thing {}
            )),
            "graphql"
        )
        .expect("no attribute"),
        None
    );
    assert_eq!(
        explicit_name(
            &attrs(parse_quote! {
                #[graphql]
                struct Thing {}
            }),
            "graphql"
        )
        .expect("a bare attribute"),
        None
    );
    assert_eq!(
        explicit_name(
            &attrs(parse_quote! {
                #[graphql(complex, name = "ThingOutput")]
                struct Thing {}
            }),
            "graphql"
        )
        .expect("a name"),
        Some("ThingOutput".to_owned())
    );
    let problem = explicit_name(
        &attrs(parse_quote! {
            #[graphql = "ThingOutput"]
            struct Thing {}
        }),
        "graphql",
    )
    .expect_err("not an argument list");
    assert!(
        problem.to_string().contains("arguments in brackets"),
        "{problem}"
    );
}

/// A field kept out of the schema is kept out of the mirror.
#[test]
fn a_field_out_of_the_schema_is_out_of_the_mirror() {
    assert!(
        !graphql_skip(&attrs(parse_quote!(
            struct Thing {}
        )))
        .expect("no attribute")
    );
    assert!(
        !graphql_skip(&attrs(parse_quote! {
            #[graphql(name = "ThingOutput")]
            struct Thing {}
        }))
        .expect("another argument")
    );
    assert!(
        graphql_skip(&attrs(parse_quote! {
            #[graphql(skip)]
            struct Thing {}
        }))
        .expect("the mark")
    );
}

/// The name the mirror decided is set, whatever was there before.
#[test]
fn the_name_is_set_however_the_attribute_was_written() {
    let mut written = attrs(parse_quote!(
        struct Thing {}
    ));
    set_name(&mut written, "graphql", "ThingOutput");
    assert_eq!(
        written[0].to_token_stream().to_string(),
        "# [graphql (name = \"ThingOutput\")]"
    );

    let mut written = attrs(parse_quote! {
        #[graphql]
        struct Thing {}
    });
    set_name(&mut written, "graphql", "ThingOutput");
    assert_eq!(
        written[0].to_token_stream().to_string(),
        "# [graphql (name = \"ThingOutput\")]"
    );

    let mut written = attrs(parse_quote! {
        #[graphql(complex)]
        struct Thing {}
    });
    set_name(&mut written, "graphql", "ThingOutput");
    assert_eq!(
        written[0].to_token_stream().to_string(),
        "# [graphql (complex , name = \"ThingOutput\")]"
    );
}

/// Arguments that are not arguments at all are refused where they are read.
#[test]
fn arguments_that_do_not_parse_are_refused() {
    assert!(mirror_args(quote!(=)).is_err(), "not an argument list");
    assert!(
        mirror_args(quote!(rt = "12 34")).is_err(),
        "not a path either"
    );
}

/// A `with` that is not a path, or a `ty` that is not a type, is refused the
/// same way.
#[test]
fn an_accessor_that_is_not_a_path_is_refused() {
    for mark in [
        quote!(#[filter(with = 4)]),
        quote!(#[filter(with = "12 34")]),
        quote!(#[filter(with = "a::b", ty = 4)]),
        quote!(#[filter(with = "a::b", ty = "not a type")]),
    ] {
        let item: ItemStruct = parse_quote! {
            struct Thing {
                #mark
                name: String,
            }
        };
        let mut attrs = field_attrs(item);
        assert!(take_filter(&mut attrs).is_err(), "not an accessor");
    }
}

/// A GraphQL attribute that does not parse is refused wherever it is read.
#[test]
fn a_graphql_attribute_that_does_not_parse_is_refused() {
    let broken: Vec<Attribute> = vec![parse_quote!(#[graphql(name = 4)])];
    assert!(explicit_name(&broken, "graphql").is_err(), "not a name");

    let broken: Vec<Attribute> = vec![parse_quote!(#[graphql(=)])];
    assert!(explicit_name(&broken, "graphql").is_err(), "not arguments");
    assert!(graphql_skip(&broken).is_err(), "not arguments");

    let broken: Vec<Attribute> = vec![parse_quote!(#[graphql = "ThingOutput"])];
    assert!(graphql_skip(&broken).is_err(), "not arguments");
}
