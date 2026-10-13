//! Tests for the workdir part reader.
//!
//! The failures are the point: each one is a different thing for the caller to
//! do about, so each is checked for its own kind rather than only for "it
//! failed". The REST rendering of the same four is pinned beside the handler in
//! `upload.rs`, which is what holds the status codes still.

use leviath_core::mime::Delivery;

use super::{FileBudget, delivery, mime_type, read_within};
use crate::commands::serve::core::error::ServeError;

/// A few bytes that sniff as a PNG.
fn png() -> Vec<u8> {
    b"\x89PNG\r\n\x1a\nbody".to_vec()
}

/// A file inside the working directory reads, and carries its own file name.
#[test]
fn a_file_inside_the_workdir_reads_as_a_part() {
    let dir = tempfile::tempdir().expect("a temp workdir");
    std::fs::write(dir.path().join("hero.png"), png()).expect("the file is written");
    let part = read_within("hero.png", dir.path(), &FileBudget::new(1024)).expect("it reads");
    assert_eq!(part.name, "hero.png");
    assert_eq!(part.data, png());
}

/// Each refusal keeps its own kind: escaping the directory is forbidden, a
/// missing or empty file is the request's own fault, and an oversize one is
/// too large.
#[test]
fn each_refusal_says_which_kind_it_is() {
    let dir = tempfile::tempdir().expect("a temp workdir");
    std::fs::write(dir.path().join("hero.png"), png()).expect("the file is written");
    std::fs::write(dir.path().join("empty.png"), b"").expect("the empty file is written");

    let code = |path: &str, max: u64| {
        read_within(path, dir.path(), &FileBudget::new(max))
            .expect_err("refused")
            .code()
    };
    assert_eq!(code("../escape.png", 1024), "FORBIDDEN");
    assert_eq!(code("missing.png", 1024), "BAD_USER_INPUT");
    assert_eq!(code("empty.png", 1024), "BAD_USER_INPUT");
    assert_eq!(code("hero.png", 4), "PAYLOAD_TOO_LARGE");
}

/// What is not a regular file (a directory here; a device on Unix too) is
/// refused rather than read.
#[test]
fn what_is_not_a_file_is_refused() {
    let dir = tempfile::tempdir().expect("a temp workdir");
    std::fs::create_dir_all(dir.path().join("folder")).expect("the folder is made");
    let refused = read_within("folder", dir.path(), &FileBudget::new(1024)).expect_err("refused");
    assert_eq!(refused.code(), "BAD_USER_INPUT");
    assert!(refused.to_string().contains("is not a file"), "{refused}");
}

/// The files one request names are held to one budget between them: each is
/// read against what the ones before it left.
#[test]
fn the_files_a_request_names_share_one_budget() {
    let dir = tempfile::tempdir().expect("a temp workdir");
    std::fs::write(dir.path().join("a.txt"), [b'a'; 10]).expect("written");
    std::fs::write(dir.path().join("b.txt"), [b'b'; 10]).expect("written");
    let budget = FileBudget::new(15);
    assert!(read_within("a.txt", dir.path(), &budget).is_ok());
    let refused = read_within("b.txt", dir.path(), &budget).expect_err("over what was left");
    assert_eq!(refused.code(), "PAYLOAD_TOO_LARGE");
    assert!(
        refused.to_string().contains("over the 5 bytes"),
        "{refused}"
    );
    assert!(read_within("a.txt", dir.path(), &FileBudget::new(10)).is_ok());
}

/// A file far over the budget is refused by its size, read no further than
/// one byte past the budget.
#[test]
fn a_large_file_is_refused_by_its_size() {
    let dir = tempfile::tempdir().expect("a temp workdir");
    let big = std::fs::File::create(dir.path().join("big.bin")).expect("created");
    big.set_len(64 << 20).expect("sized");
    let refused = read_within("big.bin", dir.path(), &FileBudget::new(16)).expect_err("too large");
    assert_eq!(refused.code(), "PAYLOAD_TOO_LARGE");
    assert!(refused.to_string().contains("67108864 bytes"), "{refused}");
}

/// The three `deliver` words are read, and anything else is refused with the
/// list of the ones that work.
#[test]
fn the_delivery_words_are_the_three_the_runtime_knows() {
    assert_eq!(delivery("native").expect("a word"), Delivery::Native);
    assert_eq!(delivery("text").expect("a word"), Delivery::Text);
    assert_eq!(delivery("stand_in").expect("a word"), Delivery::StandIn);
    let refused = delivery("loud").expect_err("not a word");
    assert_eq!(refused.code(), "BAD_USER_INPUT");
    assert!(refused.to_string().contains("stand_in"), "{refused}");
}

/// A declared type is parsed, and one that is not a type names the part it was
/// sent for.
#[test]
fn a_declared_type_is_read_or_named_in_the_refusal() {
    let parsed = mime_type("hero.png", "image/png").expect("a type");
    assert_eq!(parsed.as_str(), "image/png");
    let refused = mime_type("hero.png", "nope").expect_err("not a type");
    let ServeError::BadRequest(message) = &refused else {
        panic!("a type that will not parse is the request's own fault, not {refused}");
    };
    assert!(message.contains("hero.png"), "{message}");
}
