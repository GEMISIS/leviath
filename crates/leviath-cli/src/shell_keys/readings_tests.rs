//! How one argument is read as paths.

use super::*;

/// A plain word, a long flag and a short flag are read as before: the tail
/// reading that catches `-f../list` and `-rf/etc/x` is untouched.
#[test]
fn candidates_read_flags_the_way_a_program_might() {
    assert_eq!(candidates("src"), ["src"]);
    assert_eq!(candidates("a=b"), ["a=b"]);
    assert_eq!(candidates("-"), ["-"]);
    assert_eq!(candidates("--"), ["--"]);
    assert_eq!(candidates("--file=x"), ["x"]);
    assert_eq!(candidates("--verbose"), ["--verbose"]);
    assert_eq!(candidates("-rn"), ["n"]);
    assert!(candidates("-r").is_empty());
    assert_eq!(candidates("-f/x"), ["/x", "x"]);
    assert_eq!(
        candidates("-f../list"),
        ["../list", "./list", "/list", "list", "ist", "st", "t"]
    );
    assert_eq!(
        candidates("-rf/etc/x"),
        ["f/etc/x", "/etc/x", "etc/x", "tc/x", "c/x", "/x", "x"]
    );
    // A character that takes more than one byte is never cut in half.
    assert_eq!(candidates("-fé"), ["é"]);
}

/// `-name=value` is its value, and each reading that starts inside the name:
/// the tails inside the value, which made `-gdir=res://tests` read as
/// `/tests`, are gone.
#[test]
fn a_short_flag_with_an_equals_is_read_as_its_value() {
    assert_eq!(
        candidates("-gdir=res://tests"),
        [
            "dir=res://tests",
            "ir=res://tests",
            "r=res://tests",
            "=res://tests",
            "res://tests"
        ]
    );
    assert_eq!(
        candidates("-o=/etc/passwd"),
        ["=/etc/passwd", "/etc/passwd"]
    );
    assert_eq!(
        candidates("-D_x-y=1"),
        ["_x-y=1", "x-y=1", "-y=1", "y=1", "=1", "1"]
    );
    // No value: only the readings inside the name.
    assert_eq!(candidates("-o="), ["="]);
    // What stands before the `=` is not a flag's name, so the value may start
    // anywhere: every tail is read, as for any short flag.
    assert_eq!(candidates("-f/a=b"), ["/a=b", "a=b", "=b", "b"]);
    assert_eq!(candidates("-1=/x"), ["=/x", "/x", "x"]);
    assert_eq!(candidates("-=/x"), ["/x", "x"]);
}

/// A URL glued to a short flag is read up to its colon and then as what
/// follows its `://`. A tail in front of the scheme is still read, so a path
/// in front of a made-up URL is still seen.
#[test]
fn a_url_glued_to_a_short_flag_is_not_read_as_tails_of_itself() {
    assert_eq!(
        candidates("-ohttp://host/x"),
        [
            "http://host/x",
            "ttp://host/x",
            "tp://host/x",
            "p://host/x",
            "host/x"
        ]
    );
    // The scheme is at the dash: there is no tail before the colon.
    assert_eq!(candidates("-x://etc/passwd"), ["etc/passwd"]);
    assert_eq!(candidates("-x:///etc/passwd"), ["/etc/passwd"]);
    // Nothing after the `://`.
    assert_eq!(
        candidates("-ohttp://"),
        ["http://", "ttp://", "tp://", "p://"]
    );
    // The path in front of the URL keeps its tails.
    assert_eq!(
        candidates("-rf/etc/x://y"),
        [
            "f/etc/x://y",
            "/etc/x://y",
            "etc/x://y",
            "tc/x://y",
            "c/x://y",
            "/x://y",
            "x://y",
            "y"
        ]
    );
    // One letter is a scheme too.
    assert_eq!(candidates("-f://x"), ["x"]);
    // No scheme in front of the `://`: not a URL, every tail is read.
    assert_eq!(candidates("-I/://x"), ["/://x", "://x", "//x", "/x", "x"]);
    assert_eq!(
        candidates("-1.2://x"),
        [".2://x", "2://x", "://x", "//x", "/x", "x"]
    );
}

/// A word that is no URL is spelled one way.
#[test]
fn a_plain_word_has_one_spelling() {
    for word in [
        "src/main.rs",
        "/etc/hosts",
        "a:b",
        "x:/y",
        "://x",
        "1x://y",
        "a b://c",
    ] {
        assert_eq!(spellings(word), Some(vec![word]), "{word}");
    }
}

/// A URL is read as it is spelled and as what follows its scheme, however
/// many schemes lead it. The second reading is what catches a real path
/// behind a made-up prefix.
#[test]
fn a_url_is_read_with_and_without_its_scheme() {
    assert_eq!(spellings("res://tests"), Some(vec!["res://tests", "tests"]));
    assert_eq!(
        spellings("https://example.com/a?b=1"),
        Some(vec!["https://example.com/a?b=1", "example.com/a?b=1"])
    );
    assert_eq!(
        spellings("x:///etc/passwd"),
        Some(vec!["x:///etc/passwd", "/etc/passwd"])
    );
    assert_eq!(
        spellings("x://../../etc/passwd"),
        Some(vec!["x://../../etc/passwd", "../../etc/passwd"])
    );
    assert_eq!(
        spellings("a+b.c-d://e://f"),
        Some(vec!["a+b.c-d://e://f", "e://f", "f"])
    );
    assert_eq!(spellings("res://"), Some(vec!["res://", ""]));
}

/// A `file:` URL names a path, wherever in the word it starts and however
/// its scheme is cased; one that names another host or hides its path in
/// percent-encoding cannot be placed.
#[test]
fn a_file_url_is_read_as_its_path() {
    assert_eq!(
        spellings("file:///etc/passwd"),
        Some(vec!["file:///etc/passwd", "/etc/passwd", "/etc/passwd"])
    );
    assert_eq!(
        spellings("file://localhost/etc/passwd"),
        Some(vec![
            "file://localhost/etc/passwd",
            "localhost/etc/passwd",
            "/etc/passwd"
        ])
    );
    assert_eq!(
        spellings("FILE://LocalHost/etc/passwd").map(|s| s[2]),
        Some("/etc/passwd")
    );
    assert_eq!(
        spellings("file:/etc/passwd"),
        Some(vec!["file:/etc/passwd", "/etc/passwd"])
    );
    assert_eq!(
        spellings("file:notes.md"),
        Some(vec!["file:notes.md", "notes.md"])
    );
    assert_eq!(spellings("file://"), Some(vec!["file://", "", "/"]));
    assert_eq!(
        spellings("jar:file:/etc/x.jar!/a"),
        Some(vec!["jar:file:/etc/x.jar!/a", "/etc/x.jar!/a"])
    );
    assert_eq!(
        spellings("git+file:///srv/repo"),
        Some(vec!["git+file:///srv/repo", "/srv/repo", "/srv/repo"])
    );
    // A word that only ends in the letters is not a URL.
    assert_eq!(spellings("profile:/x"), Some(vec!["profile:/x"]));
    for unplaceable in [
        "file://fileserver/share/x",
        "file:///work/%2e%2e/%2e%2e/etc/passwd",
        "file:/etc/%70asswd",
        "x=file://host/x",
    ] {
        assert_eq!(spellings(unplaceable), None, "{unplaceable}");
    }
}

/// A value is also read in the parts a program may take it apart into: what
/// stands between its `:`, `,` and `=`.
#[test]
fn a_value_is_read_in_its_parts_too() {
    assert_eq!(parts("x:/etc/hosts"), ["x", "/etc/hosts"]);
    assert_eq!(parts("if=/etc/hosts"), ["if", "/etc/hosts"]);
    assert_eq!(parts("a,/etc/hosts,b"), ["a", "/etc/hosts", "b"]);
    assert_eq!(parts("x=a:~/y,../z"), ["x", "a", "~/y", "../z"]);
    assert_eq!(parts("res://tests"), ["res", "//tests"]);
    assert_eq!(parts("12:30"), ["12", "30"]);
    assert_eq!(parts("::1"), ["1"]);
    // Nothing to take apart: the value has no reading but the whole.
    assert!(parts("src/main.rs").is_empty());
    assert!(parts("").is_empty());

    fn read(value: &str) -> Option<Vec<(&str, bool)>> {
        readings(value).map(|all| all.iter().map(|r| (r.text, r.part)).collect())
    }
    assert_eq!(read("src/main.rs"), Some(vec![("src/main.rs", false)]));
    assert_eq!(
        read("x:/etc/hosts"),
        Some(vec![
            ("x:/etc/hosts", false),
            ("x", true),
            ("/etc/hosts", true)
        ])
    );
    assert_eq!(
        read("res://tests"),
        Some(vec![
            ("res://tests", false),
            ("tests", false),
            ("res", true),
            ("//tests", true)
        ])
    );
    assert_eq!(read("file://server/share"), None);
}
