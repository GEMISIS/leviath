//! What the macro writes, once every shape has been read into one form.
//!
//! Every impl below is data and delegation. `cheap` is a list of one call per
//! field, `io` is the same list with reads allowed, and `test` and `confirm`
//! hand the whole thing to the runtime. Nothing here decides anything, which
//! is what makes one reading of the runtime's `engine.rs` a reading of every
//! mirrored type in the schema.

use proc_macro2::{Literal, Span, TokenStream};
use quote::quote;
use syn::{Ident, Path, Type};

use crate::docs;
use crate::names::{self, Names};

/// How the mirror reads one field's value off the value being filtered.
#[derive(Debug, Clone)]
pub(crate) enum Access {
    /// A plain struct field.
    Field(Ident),
    /// An accessor the macro moved the resolver's body into.
    Accessor(Ident),
    /// A function named by `#[filter(with = "...")]`.
    With(Path),
}

/// One mirrored field.
#[derive(Debug, Clone)]
pub(crate) struct Field {
    /// The filter input's field name.
    pub(crate) ident: Ident,
    /// The GraphQL field name, for the doc comment and any rename.
    pub(crate) gql: String,
    /// Whether the GraphQL name differs from what the ident camel-cases to.
    pub(crate) rename: bool,
    /// The output field's own doc comment.
    pub(crate) doc: String,
    /// The type the value is read as, with every lifetime made `'static`.
    pub(crate) ty: Type,
    /// How the value is read.
    pub(crate) access: Access,
    /// Whether answering this field costs a read.
    pub(crate) io: bool,
    /// Whether this field is offered as a sort key.
    pub(crate) orderable: bool,
    /// The Rust name a sort key travels under.
    pub(crate) wire: String,
}

/// One mirrored union variant.
#[derive(Debug, Clone)]
pub(crate) struct Variant {
    /// The filter input's field name.
    pub(crate) ident: Ident,
    /// The Rust variant this field is about.
    pub(crate) variant: Ident,
    /// The type the variant carries.
    pub(crate) ty: Type,
    /// The variant's own doc comment.
    pub(crate) doc: String,
    /// The GraphQL field name.
    pub(crate) gql: String,
}

/// What one shape read down to.
#[derive(Debug, Clone)]
pub(crate) enum Body {
    /// A struct's fields or an impl's resolvers.
    Object(Vec<Field>),
    /// A union's variants.
    Union(Vec<Variant>),
    /// An enum's values.
    Enumeration(Vec<Ident>),
}

/// Everything the emitter needs about one mirrored type.
#[derive(Debug, Clone)]
pub(crate) struct Mirrored {
    /// The runtime module the generated code calls into.
    pub(crate) rt: Path,
    /// Whether to also write the quantifier input for lists of this type.
    pub(crate) list: bool,
    /// The Rust type being mirrored.
    pub(crate) target: Ident,
    /// Every name this type produces.
    pub(crate) names: Names,
    /// What was read off the shape.
    pub(crate) body: Body,
}

/// The whole of what one mirrored type produces.
pub(crate) fn emit(mirrored: &Mirrored) -> TokenStream {
    let shaped = match &mirrored.body {
        Body::Object(fields) => object(mirrored, fields),
        Body::Union(variants) => union(mirrored, variants),
        Body::Enumeration(values) => enumeration(mirrored, values),
    };
    let list = mirrored.list.then(|| list(mirrored));
    quote! {
        #shaped
        #list
    }
}

/// The filter input, and the impls behind it, for an object.
fn object(mirrored: &Mirrored, fields: &[Field]) -> TokenStream {
    let rt = &mirrored.rt;
    let target = &mirrored.target;
    let filter = &mirrored.names.filter;
    let filter_gql = &mirrored.names.filter_gql;
    let doc = docs::doc(&docs::object_doc(&mirrored.names.target));
    let declarations = fields.iter().map(|field| declare(mirrored, field));
    let combinators = combinators(mirrored);

    let reads = fields.iter().any(|field| !field.io);
    let cheap_target = binding(reads, "target");
    let cheap_cx = binding(reads, "cx");
    let acc = binding(!fields.is_empty(), "acc");
    let cheap = fields.iter().map(|field| {
        let ident = &field.ident;
        if field.io {
            return quote!(acc.pending(&self.#ident););
        }
        let value = value(field, &cheap_target, &cheap_cx);
        quote!(acc.field(self.#ident.as_deref(), &#value, #cheap_cx);)
    });

    let io_target = binding(!fields.is_empty(), "target");
    let cx = ident("cx");
    let confirms = fields.iter().map(|field| {
        let name = &field.ident;
        let value = value(field, &io_target, &cx);
        if field.io {
            quote!(confirm.io(self.#name.as_deref(), #value).await;)
        } else {
            quote!(confirm.field(self.#name.as_deref(), &#value).await;)
        }
    });

    let shared = shared(mirrored);
    let order = order(mirrored, fields);
    quote! {
        #doc
        #[derive(Clone, Debug, Default, ::async_graphql::InputObject)]
        #[graphql(name = #filter_gql)]
        pub(crate) struct #filter {
            #(#declarations)*
            #combinators
        }

        #shared

        impl #rt::Mirror for #filter {
            type Target = #target;

            fn parts(&self) -> #rt::Parts<'_, Self> {
                #rt::Parts::new(
                    self.and.as_deref(),
                    self.or.as_deref(),
                    self.not.as_deref(),
                    self.is_null,
                )
            }

            fn cheap(
                &self,
                #cheap_target: &Self::Target,
                #cheap_cx: &#rt::MatchCx<'_>,
                #acc: &mut #rt::Acc,
            ) {
                #(#cheap)*
            }

            fn io<'mirror>(
                &'mirror self,
                #io_target: &'mirror Self::Target,
                cx: &'mirror #rt::MatchCx<'mirror>,
            ) -> #rt::BoxFuture<'mirror, bool> {
                Box::pin(async move {
                    let mut confirm = #rt::Confirm::new(cx);
                    #(#confirms)*
                    confirm.finish()
                })
            }
        }

        #order
    }
}

/// The filter input, and the impls behind it, for a union.
fn union(mirrored: &Mirrored, variants: &[Variant]) -> TokenStream {
    let rt = &mirrored.rt;
    let target = &mirrored.target;
    let filter = &mirrored.names.filter;
    let filter_gql = &mirrored.names.filter_gql;
    let doc = docs::doc(&docs::union_doc(
        &mirrored.names.target,
        &variants
            .iter()
            .map(|each| each.gql.clone())
            .collect::<Vec<_>>(),
    ));
    let declarations = variants.iter().map(|each| {
        let ident = &each.ident;
        let ty = &each.ty;
        let field_doc = docs::doc(&docs::field_doc(
            &mirrored.names.target,
            &each.gql,
            &each.doc,
        ));
        quote! {
            #field_doc
            pub(crate) #ident: Option<Box<<#ty as #rt::Filterable>::Filter>>,
        }
    });
    let combinators = combinators(mirrored);
    let set = variants.iter().map(|each| {
        let ident = &each.ident;
        quote!(self.#ident.is_some())
    });
    let set = quote!(let set = [#(#set),*];);
    let cheap = variants.iter().enumerate().map(|(at, each)| {
        let ident = &each.ident;
        let variant = &each.variant;
        let at = Literal::usize_unsuffixed(at);
        quote!(#target::#variant(value) => acc.variant(self.#ident.as_deref(), value, cx, &set, #at),)
    });
    let confirms = variants.iter().enumerate().map(|(at, each)| {
        let ident = &each.ident;
        let variant = &each.variant;
        let at = Literal::usize_unsuffixed(at);
        quote!(#target::#variant(value) => confirm.variant(self.#ident.as_deref(), value, &set, #at).await,)
    });

    let shared = shared(mirrored);
    quote! {
        #doc
        #[derive(Clone, Debug, Default, ::async_graphql::InputObject)]
        #[graphql(name = #filter_gql)]
        pub(crate) struct #filter {
            #(#declarations)*
            #combinators
        }

        #shared

        impl #rt::Mirror for #filter {
            type Target = #target;

            fn parts(&self) -> #rt::Parts<'_, Self> {
                #rt::Parts::new(
                    self.and.as_deref(),
                    self.or.as_deref(),
                    self.not.as_deref(),
                    self.is_null,
                )
            }

            fn cheap(
                &self,
                target: &Self::Target,
                cx: &#rt::MatchCx<'_>,
                acc: &mut #rt::Acc,
            ) {
                #set
                match target {
                    #(#cheap)*
                }
            }

            fn io<'mirror>(
                &'mirror self,
                target: &'mirror Self::Target,
                cx: &'mirror #rt::MatchCx<'mirror>,
            ) -> #rt::BoxFuture<'mirror, bool> {
                Box::pin(async move {
                    let mut confirm = #rt::Confirm::new(cx);
                    #set
                    match target {
                        #(#confirms)*
                    }
                    confirm.finish()
                })
            }
        }
    }
}

/// The comparator input, and the impls behind it, for an enum.
fn enumeration(mirrored: &Mirrored, values: &[Ident]) -> TokenStream {
    let rt = &mirrored.rt;
    let target = &mirrored.target;
    let filter = &mirrored.names.filter;
    let filter_gql = &mirrored.names.filter_gql;
    let doc = docs::doc(&docs::enum_doc(
        &mirrored.names.target,
        &values
            .iter()
            .map(|value| names::screaming(&value.to_string()))
            .collect::<Vec<_>>(),
    ));
    quote! {
        #doc
        #[derive(Clone, Debug, Default, ::async_graphql::InputObject)]
        #[graphql(name = #filter_gql)]
        pub(crate) struct #filter {
            #[doc = "Exactly this value."]
            pub(crate) eq: Option<#target>,
            #[doc = "Anything but this value."]
            pub(crate) ne: Option<#target>,
            #[doc = "One of these values."]
            #[graphql(name = "in")]
            pub(crate) within: Option<Vec<#target>>,
            #[doc = "None of these values."]
            pub(crate) not_in: Option<Vec<#target>>,
            #[doc = "Match where there is no value at all."]
            pub(crate) is_null: Option<bool>,
        }

        impl #rt::Nullable for #filter {
            fn is_null(&self) -> Option<bool> {
                self.is_null
            }
        }

        impl #rt::EnumParts for #filter {
            type Value = #target;

            fn choices(&self) -> #rt::Choices<'_, Self::Value> {
                #rt::Choices::new(
                    self.eq,
                    self.ne,
                    self.within.as_deref(),
                    self.not_in.as_deref(),
                    self.is_null,
                )
            }
        }

        impl #rt::Filterable for #target {
            type Filter = #filter;

            fn test(&self, filter: &Self::Filter, cx: &#rt::MatchCx<'_>) -> #rt::Tri {
                #rt::enum_test(self, filter, cx)
            }

            fn confirm<'mirror>(
                &'mirror self,
                filter: &'mirror Self::Filter,
                cx: &'mirror #rt::MatchCx<'mirror>,
            ) -> #rt::BoxFuture<'mirror, bool> {
                #rt::settled(#rt::enum_test(self, filter, cx))
            }
        }
    }
}

/// The quantifier input for lists of one mirrored type.
fn list(mirrored: &Mirrored) -> TokenStream {
    let rt = &mirrored.rt;
    let target = &mirrored.target;
    let filter = &mirrored.names.filter;
    let list = &mirrored.names.list;
    let list_gql = &mirrored.names.list_gql;
    let doc = docs::doc(&docs::list_doc(&mirrored.names.target));
    quote! {
        #doc
        #[derive(Clone, Debug, Default, ::async_graphql::InputObject)]
        #[graphql(name = #list_gql)]
        pub(crate) struct #list {
            #[doc = "At least one item matches."]
            pub(crate) some: Option<Box<#filter>>,
            #[doc = "Every item matches."]
            pub(crate) every: Option<Box<#filter>>,
            #[doc = "No item matches."]
            pub(crate) none: Option<Box<#filter>>,
            #[doc = "Match where there is no list at all."]
            pub(crate) is_null: Option<bool>,
        }

        impl #rt::Nullable for #list {
            fn is_null(&self) -> Option<bool> {
                self.is_null
            }
        }

        impl #rt::Quantified for #list {
            type Item = #target;

            fn quantifiers(&self) -> #rt::Quantifiers<'_, #filter> {
                #rt::Quantifiers::new(
                    self.some.as_deref(),
                    self.every.as_deref(),
                    self.none.as_deref(),
                )
            }
        }

        impl #rt::ListItem for #target {
            type ListFilter = #list;

            fn list_test(
                items: &[Self],
                filter: &Self::ListFilter,
                cx: &#rt::MatchCx<'_>,
            ) -> #rt::Tri {
                #rt::quantified(items, filter, cx)
            }

            fn list_confirm<'mirror>(
                items: &'mirror [Self],
                filter: &'mirror Self::ListFilter,
                cx: &'mirror #rt::MatchCx<'mirror>,
            ) -> #rt::BoxFuture<'mirror, bool> {
                #rt::quantified_confirm(items, filter, cx)
            }
        }
    }
}

/// The sort-key enum and the `Orderable` impl, for the fields marked with it.
fn order(mirrored: &Mirrored, fields: &[Field]) -> TokenStream {
    let sortable: Vec<&Field> = fields.iter().filter(|field| field.orderable).collect();
    if sortable.is_empty() {
        return TokenStream::new();
    }
    let rt = &mirrored.rt;
    let target = &mirrored.target;
    let order = &mirrored.names.order;
    let order_gql = &mirrored.names.order_gql;
    let order_input = &mirrored.names.order_input;
    let order_input_gql = &mirrored.names.order_input_gql;
    let doc = docs::doc(&docs::order_doc(&mirrored.names.target));
    let term_doc = docs::doc(&docs::term_doc(&mirrored.names.target));
    // `default_with` takes an expression as text, so the runtime path is
    // spelled out here rather than interpolated into the attribute.
    let descending = format!("{}::OrderDirection::Desc", quote!(#rt));

    let uses_cx = sortable
        .iter()
        .any(|field| matches!(field.access, Access::With(_)));
    let cx = binding(uses_cx, "cx");
    let this = ident("self");

    let values = sortable.iter().map(|field| {
        let variant = ident(&names::pascal(&field.ident.to_string()));
        let field_doc = docs::doc(&format!(
            "Sort by `{}.{}`.",
            mirrored.names.target, field.gql
        ));
        quote! {
            #field_doc
            #variant,
        }
    });
    let listed = sortable.iter().map(|field| {
        let variant = ident(&names::pascal(&field.ident.to_string()));
        quote!(#order::#variant)
    });
    let keys = sortable.iter().map(|field| {
        let variant = ident(&names::pascal(&field.ident.to_string()));
        let value = value(field, &this, &cx);
        quote!(#order::#variant => #rt::sort_key(&#value),)
    });
    let wires = sortable.iter().map(|field| {
        let variant = ident(&names::pascal(&field.ident.to_string()));
        let wire = &field.wire;
        quote!(Self::#variant => #wire,)
    });

    quote! {
        #doc
        #[derive(Debug, Clone, Copy, PartialEq, Eq, ::async_graphql::Enum)]
        #[graphql(name = #order_gql)]
        pub(crate) enum #order {
            #(#values)*
        }

        impl #order {
            #[doc = "Every sort key this type offers, in declaration order."]
            pub(crate) const ALL: &'static [Self] = &[#(#listed),*];
        }

        impl #rt::OrderField for #order {
            fn wire(self) -> &'static str {
                match self {
                    #(#wires)*
                }
            }
        }

        impl<'mirror> #rt::Orderable<#rt::MatchCx<'mirror>> for #target {
            type Field = #order;

            fn key(
                &self,
                field: Self::Field,
                #cx: &#rt::MatchCx<'mirror>,
            ) -> #rt::CursorKey {
                match field {
                    #(#keys)*
                }
            }
        }

        #term_doc
        #[derive(Debug, Clone, Copy, PartialEq, Eq, ::async_graphql::InputObject)]
        #[graphql(name = #order_input_gql)]
        pub(crate) struct #order_input {
            #[doc = "Which field to order by."]
            pub(crate) field: #order,
            #[doc = "Which way it runs. Newest or largest first unless you say otherwise."]
            #[graphql(default_with = #descending)]
            pub(crate) direction: #rt::OrderDirection,
        }

        impl #order_input {
            #[doc = "This term, as the walk compares on it."]
            pub(crate) fn term(self) -> #rt::Term<#order> {
                #rt::Term {
                    field: self.field,
                    direction: self.direction,
                }
            }
        }
    }
}

/// The four fields every filter input carries.
fn combinators(mirrored: &Mirrored) -> TokenStream {
    let filter = &mirrored.names.filter;
    quote! {
        #[doc = "Every filter in this list has to hold."]
        pub(crate) and: Option<Vec<#filter>>,
        #[doc = "At least one filter in this list has to hold."]
        pub(crate) or: Option<Vec<#filter>>,
        #[doc = "This filter must not hold."]
        pub(crate) not: Option<Box<#filter>>,
        #[doc = "Match where the value itself is absent."]
        #[doc = ""]
        #[doc = "`true` matches only where there is no value, `false` only where there is one."]
        pub(crate) is_null: Option<bool>,
    }
}

/// The two impls an object and a union both carry.
fn shared(mirrored: &Mirrored) -> TokenStream {
    let rt = &mirrored.rt;
    let target = &mirrored.target;
    let filter = &mirrored.names.filter;
    quote! {
        impl #rt::Nullable for #filter {
            fn is_null(&self) -> Option<bool> {
                self.is_null
            }
        }

        impl #rt::Filterable for #target {
            type Filter = #filter;

            fn test(&self, filter: &Self::Filter, cx: &#rt::MatchCx<'_>) -> #rt::Tri {
                #rt::object_test(self, filter, cx)
            }

            fn confirm<'mirror>(
                &'mirror self,
                filter: &'mirror Self::Filter,
                cx: &'mirror #rt::MatchCx<'mirror>,
            ) -> #rt::BoxFuture<'mirror, bool> {
                #rt::object_confirm(self, filter, cx)
            }
        }
    }
}

/// One field of a filter input.
fn declare(mirrored: &Mirrored, field: &Field) -> TokenStream {
    let rt = &mirrored.rt;
    let ident = &field.ident;
    let ty = &field.ty;
    let gql = &field.gql;
    let doc = docs::doc(&docs::field_doc(&mirrored.names.target, gql, &field.doc));
    let rename = field.rename.then(|| quote!(#[graphql(name = #gql)]));
    quote! {
        #doc
        #rename
        pub(crate) #ident: Option<Box<<#ty as #rt::Filterable>::Filter>>,
    }
}

/// How one field's value is read, given what the surrounding code calls the
/// value being filtered and the match context.
fn value(field: &Field, target: &Ident, cx: &Ident) -> TokenStream {
    match &field.access {
        Access::Field(name) => quote!(#target.#name),
        Access::Accessor(name) => quote!(#target.#name()),
        Access::With(path) => quote!(#path(#target, #cx)),
    }
}

/// A binding's name, underscored where the code around it does not use it.
fn binding(used: bool, name: &str) -> Ident {
    if used {
        ident(name)
    } else {
        ident(&format!("_{name}"))
    }
}

/// An identifier at the call site.
fn ident(name: &str) -> Ident {
    Ident::new(name, Span::call_site())
}

#[cfg(test)]
#[path = "emit_tests.rs"]
mod tests;
