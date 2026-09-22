//! Reading the two attributes this macro understands, and stripping them.
//!
//! `#[mirror(...)]` is the item's own argument list. `#[filter(...)]` is the
//! per-field helper, and it has to be removed from what is emitted, because
//! nothing downstream knows what it means.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Error, Expr, Ident, Lit, Meta, Path, Result, Token, Type, parse_quote};

/// The runtime module generated code calls into when nothing else is said.
pub(crate) const DEFAULT_RT: &str = "crate::commands::serve::graphql::filter";

/// What `#[mirror(...)]` was given.
#[derive(Debug, Clone)]
pub(crate) struct MirrorArgs {
    /// Whether to also write the quantifier input for lists of this type.
    pub(crate) list: bool,
    /// Whether to name the type and write nothing else.
    ///
    /// For a type that has to carry the suffix every output type carries and
    /// is deliberately outside the mirror: one that is only ever read behind
    /// an interface, and so is never the thing a listing filters on.
    pub(crate) no_filter: bool,
    /// The runtime module the generated code calls into.
    pub(crate) rt: Path,
}

/// The runtime path generated code calls into when nothing else is said.
///
/// A literal this file wrote, so it parses; there is no failing case to carry
/// through every caller.
fn default_rt() -> Path {
    syn::parse_str(DEFAULT_RT).expect("the default runtime path is a path")
}

/// Read the arguments on `#[mirror(...)]`.
pub(crate) fn mirror_args(tokens: TokenStream) -> Result<MirrorArgs> {
    let mut args = MirrorArgs {
        list: false,
        no_filter: false,
        rt: default_rt(),
    };
    if tokens.is_empty() {
        return Ok(args);
    }
    let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(tokens)?;
    for meta in metas {
        match &meta {
            Meta::Path(path) if path.is_ident("list") => args.list = true,
            Meta::Path(path) if path.is_ident("no_filter") => args.no_filter = true,
            Meta::NameValue(pair) if pair.path.is_ident("rt") => {
                args.rt = syn::parse_str(&text(&pair.value)?)?;
            }
            other => {
                return Err(Error::new(
                    other.span(),
                    "unknown `#[mirror]` argument: it takes `list`, `no_filter` and \
                     `rt = \"path\"`",
                ));
            }
        }
    }
    if args.list && args.no_filter {
        return Err(Error::new(
            Span::call_site(),
            "`no_filter` writes no filter, so there is no list input for `list` to ask \
             for: drop one of the two",
        ));
    }
    Ok(args)
}

/// What the `#[filter(...)]` attributes on one field said.
#[derive(Debug, Clone)]
pub(crate) struct FilterAttr {
    /// Leave this field out of the mirror entirely.
    pub(crate) skip: bool,
    /// Answer this field in the second phase, where reads are allowed.
    pub(crate) io: bool,
    /// Offer this field as a sort key.
    pub(crate) orderable: bool,
    /// Read this field's value through a function of the caller's.
    pub(crate) with: Option<Path>,
    /// Mirror the field as this type rather than as the one the resolver
    /// answers with.
    ///
    /// For a resolver that shapes its answer for the client - a page of a list
    /// rather than the list - where the value a filter compares is the shape
    /// underneath. The accessor `with` names is what produces it.
    pub(crate) ty: Option<Type>,
    /// Where the attribute was written, for the errors about it.
    pub(crate) span: Span,
}

impl FilterAttr {
    /// The state of a field nothing was said about.
    fn unmarked() -> Self {
        Self {
            skip: false,
            io: false,
            orderable: false,
            with: None,
            ty: None,
            span: Span::call_site(),
        }
    }
}

/// Read every `#[filter(...)]` on one field, and take them out of `attrs`.
pub(crate) fn take_filter(attrs: &mut Vec<Attribute>) -> Result<FilterAttr> {
    let mut marks = FilterAttr::unmarked();
    let mut found = false;
    let mut error = None;
    attrs.retain(|attr| {
        if !attr.path().is_ident("filter") {
            return true;
        }
        found = true;
        marks.span = attr.span();
        if let Err(problem) = read_filter(attr, &mut marks) {
            error.get_or_insert(problem);
        }
        false
    });
    if let Some(problem) = error {
        return Err(problem);
    }
    if found {
        check(&marks)?;
    }
    Ok(marks)
}

/// Fold one `#[filter(...)]` attribute into what the field asked for.
fn read_filter(attr: &Attribute, marks: &mut FilterAttr) -> Result<()> {
    let metas = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
    for meta in metas {
        match &meta {
            Meta::Path(path) if path.is_ident("skip") => marks.skip = true,
            Meta::Path(path) if path.is_ident("io") => marks.io = true,
            Meta::Path(path) if path.is_ident("orderable") => marks.orderable = true,
            Meta::NameValue(pair) if pair.path.is_ident("with") => {
                marks.with = Some(syn::parse_str(&text(&pair.value)?)?);
            }
            Meta::NameValue(pair) if pair.path.is_ident("ty") => {
                marks.ty = Some(syn::parse_str(&text(&pair.value)?)?);
            }
            other => {
                return Err(Error::new(
                    other.span(),
                    "unknown `#[filter]` argument: it takes `skip`, `io`, `orderable`, \
                     `with = \"path::fn\"` and `ty = \"Type\"`",
                ));
            }
        }
    }
    Ok(())
}

/// Refuse the combinations that cannot mean anything.
fn check(marks: &FilterAttr) -> Result<()> {
    if marks.skip && (marks.io || marks.orderable || marks.with.is_some() || marks.ty.is_some()) {
        return Err(Error::new(
            marks.span,
            "`#[filter(skip)]` leaves the field out of the mirror, so nothing else \
             on this attribute can apply to it",
        ));
    }
    if marks.io && marks.orderable {
        return Err(Error::new(
            marks.span,
            "`#[filter(orderable)]` cannot sit on an `io` field: a sort key is read for \
             every value in the listing, and this one costs a read each time",
        ));
    }
    if marks.ty.is_some() && marks.with.is_none() {
        return Err(Error::new(
            marks.span,
            "`ty` says the mirror reads a different type from the one the field \
             answers with, so it needs a `with = \"path::fn\"` that produces it",
        ));
    }
    Ok(())
}

/// The string behind a `name = "value"` argument.
fn text(value: &Expr) -> Result<String> {
    match value {
        Expr::Lit(literal) => match &literal.lit {
            Lit::Str(text) => Ok(text.value()),
            other => Err(Error::new(other.span(), "expected a string in quotes")),
        },
        other => Err(Error::new(other.span(), "expected a string in quotes")),
    }
}

/// The `name = "..."` an item's GraphQL attribute already carries.
pub(crate) fn explicit_name(attrs: &[Attribute], which: &str) -> Result<Option<String>> {
    let Some(attr) = attrs.iter().find(|attr| attr.path().is_ident(which)) else {
        return Ok(None);
    };
    for meta in list_of(attr)? {
        if let Meta::NameValue(pair) = &meta
            && pair.path.is_ident("name")
        {
            return Ok(Some(text(&pair.value)?));
        }
    }
    Ok(None)
}

/// Whether a field or resolver is kept out of the schema itself.
pub(crate) fn graphql_skip(attrs: &[Attribute]) -> Result<bool> {
    let Some(attr) = attrs.iter().find(|attr| attr.path().is_ident("graphql")) else {
        return Ok(false);
    };
    Ok(list_of(attr)?
        .iter()
        .any(|meta| matches!(meta, Meta::Path(path) if path.is_ident("skip"))))
}

/// The arguments of one attribute, or nothing for one written bare.
fn list_of(attr: &Attribute) -> Result<Vec<Meta>> {
    match &attr.meta {
        Meta::Path(_) => Ok(Vec::new()),
        Meta::List(_) => Ok(attr
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?
            .into_iter()
            .collect()),
        Meta::NameValue(pair) => Err(Error::new(
            pair.span(),
            "expected an attribute with arguments in brackets",
        )),
    }
}

/// Give an item's GraphQL attribute the name the mirror decided.
///
/// The attribute may be missing, bare or already carrying other arguments, and
/// all three end up as `#[which(..., name = "...")]`.
pub(crate) fn set_name(attrs: &mut Vec<Attribute>, which: &str, name: &str) {
    let path = Ident::new(which, Span::call_site());
    let Some(at) = attrs.iter().position(|attr| attr.path().is_ident(which)) else {
        attrs.push(parse_quote!(#[#path(name = #name)]));
        return;
    };
    let existing = match &attrs[at].meta {
        Meta::List(list) => list.tokens.clone(),
        _ => TokenStream::new(),
    };
    let tokens = if existing.is_empty() {
        quote!(name = #name)
    } else {
        quote!(#existing, name = #name)
    };
    attrs[at] = parse_quote!(#[#path(#tokens)]);
}

#[cfg(test)]
#[path = "attrs_tests.rs"]
mod tests;
