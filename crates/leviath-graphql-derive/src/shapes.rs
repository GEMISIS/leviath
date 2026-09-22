//! The three derive-shaped items: a `SimpleObject` struct, an `Enum`, a
//! `Union`.
//!
//! These need no rewriting, only reading: every value is already in memory, so
//! the mirror reads a struct field directly and matches a union's variant.
//! What the macro adds to the item itself is the GraphQL name, because the
//! suffix legend is not something a hand-written name should be able to break.

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Attribute, Error, Fields, Ident, ItemEnum, ItemStruct, Result, Type};

use crate::attrs::{self, MirrorArgs};
use crate::docs;
use crate::emit::{Access, Body, Field, Mirrored, Variant};
use crate::names::{self, Shape};

/// Mirror one `SimpleObject` struct.
pub(crate) fn structure(args: MirrorArgs, mut item: ItemStruct) -> Result<TokenStream> {
    derived(&item.attrs, "SimpleObject", item.ident.span())?;
    let explicit = attrs::explicit_name(&item.attrs, "graphql")?;
    let names = names::names(&item.ident, Shape::Object, explicit.as_deref())?;
    if explicit.is_none() {
        attrs::set_name(&mut item.attrs, "graphql", &names.target);
    }

    if args.no_filter {
        // Named and nothing else. The helper attributes still come off, so one
        // left on a field is read rather than ignored.
        for field in item.fields.iter_mut() {
            attrs::take_filter(&mut field.attrs)?;
        }
        return Ok(quote!(#item));
    }

    let Fields::Named(named) = &mut item.fields else {
        return Err(Error::new(
            item.ident.span(),
            "a mirrored `SimpleObject` has named fields, because the mirror's own \
             fields are named after them",
        ));
    };

    let mut fields = Vec::new();
    for field in &mut named.named {
        let marks = attrs::take_filter(&mut field.attrs)?;
        if marks.skip || attrs::graphql_skip(&field.attrs)? {
            continue;
        }
        if marks.io {
            return Err(Error::new(
                marks.span,
                "`#[filter(io)]` is for a resolver whose body reads something, and a \
                 struct field is already in memory",
            ));
        }
        if marks.ty.is_some() {
            return Err(Error::new(
                marks.span,
                "`ty` is for a resolver that shapes its answer for the client, and a \
                 struct field is the value itself: give the field the type the mirror \
                 should read",
            ));
        }
        let ident = field
            .ident
            .clone()
            .expect("a struct with named fields names every one of them");
        let gql = attrs::explicit_name(&field.attrs, "graphql")?;
        let access = match &marks.with {
            Some(path) => Access::With(path.clone()),
            None => Access::Field(ident.clone()),
        };
        fields.push(Field {
            gql: gql
                .clone()
                .unwrap_or_else(|| names::camel(&ident.to_string())),
            rename: gql.is_some(),
            wire: ident.to_string(),
            doc: docs::doc_of(&field.attrs),
            ty: field.ty.clone(),
            access,
            io: false,
            orderable: marks.orderable,
            ident,
        });
    }

    let mirror = crate::emit::emit(&Mirrored {
        rt: args.rt,
        list: args.list,
        target: item.ident.clone(),
        names,
        body: Body::Object(fields),
    });
    Ok(quote! {
        #item
        #mirror
    })
}

/// Mirror one `Enum` or one `Union`, whichever the item derives.
pub(crate) fn enumeration(args: MirrorArgs, mut item: ItemEnum) -> Result<TokenStream> {
    if args.no_filter {
        return Err(Error::new(
            item.ident.span(),
            "`no_filter` is about the `Output` suffix, and an enum or a union keeps \
             its own name in the schema: there is nothing here for it to do",
        ));
    }
    let union = derived(&item.attrs, "Union", item.ident.span()).is_ok();
    if !union {
        derived(&item.attrs, "Enum", item.ident.span()).map_err(|_| {
            Error::new(
                item.ident.span(),
                "`#[mirror]` on an enum needs `#[derive(Enum)]` or `#[derive(Union)]`: \
                 those are the two shapes it knows how to mirror",
            )
        })?;
    }
    let shape = if union {
        Shape::Union
    } else {
        Shape::Enumeration
    };
    let explicit = attrs::explicit_name(&item.attrs, "graphql")?;
    let names = names::names(&item.ident, shape, explicit.as_deref())?;

    let body = if union {
        Body::Union(variants(&mut item)?)
    } else {
        Body::Enumeration(
            item.variants
                .iter()
                .map(|each| each.ident.clone())
                .collect(),
        )
    };

    let mirror = crate::emit::emit(&Mirrored {
        rt: args.rt,
        list: args.list,
        target: item.ident.clone(),
        names,
        body,
    });
    Ok(quote! {
        #item
        #mirror
    })
}

/// One mirrored field per union variant.
fn variants(item: &mut ItemEnum) -> Result<Vec<Variant>> {
    let mut variants = Vec::new();
    for variant in &mut item.variants {
        attrs::take_filter(&mut variant.attrs)?;
        let Fields::Unnamed(unnamed) = &variant.fields else {
            return Err(Error::new(
                variant.ident.span(),
                "a union's variants each carry exactly one type, which is the member \
                 type the schema lists",
            ));
        };
        let Some(only) = unnamed
            .unnamed
            .first()
            .filter(|_| unnamed.unnamed.len() == 1)
        else {
            return Err(Error::new(
                variant.ident.span(),
                "a union's variants each carry exactly one type, which is the member \
                 type the schema lists",
            ));
        };
        let member = member(&only.ty)?;
        let ident = names::ident(&names::snake(&member.to_string()));
        variants.push(Variant {
            gql: names::camel(&ident.to_string()),
            doc: docs::doc_of(&variant.attrs),
            ty: only.ty.clone(),
            variant: variant.ident.clone(),
            ident,
        });
    }
    Ok(variants)
}

/// The type name one union member carries.
fn member(ty: &Type) -> Result<Ident> {
    if let Type::Path(path) = ty
        && path.qself.is_none()
        && let Some(last) = path.path.segments.last()
        && last.arguments.is_none()
    {
        return Ok(last.ident.clone());
    }
    Err(Error::new(
        ty.span(),
        "a union member is a plain named type, because the mirror names a field \
         after it",
    ))
}

/// Whether an item derives the async-graphql shape it is being read as.
fn derived(attrs: &[Attribute], want: &str, at: proc_macro2::Span) -> Result<()> {
    let mut found = false;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("derive")) {
        attr.parse_nested_meta(|meta| {
            if meta
                .path
                .segments
                .last()
                .is_some_and(|last| last.ident == want)
            {
                found = true;
            }
            Ok(())
        })?;
    }
    if found {
        Ok(())
    } else {
        Err(Error::new(
            at,
            format!("`#[mirror]` here needs `#[derive({want})]` beneath it"),
        ))
    }
}

#[cfg(test)]
#[path = "shapes_tests.rs"]
mod tests;
