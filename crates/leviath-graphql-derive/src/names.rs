//! Every name the mirror system derives, in one function.
//!
//! The suffix legend is the whole naming system, so it is decided here and
//! nowhere else: an object reads as `XOutput`, its filter mirror as `XInput`,
//! a list of it as `XListInput`, and an enum's comparator as `XFilter` on both
//! sides. A type whose name already says it is something else is refused
//! rather than mirrored.

use proc_macro2::Span;
use syn::{Error, Ident, Result};

/// Which kind of item is being mirrored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A `SimpleObject` struct or an `#[Object]` impl.
    Object,
    /// An `Enum`.
    Enumeration,
    /// A `Union`.
    Union,
}

/// Suffixes that say the type is not something a filter mirrors.
const OUT_OF_SCOPE: [&str; 3] = ["Connection", "Result", "Event"];

/// The suffix every mirrored object type's GraphQL name ends in.
pub(crate) const OUTPUT: &str = "Output";

/// Every name one mirrored type produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Names {
    /// The GraphQL name of the type being mirrored.
    pub(crate) target: String,
    /// The Rust name of the filter input.
    pub(crate) filter: Ident,
    /// The GraphQL name of the filter input.
    pub(crate) filter_gql: String,
    /// The Rust name of the list input.
    pub(crate) list: Ident,
    /// The GraphQL name of the list input.
    pub(crate) list_gql: String,
    /// The Rust name of the sort-key enum.
    pub(crate) order: Ident,
    /// The GraphQL name of the sort-key enum.
    pub(crate) order_gql: String,
    /// The Rust name of the order input.
    pub(crate) order_input: Ident,
    /// The GraphQL name of the order input.
    pub(crate) order_input_gql: String,
}

/// Every name derived from one Rust identifier and the shape it names.
///
/// `explicit` is the GraphQL name the source already gave the type, if any.
/// For an object it has to end in `Output`, because a hand-written name that
/// does not is the one way the suffix legend could be broken from inside.
pub(crate) fn names(ident: &Ident, shape: Shape, explicit: Option<&str>) -> Result<Names> {
    let rust = ident.to_string();
    if let Some(suffix) = OUT_OF_SCOPE.iter().find(|suffix| rust.ends_with(*suffix)) {
        return Err(Error::new(
            ident.span(),
            format!(
                "`{rust}` ends in `{suffix}`, so it is not a type a filter mirrors: \
                 a connection, a mutation result and an event are read, never filtered on"
            ),
        ));
    }

    let stem = match (shape, explicit) {
        (Shape::Object, Some(name)) => match name.strip_suffix(OUTPUT) {
            Some(stem) => stem.to_owned(),
            None => {
                return Err(Error::new(
                    ident.span(),
                    format!(
                        "`{name}` does not end in `{OUTPUT}`, and every object type in this \
                         schema does: drop the explicit name and let `#[mirror]` set it"
                    ),
                ));
            }
        },
        (_, _) => explicit.unwrap_or(&rust).to_owned(),
    };

    let target = match shape {
        Shape::Object => format!("{stem}{OUTPUT}"),
        Shape::Enumeration | Shape::Union => stem.clone(),
    };
    let filter_gql = match shape {
        Shape::Object | Shape::Union => format!("{stem}Input"),
        Shape::Enumeration => format!("{stem}Filter"),
    };
    let list_gql = match shape {
        Shape::Object | Shape::Union => format!("{stem}ListInput"),
        Shape::Enumeration => format!("{stem}ListFilter"),
    };

    Ok(Names {
        target,
        filter: suffixed(ident, "Filter"),
        filter_gql,
        list: suffixed(ident, "ListFilter"),
        list_gql,
        order: suffixed(ident, "OrderField"),
        order_gql: format!("{stem}OrderField"),
        order_input: suffixed(ident, "Order"),
        order_input_gql: format!("{stem}Order"),
    })
}

/// A Rust identifier with a suffix, keeping the original's span.
fn suffixed(ident: &Ident, suffix: &str) -> Ident {
    Ident::new(&format!("{ident}{suffix}"), ident.span())
}

/// A Rust identifier built from a name, at the call site.
pub(crate) fn ident(name: &str) -> Ident {
    Ident::new(name, Span::call_site())
}

/// One field's GraphQL name, the way async-graphql camel-cases it.
pub(crate) fn camel(name: &str) -> String {
    let mut out = String::new();
    for (at, part) in name.split('_').enumerate() {
        let mut chars = part.chars();
        match (at, chars.next()) {
            (0, _) => out.push_str(part),
            (_, Some(first)) => {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
            (_, None) => {}
        }
    }
    out
}

/// A field name as a type or variant name.
pub(crate) fn pascal(name: &str) -> String {
    let camel = camel(name);
    let mut chars = camel.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => camel,
    }
}

/// An enum value's name the way GraphQL writes it.
pub(crate) fn screaming(name: &str) -> String {
    snake(name).to_uppercase()
}

/// A type name as a field name, which is how a union's variants are written.
pub(crate) fn snake(name: &str) -> String {
    let mut out = String::new();
    for (at, letter) in name.chars().enumerate() {
        if letter.is_uppercase() {
            if at > 0 {
                out.push('_');
            }
            out.extend(letter.to_lowercase());
        } else {
            out.push(letter);
        }
    }
    out
}

#[cfg(test)]
#[path = "names_tests.rs"]
mod tests;
