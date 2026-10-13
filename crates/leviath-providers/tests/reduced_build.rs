//! A build with providers left out, as `cargo install leviath-cli
//! --no-default-features --features ...` makes one.
//!
//! CI runs this file in a build with Anthropic alone, which is the only place
//! these tests exist: every other build has every provider, so a test here
//! is compiled only for the features it is about to miss. It is a file of
//! its own, rather than beside the code, because the crate's unit tests are
//! written for the build with every provider and do not compile without them.

/// A script provider in a build without them is refused, saying what to
/// rebuild with, rather than failing to compile or silently doing nothing.
#[cfg(not(feature = "rhai"))]
#[test]
fn a_script_provider_is_refused_with_what_to_rebuild_with() {
    let inspected = leviath_providers::rhai_provider::inspect_source(
        "p.rhai",
        "fn chat(state, request) { () }",
    );
    let said = inspected.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(said.contains("--features rhai"), "{said}");
    let missing = leviath_providers::compiled::scripts_missing().unwrap_or_default();
    assert!(missing.contains("--features rhai"), "{missing}");
}

/// A provider this build left out is named with the feature that builds it.
#[cfg(not(feature = "openai"))]
#[test]
fn a_provider_left_out_names_the_feature_that_builds_it() {
    let said = leviath_providers::compiled::missing("openai").unwrap_or_default();
    assert!(said.contains("--features openai"), "{said}");
}
