//! `#[mirror]`: a GraphQL output type's filter input, written from the type.
//!
//! One attribute, four shapes. It goes above `#[Object]`, `#[derive(
//! SimpleObject)]`, `#[derive(Enum)]` or `#[derive(Union)]`, names the output
//! type `XOutput`, and writes the input object `XInput` that mirrors it field
//! for field, each field wrapped in the filter its own type is compared with.
//!
//! ```ignore
//! #[mirror(list)]
//! #[Object]
//! impl Region {
//!     /// Region name, unique within the layout that declares it.
//!     async fn name(&self) -> &str { &self.region().name }
//! }
//! ```
//!
//! Everything it writes is data and delegation. The three-valued matching
//! logic, the absent-value rule and the list quantifiers live in a runtime
//! module the macro only calls into, which the `rt = "path"` argument selects.
//!
//! What it refuses is as much of the point as what it writes. A resolver that
//! takes arguments, or awaits something, has no honest cheap mirror, so it is
//! a compile error until someone says in the source what should happen to it:
//! `#[filter(skip)]` to leave it out, `#[filter(io)]` to answer it in the
//! second phase, or `#[filter(with = "path::fn")]` to give it an accessor.

mod attrs;
mod docs;
mod emit;
mod expand;
mod names;
mod object;
mod shapes;

/// Write the filter mirror of the output type this sits on.
///
/// Takes `list` to also write the `some` / `every` / `none` input for lists of
/// this type, and `rt = "path"` to point the generated code at a runtime
/// module other than the default.
#[proc_macro_attribute]
pub fn mirror(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    expand::mirror(attr.into(), item.into()).into()
}
