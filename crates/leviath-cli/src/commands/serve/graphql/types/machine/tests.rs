//! Tests for the machine-side types that read a word and answer a value.
//!
//! The server descriptions these read from are shared with the REST routes, so
//! the words are fixed there and the mapping is what this schema adds. What is
//! asserted is that every word those routes can carry has a value here, and
//! that a word none of them carries lands somewhere safe rather than panicking.

use super::{McpAuth, McpServerTransport};

/// Every transport word the server description carries, and one it never does.
#[test]
fn every_transport_word_reads_back() {
    assert_eq!(
        McpServerTransport::from_wire("stdio"),
        McpServerTransport::Stdio
    );
    assert_eq!(
        McpServerTransport::from_wire("http"),
        McpServerTransport::Http
    );
    assert_eq!(
        McpServerTransport::from_wire("invalid"),
        McpServerTransport::Invalid
    );
    // A word this build does not know is a transport it cannot use, which is
    // the same thing `INVALID` says.
    assert_eq!(
        McpServerTransport::from_wire("carrier-pigeon"),
        McpServerTransport::Invalid
    );
}

/// Every auth word, and the reading an unknown one gets.
#[test]
fn every_auth_word_reads_back() {
    assert_eq!(McpAuth::from_wire("n/a"), McpAuth::NotApplicable);
    assert_eq!(McpAuth::from_wire("none"), McpAuth::None);
    assert_eq!(McpAuth::from_wire("header"), McpAuth::Header);
    assert_eq!(McpAuth::from_wire("authenticated"), McpAuth::Authenticated);
    assert_eq!(McpAuth::from_wire("expired"), McpAuth::Expired);
    // `NONE` rather than anything else: it is the reading that offers a login
    // instead of assuming a credential is already in place.
    assert_eq!(McpAuth::from_wire("who knows"), McpAuth::None);
}
