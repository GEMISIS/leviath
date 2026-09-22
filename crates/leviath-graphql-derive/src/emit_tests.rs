//! What the emitter writes, given one form to write it from.

use syn::parse_quote;

use super::{Access, Body, Field, Mirrored, Variant, binding, emit, ident};
use crate::attrs::mirror_args;
use crate::names::{Shape, names};

/// One mirrored type, described the way each shape describes it.
fn mirrored(body: Body, list: bool) -> Mirrored {
    Mirrored {
        rt: mirror_args(quote::quote!()).expect("no arguments").rt,
        list,
        target: ident("Thing"),
        names: names(&ident("Thing"), Shape::Object, None).expect("a plain name"),
        body,
    }
}

/// One mirrored field, with nothing marked on it.
fn field(access: Access) -> Field {
    Field {
        ident: ident("name"),
        gql: "name".to_owned(),
        rename: false,
        doc: "What it is called.".to_owned(),
        ty: parse_quote!(String),
        access,
        io: false,
        orderable: false,
        wire: "name".to_owned(),
    }
}

/// The generated code, as readable Rust.
fn written(mirrored: &Mirrored) -> String {
    prettyplease::unparse(&syn::parse2(emit(mirrored)).expect("the output is Rust"))
}

/// Each way of reading a value is one line in the generated code.
#[test]
fn each_way_of_reading_a_value_is_one_line() {
    let written = written(&mirrored(
        Body::Object(vec![
            field(Access::Field(ident("name"))),
            Field {
                ident: ident("size"),
                gql: "size".to_owned(),
                wire: "size".to_owned(),
                access: Access::Accessor(ident("mirror_size")),
                ..field(Access::Field(ident("size")))
            },
            Field {
                ident: ident("owner"),
                gql: "owner".to_owned(),
                wire: "owner".to_owned(),
                access: Access::With(parse_quote!(relations::owner_of)),
                ..field(Access::Field(ident("owner")))
            },
        ]),
        false,
    ));
    assert!(written.contains("&target.name, cx"), "{written}");
    assert!(written.contains("&target.mirror_size(), cx"), "{written}");
    assert!(
        written.contains("&relations::owner_of(target, cx), cx"),
        "{written}"
    );
}

/// A type with nothing to compare still carries the combinators.
#[test]
fn a_type_with_no_fields_still_composes() {
    let written = written(&mirrored(Body::Object(Vec::new()), false));
    assert!(
        written.contains("pub(crate) and: Option<Vec<ThingFilter>>"),
        "{written}"
    );
    assert!(
        written.contains("_target: &Self::Target") && written.contains("_acc: &mut"),
        "an unused binding is named as one:\n{written}"
    );
    assert!(!written.contains("ThingListFilter"), "{written}");
}

/// A sort key is written only where one was asked for.
#[test]
fn a_sort_key_is_written_only_where_it_was_asked_for() {
    let plain = written(&mirrored(
        Body::Object(vec![field(Access::Field(ident("name")))]),
        false,
    ));
    assert!(!plain.contains("ThingOrderField"), "{plain}");

    let sorted = written(&mirrored(
        Body::Object(vec![Field {
            orderable: true,
            ..field(Access::Field(ident("name")))
        }]),
        false,
    ));
    assert!(sorted.contains("enum ThingOrderField"), "{sorted}");
    assert!(sorted.contains("Self::Name => \"name\""), "{sorted}");
    assert!(
        sorted.contains("const ALL: &'static [Self] = &[ThingOrderField::Name]"),
        "{sorted}"
    );
    assert!(sorted.contains("sort_key(&self.name)"), "{sorted}");
    assert!(
        sorted.contains("_cx:"),
        "a key read off the value alone does not need the context:\n{sorted}"
    );

    let through_an_accessor = written(&mirrored(
        Body::Object(vec![Field {
            orderable: true,
            access: Access::With(parse_quote!(relations::owner_of)),
            ..field(Access::Field(ident("name")))
        }]),
        false,
    ));
    assert!(
        !through_an_accessor.contains("_cx:"),
        "a key read through an accessor is given the context:\n{through_an_accessor}"
    );
}

/// A renamed field carries the name the schema shows.
#[test]
fn a_renamed_field_carries_its_schema_name() {
    let written = written(&mirrored(
        Body::Object(vec![Field {
            rename: true,
            gql: "label".to_owned(),
            ..field(Access::Field(ident("name")))
        }]),
        false,
    ));
    assert!(
        written.contains("#[graphql(name = \"label\")]"),
        "{written}"
    );
}

/// A list input is written beside the filter it quantifies over.
#[test]
fn a_list_input_quantifies_over_the_filter() {
    let written = written(&mirrored(
        Body::Object(vec![field(Access::Field(ident("name")))]),
        true,
    ));
    assert!(
        written.contains("pub(crate) struct ThingListFilter"),
        "{written}"
    );
    assert!(
        written.contains("some: Option<Box<ThingFilter>>"),
        "{written}"
    );
    assert!(written.contains("fn list_test("), "{written}");
}

/// An enum's comparator is written from its values.
#[test]
fn an_enum_comparator_is_written_from_its_values() {
    let mut described = mirrored(
        Body::Enumeration(vec![ident("Steel"), ident("Brass")]),
        false,
    );
    described.names = names(&ident("Thing"), Shape::Enumeration, None).expect("a plain name");
    let written = written(&described);
    assert!(written.contains("(STEEL, BRASS)"), "{written}");
    assert!(written.contains("fn choices("), "{written}");
    assert!(written.contains("enum_test(self, filter, cx)"), "{written}");
}

/// A union's variants each become one field and one match arm.
#[test]
fn a_union_variant_is_one_field_and_one_arm() {
    let mut described = mirrored(
        Body::Union(vec![Variant {
            ident: ident("tag"),
            variant: ident("Tag"),
            ty: parse_quote!(Tag),
            doc: "A tag.".to_owned(),
            gql: "tag".to_owned(),
        }]),
        false,
    );
    described.names = names(&ident("Thing"), Shape::Union, None).expect("a plain name");
    let written = written(&described);
    assert!(
        written.contains("let set = [self.tag.is_some()];"),
        "{written}"
    );
    assert!(
        written
            .contains("Thing::Tag(value) => acc.variant(self.tag.as_deref(), value, cx, &set, 0)"),
        "{written}"
    );
    assert!(
        written.contains("confirm.variant(self.tag.as_deref(), value, &set, 0).await"),
        "{written}"
    );
}

/// A binding the code around it does not use is named as one.
#[test]
fn an_unused_binding_says_so() {
    assert_eq!(binding(true, "target"), ident("target"));
    assert_eq!(binding(false, "target"), ident("_target"));
}
