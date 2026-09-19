//! The service layer both front ends call.
//!
//! `lev serve` answers on two surfaces: the REST routes and, from this
//! release, GraphQL. Neither is built on the other. A REST handler and a
//! GraphQL resolver that answer the same question call the same function
//! here, and the function knows nothing about HTTP status codes, axum
//! extractors or GraphQL selection sets.
//!
//! What lives here returns domain values and [`error::ServeError`]. What
//! lives in the handler modules turns a request into arguments and a value
//! into a response. That split is what keeps the two surfaces honest: a fix
//! to a filter, a cursor or a daemon reply lands in one place and both
//! surfaces get it.

pub(super) mod error;
pub(super) mod lifecycle;
pub(super) mod runs;
