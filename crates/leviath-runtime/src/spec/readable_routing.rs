//! `RoutingDef`'s two forms: every field in a readable format, and the
//! binary one the run file keeps.
//!
//! The binary frame has the two fields routing had before it took an
//! `on_unmatched`, so run files keep their layout. A routing block that sets
//! `on_unmatched` writes its `on_error` slot as the variant added at the end
//! of [`ErrorFrame`], which carries both settings. One that does not is
//! written exactly as before, byte for byte.

use serde::{Deserialize, Serialize};

use super::{WriteWith, pick};
use crate::spec::graph::{OnRoutingError, OnUnmatched, RoutingDef};

/// A routing block in the run file's binary frames.
#[derive(Serialize, Deserialize)]
struct RoutingFrame {
    retry_attempts: Option<std::num::NonZeroU32>,
    on_error: Option<ErrorFrame>,
}

/// The `on_error` slot of a [`RoutingFrame`].
///
/// Its first two variants keep the order and shape `OnRoutingError` has
/// always been written in. The settings variant is added at the end, so a
/// file written before it existed reads the same.
#[derive(Serialize, Deserialize)]
enum ErrorFrame {
    Fail,
    ErrorEdge,
    Settings {
        on_error: Option<OnRoutingError>,
        on_unmatched: OnUnmatched,
    },
}

impl From<&RoutingDef> for RoutingFrame {
    fn from(def: &RoutingDef) -> Self {
        let on_error = match (def.on_error, def.on_unmatched) {
            (on_error, Some(on_unmatched)) => Some(ErrorFrame::Settings {
                on_error,
                on_unmatched,
            }),
            (Some(OnRoutingError::Fail), None) => Some(ErrorFrame::Fail),
            (Some(OnRoutingError::ErrorEdge), None) => Some(ErrorFrame::ErrorEdge),
            (None, None) => None,
        };
        Self {
            retry_attempts: def.retry_attempts,
            on_error,
        }
    }
}

impl From<RoutingFrame> for RoutingDef {
    fn from(frame: RoutingFrame) -> Self {
        let (on_error, on_unmatched) = match frame.on_error {
            None => (None, None),
            Some(ErrorFrame::Fail) => (Some(OnRoutingError::Fail), None),
            Some(ErrorFrame::ErrorEdge) => (Some(OnRoutingError::ErrorEdge), None),
            Some(ErrorFrame::Settings {
                on_error,
                on_unmatched,
            }) => (on_error, Some(on_unmatched)),
        };
        Self {
            retry_attempts: frame.retry_attempts,
            on_error,
            on_unmatched,
        }
    }
}

impl Serialize for RoutingDef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let frame: WriteWith<Self, S> = |v, s| RoutingFrame::from(v).serialize(s);
        let text: WriteWith<Self, S> = |v, s| RoutingDef::serialize(v, s);
        pick(s.is_human_readable(), frame, text)(self, s)
    }
}

impl<'de> Deserialize<'de> for RoutingDef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let frame: fn(D) -> Result<Self, D::Error> =
            |d| RoutingFrame::deserialize(d).map(Self::from);
        let text: fn(D) -> Result<Self, D::Error> = |d| RoutingDef::deserialize(d);
        pick(d.is_human_readable(), frame, text)(d)
    }
}
