//! The four scalars the schema adds to GraphQL's own.
//!
//! Each one exists because a built-in would have lied:
//!
//! - `Int` is 32-bit. A 2 GiB file size and a large token roll-up both
//!   overflow it, so sizes and counters are [`BigInt`].
//! - `Float` is binary floating point. A cost that silently rounds is a lie
//!   about spend, so money is [`Decimal`], written as a string.
//! - A date string would invite timezone maths over a number the daemon
//!   stores as unix seconds, so times are [`Timestamp`].
//! - A cursor is the server's own token, and a client that takes it apart
//!   has coupled itself to the paging implementation, so it is [`Cursor`].

use async_graphql::{InputValueError, InputValueResult, Scalar, ScalarType, Value};

/// Unix epoch seconds. The schema description is on the impl below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timestamp(pub(crate) i64);

/// Unix epoch seconds, as the daemon stores them. No string dates, so no
/// timezone maths over a number that never had a timezone.
#[Scalar(name = "Timestamp")]
impl ScalarType for Timestamp {
    fn parse(value: Value) -> InputValueResult<Self> {
        match value {
            Value::Number(n) => n
                .as_i64()
                .map(Timestamp)
                .ok_or_else(|| InputValueError::custom("expected whole unix seconds")),
            other => Err(InputValueError::expected_type(other)),
        }
    }

    fn to_value(&self) -> Value {
        Value::Number(self.0.into())
    }
}

/// A 64-bit integer. The schema description is on the impl below, which is
/// what clients read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BigInt(pub(crate) i64);

/// A 64-bit integer, serialized as a JSON number. GraphQL's `Int` is 32-bit:
/// it cannot hold a 2 GiB file size or a large aggregate token count. Every
/// magnitude this API produces is well under 2^53, so it round-trips through
/// a browser without loss.
#[Scalar(name = "BigInt")]
impl ScalarType for BigInt {
    fn parse(value: Value) -> InputValueResult<Self> {
        match value {
            Value::Number(n) => n
                .as_i64()
                .map(BigInt)
                .ok_or_else(|| InputValueError::custom("expected a whole number")),
            other => Err(InputValueError::expected_type(other)),
        }
    }

    fn to_value(&self) -> Value {
        Value::Number(self.0.into())
    }
}

/// An exact decimal. The schema description is on the impl below.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub(crate) struct Decimal(pub(crate) f64);

/// An exact decimal, serialized as a JSON string. Money is never a `Float` on
/// the wire: a cost figure that a JSON parser silently re-rounds is a lie
/// about spend. The daemon keeps costs as `f64`, so this carries that number's
/// shortest round-tripping form rather than inventing precision.
#[Scalar(name = "Decimal")]
impl ScalarType for Decimal {
    /// Read one from the string it travels as.
    ///
    /// A JSON number is refused rather than accepted, for the same reason this
    /// goes out as a string: a parser that re-rounds a cost figure on the way in
    /// has changed it, and a schema that took both would make the round trip
    /// depend on which one a client chose.
    fn parse(value: Value) -> InputValueResult<Self> {
        match value {
            Value::String(s) => s
                .parse::<f64>()
                .map(Decimal)
                .map_err(|_| InputValueError::custom("expected a decimal string")),
            other => Err(InputValueError::expected_type(other)),
        }
    }

    fn to_value(&self) -> Value {
        Value::String(self.0.to_string())
    }
}

/// An opaque keyset cursor. The schema description is on the impl below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cursor(pub(crate) String);

/// An opaque keyset cursor: the same token the REST routes hand out. It
/// encodes the sort, the order and a digest of the filters, so a cursor from
/// one listing cannot resume a different one.
#[Scalar(name = "Cursor")]
impl ScalarType for Cursor {
    fn parse(value: Value) -> InputValueResult<Self> {
        match value {
            Value::String(s) => Ok(Cursor(s)),
            other => Err(InputValueError::expected_type(other)),
        }
    }

    fn to_value(&self) -> Value {
        Value::String(self.0.clone())
    }
}

/// Arbitrary JSON. The schema description is on the impl below.
///
/// Deserializes so it can sit inside a recorded tool call's arguments, where a
/// tool's own schema leaves part of the shape to whoever wrote the blueprint.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(crate) struct Json(pub(crate) serde_json::Value);

/// Arbitrary JSON, for the few places where the shape is the caller's rather
/// than ours: a model's provider-specific parameters, a tool call's arguments,
/// an output schema. A typed field is better wherever the type is ours to
/// define, so this appears only where a manifest or a model chose the shape and
/// inventing one here would mean dropping whatever did not fit.
#[Scalar(name = "JSON")]
impl ScalarType for Json {
    fn parse(value: Value) -> InputValueResult<Self> {
        // Every GraphQL value has a JSON form, including the binary one a
        // multipart upload carries, so the conversion has no failing case. A
        // refusal here would have to invent a reason for it.
        Ok(Json(value.into_json().expect("a GraphQL value is JSON")))
    }

    fn to_value(&self) -> Value {
        // Every JSON value has a GraphQL counterpart, so this conversion has no
        // failing case for a value that was itself JSON a moment ago.
        Value::from_json(self.0.clone()).expect("a JSON value converts to a GraphQL one")
    }
}

#[cfg(test)]
mod tests {
    use super::{BigInt, Cursor, Decimal, Json, Timestamp};
    use async_graphql::{ScalarType, Value};

    /// Times and counters cross the wire as numbers, both ways.
    #[test]
    fn whole_numbers_round_trip() {
        let stamp = Timestamp(1_788_924_523);
        assert_eq!(stamp.to_value(), Value::Number(1_788_924_523.into()));
        assert_eq!(
            Timestamp::parse(Value::Number(1_788_924_523.into())).expect("parsed"),
            stamp
        );
        let big = BigInt(2_147_483_648);
        assert_eq!(big.to_value(), Value::Number(2_147_483_648i64.into()));
        assert_eq!(
            BigInt::parse(Value::Number(2_147_483_648i64.into())).expect("parsed"),
            big
        );
    }

    /// A count that does not fit a whole number is refused rather than
    /// truncated, and so is a value of the wrong type.
    #[test]
    fn a_number_that_is_not_whole_is_refused() {
        let fractional = Value::Number(serde_json::Number::from_f64(1.5).expect("finite"));
        assert!(Timestamp::parse(fractional.clone()).is_err());
        assert!(BigInt::parse(fractional).is_err());
        assert!(Timestamp::parse(Value::String("now".into())).is_err());
        assert!(BigInt::parse(Value::String("many".into())).is_err());
    }

    /// Money leaves as a string, and arrives as either a string or a number,
    /// because a client that writes `0.02` in a literal should not be told it
    /// meant something else.
    #[test]
    fn money_is_written_as_a_string_and_read_from_either() {
        assert_eq!(Decimal(0.25).to_value(), Value::String("0.25".into()));
        assert_eq!(
            Decimal::parse(Value::String("0.25".into())).expect("parsed"),
            Decimal(0.25)
        );
        assert!(Decimal::parse(Value::String("free".into())).is_err());
        assert!(Decimal::parse(Value::Boolean(true)).is_err());
        // A JSON number is refused for the same reason this goes out as a
        // string: a parser that re-rounds a cost figure has changed it, and a
        // scalar that took both would make the round trip depend on which form
        // a client picked.
        let number = Value::Number(serde_json::Number::from_f64(0.25).expect("finite"));
        assert!(Decimal::parse(number).is_err());
    }

    /// A cursor is carried, never interpreted.
    #[test]
    fn a_cursor_is_carried_verbatim() {
        let token = Cursor("ab12cd".to_string());
        assert_eq!(token.to_value(), Value::String("ab12cd".into()));
        assert_eq!(
            Cursor::parse(Value::String("ab12cd".into())).expect("parsed"),
            token
        );
        assert!(Cursor::parse(Value::Boolean(false)).is_err());
    }

    /// Arbitrary JSON survives the round trip, including the shapes that have no
    /// GraphQL counterpart.
    #[test]
    fn json_survives_the_round_trip() {
        let value = serde_json::json!({
            "nested": { "list": [1, 2.5, "three", true, null] },
            "empty": {},
        });
        let parsed = Json::parse(Json(value.clone()).to_value()).expect("it reads back");
        assert_eq!(parsed.0, value);
        // A value that cannot be turned into JSON is refused rather than turned
        // into something else.
        assert!(Json::parse(Value::Enum(async_graphql::Name::new("WORD"))).is_ok());
    }
}
