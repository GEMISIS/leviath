//! Script-backed checks on the bytes behind a media type.
//!
//! A media type is a claim. `MediaType::parse` checks the claim's spelling,
//! the registry's magic prefixes and extensions name bytes nobody typed, and
//! nothing in the tree asks whether an upload that arrived as `image/png`
//! is a PNG. For the built-in types that is a fair trade: a provider that is
//! handed a broken PNG says so. For a type of your own it is not, because
//! nothing downstream knows the format at all. So a registry row can name
//! a `check`, a `.rhai` file beside the file that declares it:
//!
//! ```rhai
//! // checks/scene.rhai: an ACME scene starts with its tag and holds at
//! // least one record.
//! fn check(bytes, media_type) {
//!     if bytes.len() < 8 { return "too short to be a scene file"; }
//!     if bytes.extract(0, 4) != "ACME".to_blob() { return "missing the ACME tag"; }
//!     ()   // fine
//! }
//! ```
//!
//! One function, one contract: **return `()` when the bytes are what they
//! claim, or a string saying why they are not**. The bytes arrive as a Rhai
//! blob and the type as a string, so one script can answer for a whole
//! family (`check = ...` on `image/*` sees `"image/png"`, `"image/webp"`).
//! The string goes back to whoever handed the bytes in (the model, the API
//! caller, the person attaching a file) and the bytes are not stored.
//!
//! Execution runs on a fresh hardened engine per call: no filesystem, no
//! network, no `eval`, operation-bounded. A check that throws, loops, or
//! returns something that is neither `()` nor a string is reported as broken
//! and the bytes are refused with that report, since a check that cannot
//! run is not a check that passed.

use rhai::{AST, Dynamic, Engine, Scope};

/// Operation budget for a check: a pass over one file's bytes, with room
/// for a byte-by-byte scan of a large one.
const CHECK_MAX_OPERATIONS: u64 = 5_000_000;

/// A compiled media check, ready to call.
///
/// Compiled once, when the registry is built (at daemon boot or a reload for
/// the operator's rows, at spawn for a blueprint's), so a broken script is a
/// load error rather than a surprise at the first file.
#[derive(Debug, Clone)]
pub struct MediaCheck {
    /// The script path as written in the row, for error context.
    pub path: String,
    ast: AST,
}

/// Build the hardened engine every check runs on.
fn build_engine() -> Engine {
    let mut engine = Engine::new();
    crate::harden(&mut engine, CHECK_MAX_OPERATIONS);
    crate::functions::register_functions(&mut engine);
    crate::types::register_types(&mut engine);
    engine
}

/// Compile a media check and check its shape.
///
/// `check(bytes, media_type)` must exist and take exactly two parameters. A
/// script that defines nothing, or defines it with the wrong arity, is
/// refused here rather than silently never running.
pub fn compile(path: &str, source: &str) -> crate::Result<MediaCheck> {
    let engine = build_engine();
    let ast = engine
        .compile(source)
        .map_err(|e| crate::Error::CompilationFailed(format!("{path}: {e}")))?;

    let arity = ast
        .iter_functions()
        .find(|f| f.name == "check")
        .map(|f| f.params.len());
    match arity {
        Some(2) => Ok(MediaCheck {
            path: path.to_string(),
            ast,
        }),
        Some(n) => Err(crate::Error::ValidationFailed(format!(
            "{path}: fn check must take exactly two parameters (bytes, media_type), found {n}"
        ))),
        None => Err(crate::Error::ValidationFailed(format!(
            "{path}: script must define fn check(bytes, media_type)"
        ))),
    }
}

/// What a check said about some bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The bytes are what they claim.
    Valid,
    /// They are not, for this reason. Goes back to the sender verbatim.
    Invalid(String),
    /// The check itself is broken: it threw, ran out of operations, or
    /// returned something that is neither `()` nor a string. Carries the
    /// error text, script path included.
    Unusable(String),
}

/// Run `check` over `bytes` claiming `media_type`.
pub fn check(check: &MediaCheck, media_type: &str, bytes: &[u8]) -> Verdict {
    let engine = build_engine();
    let blob: rhai::Blob = bytes.to_vec();
    let result: Result<Dynamic, _> = engine.call_fn(
        &mut Scope::new(),
        &check.ast,
        "check",
        (blob, media_type.to_string()),
    );
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            return Verdict::Unusable(format!("{}: check: {e}", check.path));
        }
    };
    if value.is_unit() {
        return Verdict::Valid;
    }
    match value.into_string() {
        // An empty string is easy to write by accident and unambiguous in
        // meaning, so it reads as "fine" rather than as a blank complaint.
        Ok(reason) if reason.trim().is_empty() => Verdict::Valid,
        Ok(reason) => Verdict::Invalid(reason),
        Err(actual) => Verdict::Unusable(format!(
            "{}: check must return () or a string, got {actual}",
            check.path
        )),
    }
}

/// The registry's side of the contract: a compiled script is a check the
/// core can run where bytes are stored. A script that cannot run refuses the
/// bytes with its own error, because bytes a check could not look at have
/// not passed it.
impl leviath_core::media::MediaCheck for MediaCheck {
    fn check(
        &self,
        media_type: &leviath_core::media::MediaType,
        bytes: &[u8],
    ) -> Result<(), String> {
        match check(self, media_type.as_str(), bytes) {
            Verdict::Valid => Ok(()),
            Verdict::Invalid(reason) => Err(reason),
            Verdict::Unusable(error) => Err(format!("the check could not run: {error}")),
        }
    }

    fn describe(&self) -> String {
        self.path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(source: &str) -> MediaCheck {
        compile("checks/test.rhai", source).expect("fixture compiles")
    }

    #[test]
    fn a_check_that_returns_unit_or_an_empty_string_accepts() {
        let c = compiled("fn check(bytes, media_type) { () }");
        assert_eq!(check(&c, "image/png", b"anything"), Verdict::Valid);
        let blank = compiled(r#"fn check(bytes, media_type) { "  " }"#);
        assert_eq!(check(&blank, "image/png", b"anything"), Verdict::Valid);
    }

    /// The realistic shape: look at the bytes and the type, complain
    /// specifically.
    #[test]
    fn a_check_sees_the_bytes_as_a_blob_and_the_type_as_a_string() {
        let c = compiled(
            r#"
            fn check(bytes, media_type) {
                if media_type != "application/x-acme-scene" {
                    return "asked about " + media_type;
                }
                if bytes.len() < 8 { return "too short to be a scene file"; }
                if bytes.extract(0, 4) != "ACME".to_blob() { return "missing the ACME tag"; }
                ()
            }
            "#,
        );
        assert_eq!(
            check(&c, "application/x-acme-scene", b"ACME\x00\x00\x00\x01"),
            Verdict::Valid
        );
        assert_eq!(
            check(&c, "application/x-acme-scene", b"ACME"),
            Verdict::Invalid("too short to be a scene file".to_string())
        );
        assert_eq!(
            check(&c, "application/x-acme-scene", b"NOPE\x00\x00\x00\x01"),
            Verdict::Invalid("missing the ACME tag".to_string())
        );
        assert_eq!(
            check(&c, "model/obj", b"ACME\x00\x00\x00\x01"),
            Verdict::Invalid("asked about model/obj".to_string())
        );
    }

    #[test]
    fn a_check_that_throws_or_returns_the_wrong_type_is_unusable() {
        let unusable = |source: &str| {
            let verdict = format!("{:?}", check(&compiled(source), "image/png", b"x"));
            assert!(verdict.starts_with("Unusable("), "{verdict}");
            verdict
        };
        let thrown = unusable(r#"fn check(bytes, media_type) { throw "boom" }"#);
        assert!(thrown.contains("boom"), "{thrown}");
        let number = unusable("fn check(bytes, media_type) { 42 }");
        assert!(number.contains("() or a string"), "{number}");
        let runaway = unusable("fn check(bytes, media_type) { loop {} }");
        assert!(runaway.contains("checks/test.rhai"), "{runaway}");
    }

    #[test]
    fn the_shape_is_checked_at_compile_time() {
        let none = compile("c.rhai", "fn other() { () }").unwrap_err();
        assert!(
            none.to_string()
                .contains("must define fn check(bytes, media_type)"),
            "{none}"
        );
        let one = compile("c.rhai", "fn check(bytes) { () }").unwrap_err();
        assert!(one.to_string().contains("exactly two parameters"), "{one}");
        let syntax = compile("c.rhai", "fn check(bytes, media_type) {").unwrap_err();
        assert!(syntax.to_string().contains("c.rhai"), "{syntax}");
    }

    /// The core sees a compiled script as a check: a rejection is the reason,
    /// and a script that cannot run refuses the bytes rather than passing them.
    #[test]
    fn the_core_trait_turns_verdicts_into_results() {
        use leviath_core::media::{MediaCheck as _, MediaType};
        let png = MediaType::parse("image/png").unwrap();
        let ok = compiled("fn check(bytes, media_type) { () }");
        assert_eq!(ok.check(&png, b"x"), Ok(()));
        assert_eq!(ok.describe(), "checks/test.rhai");
        let no = compiled(r#"fn check(bytes, media_type) { "not a png" }"#);
        assert_eq!(no.check(&png, b"x"), Err("not a png".to_string()));
        let broken = compiled(r#"fn check(bytes, media_type) { throw "boom" }"#);
        let err = broken.check(&png, b"x").unwrap_err();
        assert!(err.starts_with("the check could not run:"), "{err}");
        assert!(err.contains("boom"), "{err}");
        assert!(format!("{broken:?}").contains("checks/test.rhai"));
    }
}
