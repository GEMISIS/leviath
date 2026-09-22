//! The filter system every listing in this schema shares.
//!
//! One annotated output type produces everything else. `#[mirror]` reads the
//! type and writes the input object that mirrors it, field for field, each
//! field wrapped in the filter its own type is compared with. What the macro
//! writes is data and delegation: the rules live here, in
//! [`engine`](self::engine), [`values`](self::values) and [`tri`](self::tri),
//! where they are written once and measured once.
//!
//! ```text
//!                     #[mirror(list)]
//!                     #[Object] impl Region { ...resolvers... }
//!                                   |
//!         +-------------------------+---------------------------+
//!         |                         |                           |
//!   type RegionOutput        input RegionInput          input RegionListInput
//!   (the same resolvers)      name: StringFilter          some / every / none
//!                             kind: RegionKindFilter
//!                             and / or / not / isNull
//! ```
//!
//! A filter is answered in two phases. The first reads only what is already
//! in memory and may answer "undecided"; the second is allowed to open files,
//! and runs only for the values the first could not settle. Where the first
//! phase gives an answer, that answer is the true one, which is what lets a
//! listing drop a value without reading anything of it.
//!
//! The macro reaches this module through its `rt` parameter, which defaults to
//! this path, so everything a generated impl names is re-exported here.

pub(crate) mod cx;
pub(crate) mod engine;
pub(crate) mod run_predicate;
pub(crate) mod run_relations;
pub(crate) mod scalars;
pub(crate) mod sift;
pub(crate) mod traits;
pub(crate) mod tri;
pub(crate) mod values;

#[cfg(test)]
pub(crate) mod testkit;

// The face the macro's generated code names. Everything a generated impl
// calls is reachable as `filter::<name>`, so the `rt` argument is one path
// rather than one path per file.
pub(crate) use cx::*;
pub(crate) use engine::*;
pub(crate) use sift::*;
pub(crate) use traits::*;
pub(crate) use tri::*;

// The ordering half of the contract belongs to `paging`, and the cursor key
// to `serve::cursor`. They are named here so a generated `Orderable` impl
// reaches them through the same one `rt` path as everything else.
pub(crate) use super::paging::order::{OrderDirection, OrderField, Orderable, Term};
pub(super) use crate::commands::serve::cursor::CursorKey;

#[cfg(test)]
#[path = "mirror_e2e_tests.rs"]
mod mirror_e2e_tests;
