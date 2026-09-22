//! Which shape the macro was put on, and what to emit when it was the wrong
//! one.
//!
//! A macro that deletes the item it could not understand turns one mistake
//! into a page of errors about everything that named the type. So a refusal
//! keeps the item, with the helper attributes taken off it, and adds the one
//! error that says what to do.

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::visit_mut::{self, VisitMut};
use syn::{Error, Field, ImplItemFn, Item, Result, Variant};

use crate::attrs;
use crate::object;
use crate::shapes;

/// Expand one `#[mirror]`, or the item plus the reason it was refused.
pub(crate) fn mirror(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand(attr, item.clone()) {
        Ok(expanded) => expanded,
        Err(problem) => refuse(item, &problem),
    }
}

/// Expand one `#[mirror]`.
fn expand(attr: TokenStream, item: TokenStream) -> Result<TokenStream> {
    let args = attrs::mirror_args(attr)?;
    match syn::parse2::<Item>(item)? {
        Item::Impl(item) => object::expand(args, item),
        Item::Struct(item) => shapes::structure(args, item),
        Item::Enum(item) => shapes::enumeration(args, item),
        other => Err(Error::new(
            other.span(),
            "`#[mirror]` goes on an `#[Object]` impl, a `SimpleObject` struct, an \
             `Enum` or a `Union`",
        )),
    }
}

/// The item as written, without the helper attributes, and one error.
fn refuse(item: TokenStream, problem: &Error) -> TokenStream {
    let said = problem.to_compile_error();
    match syn::parse2::<Item>(item) {
        Ok(mut parsed) => {
            Strip.visit_item_mut(&mut parsed);
            quote! {
                #parsed
                #said
            }
        }
        Err(_) => said,
    }
}

/// Every `#[filter(...)]` taken back off, so the compiler sees one error.
struct Strip;

impl Strip {
    /// Drop the helper attributes from one list.
    fn clean(attrs: &mut Vec<syn::Attribute>) {
        attrs.retain(|attr| !attr.path().is_ident("filter"));
    }
}

impl VisitMut for Strip {
    fn visit_field_mut(&mut self, node: &mut Field) {
        Self::clean(&mut node.attrs);
        visit_mut::visit_field_mut(self, node);
    }

    fn visit_impl_item_fn_mut(&mut self, node: &mut ImplItemFn) {
        Self::clean(&mut node.attrs);
        visit_mut::visit_impl_item_fn_mut(self, node);
    }

    fn visit_variant_mut(&mut self, node: &mut Variant) {
        Self::clean(&mut node.attrs);
        visit_mut::visit_variant_mut(self, node);
    }
}

#[cfg(test)]
#[path = "expand_tests.rs"]
mod tests;
