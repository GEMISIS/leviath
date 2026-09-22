//! The `#[Object]` impl shape.
//!
//! `#[Object]` rewrites every resolver it sees: it injects the execution
//! context and wraps the answer in a `Result`, so by the time it has run there
//! is nothing left for a filter to call. This macro therefore sits above it
//! and reads the resolvers as written.
//!
//! Each mirrored resolver's body is moved into a plain accessor of its own and
//! the resolver is left calling it. The body keeps its own source lines, so it
//! is compiled, measured and stepped through exactly once, in the place it was
//! written.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::visit_mut::{self, VisitMut};
use syn::{
    Error, ExprAwait, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, Lifetime, Result, ReturnType,
    Type, TypeReference, parse_quote,
};

use crate::attrs::{self, MirrorArgs};
use crate::docs;
use crate::emit::{Access, Body, Field, Mirrored};
use crate::names::{self, Shape};

/// Mirror one `#[Object]` impl block.
pub(crate) fn expand(args: MirrorArgs, mut item: ItemImpl) -> Result<TokenStream> {
    let target = named_type(&item)?;
    if !item.attrs.iter().any(|attr| attr.path().is_ident("Object")) {
        return Err(Error::new(
            target.span(),
            "`#[mirror]` on an impl block goes directly above `#[Object]`, which is \
             what makes the impl a GraphQL type",
        ));
    }
    let explicit = attrs::explicit_name(&item.attrs, "Object")?;
    let names = names::names(&target, Shape::Object, explicit.as_deref())?;
    if explicit.is_none() {
        attrs::set_name(&mut item.attrs, "Object", &names.target);
    }

    if args.no_filter {
        // Named and nothing else: the helper attributes come off, so an
        // `#[filter]` left on a resolver here is a mistake that is read rather
        // than ignored, and no accessor is moved.
        for entry in &mut item.items {
            if let ImplItem::Fn(method) = entry {
                attrs::take_filter(&mut method.attrs)?;
            }
        }
        return Ok(quote!(#item));
    }

    let mut fields = Vec::new();
    let mut accessors = Vec::new();
    for entry in &mut item.items {
        let ImplItem::Fn(method) = entry else {
            continue;
        };
        let marks = attrs::take_filter(&mut method.attrs)?;
        if marks.skip || attrs::graphql_skip(&method.attrs)? {
            continue;
        }
        let name = method.sig.ident.clone();
        let gql = attrs::explicit_name(&method.attrs, "graphql")?;
        // `ty` is the shape the filter compares, which is the resolver's own
        // answer unless the resolver shapes that answer for the client.
        let ty = match &marks.ty {
            Some(named) => named.clone(),
            None => returned(method)?,
        };
        let doc = docs::doc_of(&method.attrs);

        let access = match &marks.with {
            Some(path) => Access::With(path.clone()),
            None => {
                let (written, name) = accessor(method, marks.io)?;
                accessors.push(written);
                Access::Accessor(name)
            }
        };

        fields.push(Field {
            gql: gql
                .clone()
                .unwrap_or_else(|| names::camel(&name.to_string())),
            rename: gql.is_some(),
            wire: name.to_string(),
            ident: name,
            doc,
            ty,
            access,
            io: marks.io,
            orderable: marks.orderable,
        });
    }

    let mirror = crate::emit::emit(&Mirrored {
        rt: args.rt,
        list: args.list,
        target: target.clone(),
        names,
        body: Body::Object(fields),
    });
    let accessors = (!accessors.is_empty()).then(|| {
        quote! {
            impl #target {
                #(#accessors)*
            }
        }
    });
    Ok(quote! {
        #item
        #accessors
        #mirror
    })
}

/// Move one resolver's body into an accessor, and leave the resolver calling
/// it.
///
/// The returned tokens are the accessor; the resolver is rewritten in place.
fn accessor(method: &mut ImplItemFn, io: bool) -> Result<(TokenStream, Ident)> {
    only_self(method)?;
    if let Some(at) = awaited(method)
        && !io
    {
        return Err(Error::new(
            at,
            "this resolver awaits something, so it cannot be answered from memory: \
             mark it `#[filter(io)]` to answer it in the phase where reads are \
             allowed, or `#[filter(skip)]` to leave it out of the mirror",
        ));
    }
    if io && method.sig.asyncness.is_none() {
        return Err(Error::new(
            method.sig.ident.span(),
            "`#[filter(io)]` is for a resolver that reads something, and this one is \
             not even asynchronous: drop the mark",
        ));
    }

    let name = Ident::new(
        &format!("mirror_{}", method.sig.ident),
        method.sig.ident.span(),
    );
    let output = method.sig.output.clone();
    let body = method.block.clone();
    let asyncness = io.then(|| quote!(async));
    let doc = format!(
        "The value the `{}` resolver answers with, for the filter mirror.",
        method.sig.ident
    );
    let accessor = quote! {
        #[doc = #doc]
        pub(crate) #asyncness fn #name(&self) #output #body
    };
    method.block = if io {
        parse_quote!({ self.#name().await })
    } else {
        parse_quote!({ self.#name() })
    };
    Ok((accessor, name))
}

/// Refuse a resolver the mirror cannot call.
fn only_self(method: &ImplItemFn) -> Result<()> {
    let mut inputs = method.sig.inputs.iter();
    let receiver = matches!(inputs.next(), Some(FnArg::Receiver(_)));
    if !receiver || inputs.next().is_some() {
        return Err(Error::new(
            method.sig.ident.span(),
            "a mirrored resolver takes `&self` and nothing else: add `#[filter(skip)]` \
             to leave it out of the mirror, or `#[filter(with = \"path::fn\")]` to give \
             the mirror an accessor that reads the same thing",
        ));
    }
    Ok(())
}

/// The type a resolver answers with, every lifetime in it made `'static`.
///
/// The mirror names this type in an associated-type projection, where a
/// borrow of the value being filtered has nothing to borrow from. The filter
/// a type maps to does not depend on how long the value lives, so `'static`
/// is the spelling that projects.
fn returned(method: &ImplItemFn) -> Result<Type> {
    let ReturnType::Type(_, ty) = &method.sig.output else {
        return Err(Error::new(
            method.sig.ident.span(),
            "a mirrored resolver answers with a value, and this one answers with \
             nothing: add `#[filter(skip)]`",
        ));
    };
    let mut ty = (**ty).clone();
    Staticize.visit_type_mut(&mut ty);
    Ok(ty)
}

/// Where a resolver body awaits something, if it does anywhere.
fn awaited(method: &ImplItemFn) -> Option<Span> {
    let mut found = Awaits { at: None };
    found.visit_block(&method.block);
    found.at
}

/// Every lifetime rewritten to `'static`, elided ones included.
struct Staticize;

impl VisitMut for Staticize {
    fn visit_type_reference_mut(&mut self, node: &mut TypeReference) {
        node.lifetime = Some(Lifetime::new("'static", Span::call_site()));
        visit_mut::visit_type_reference_mut(self, node);
    }

    fn visit_lifetime_mut(&mut self, node: &mut Lifetime) {
        node.ident = Ident::new("static", node.ident.span());
    }
}

/// The first `.await` in a body, wherever it is.
struct Awaits {
    /// Where it was, once one has been seen.
    at: Option<Span>,
}

impl<'ast> Visit<'ast> for Awaits {
    fn visit_expr_await(&mut self, node: &'ast ExprAwait) {
        self.at.get_or_insert(node.await_token.span);
        visit::visit_expr_await(self, node);
    }
}

/// The plain type name an impl block is for.
fn named_type(item: &ItemImpl) -> Result<Ident> {
    if let Type::Path(path) = &*item.self_ty
        && path.qself.is_none()
        && let Some(last) = path.path.segments.last()
        && last.arguments.is_none()
    {
        return Ok(last.ident.clone());
    }
    Err(Error::new(
        item.self_ty.span(),
        "`#[mirror]` needs an impl of a plain named type, because it writes types \
         named after it",
    ))
}

#[cfg(test)]
#[path = "object_tests.rs"]
mod tests;
