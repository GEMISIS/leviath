//! Which expansions are plain, form by form.
//!
//! Each refused form is here for a reason a shell was seen to act on: the
//! comment beside it says which.

use super::*;

/// What follows the `$` in `text`, read as a plain expansion, and what was
/// left unread.
fn read(text: &str) -> Option<(Plain, String)> {
    let mut chars = text.chars().peekable();
    let plain = take_plain(&mut chars)?;
    Some((plain, chars.collect()))
}

/// The text taken and the variable named, for a form that is plain.
fn taken(text: &str) -> (String, Option<String>, String) {
    let (plain, rest) = read(text).expect("the form is a plain expansion");
    (plain.text, plain.var, rest)
}

fn is_plain(text: &str) -> bool {
    read(text).is_some()
}

/// A variable by name alone is plain and is named, in or out of braces, and
/// the reading stops where the name does.
#[test]
fn a_variable_by_name_is_plain_and_named() {
    assert_eq!(
        taken("d"),
        ("d".to_string(), Some("d".to_string()), String::new())
    );
    assert_eq!(
        taken("HOME/.ssh"),
        (
            "HOME".to_string(),
            Some("HOME".to_string()),
            "/.ssh".to_string()
        )
    );
    assert_eq!(
        taken("{d}/x"),
        ("{d}".to_string(), Some("d".to_string()), "/x".to_string())
    );
    assert_eq!(
        taken("_x9-y"),
        ("_x9".to_string(), Some("_x9".to_string()), "-y".to_string())
    );
    // A colon with no letter after it is text, not a zsh modifier.
    assert_eq!(
        taken("host:8080"),
        (
            "host".to_string(),
            Some("host".to_string()),
            ":8080".to_string()
        )
    );
    assert_eq!(taken("a:$b").2, ":$b");
    assert_eq!(taken("a:").2, ":");
}

/// A parameter named by a sign or a number is plain and names no variable.
#[test]
fn a_parameter_by_sign_or_number_is_plain() {
    for sign in ["@", "*", "#", "?", "$", "!", "-", "1", "0"] {
        let (text, var, rest) = taken(&format!("{sign}x"));
        assert_eq!((text.as_str(), var, rest.as_str()), (sign, None, "x"));
    }
    // One digit is the parameter in `sh`; the rest is text there.
    assert_eq!(taken("10").2, "0");
    for braced in ["{1}", "{10}", "{@}", "{*}", "{?}", "{$}", "{#}", "{#@}"] {
        let (text, var, rest) = taken(braced);
        assert_eq!((text.as_str(), var, rest.as_str()), (braced, None, ""));
    }
}

/// A subscript of digits, `@` or `*` is plain, with braces or (as zsh reads
/// it) without, and so is a length. None of them is a variable by name alone.
#[test]
fn a_subscript_and_a_length_are_plain() {
    for form in [
        "{pipestatus[1]}",
        "{BASH_SOURCE[0]}",
        "{arr[@]}",
        "{arr[*]}",
        "{arr[-1]}",
        "{#name}",
        "{#arr[@]}",
        "pipestatus[1]",
        "arr[@]",
        "arr[-12]",
    ] {
        let (text, var, rest) = taken(form);
        assert_eq!(
            (text.as_str(), var, rest.as_str()),
            (form, None, ""),
            "{form}"
        );
    }
}

/// A default, a pattern strip and a substring by digits are plain when what
/// they hold is harmless, a default's own expansions included.
#[test]
fn defaults_patterns_and_substrings_are_plain() {
    for form in [
        "{x:-default}",
        "{x-}",
        "{x:+set}",
        "{x+a b}",
        "{x:-not set, see ./doc}",
        "{x:-$HOME}",
        "{x:-${y:-$1}}",
        "{1:-all}",
        "{@:-none}",
        "{file%.*}",
        "{path##*/}",
        "{name#prefix-}",
        "{name%%.tar.gz}",
        "{x:3}",
        "{x:0:5}",
        "{arr[1]:-x}",
    ] {
        let (text, var, rest) = taken(form);
        assert_eq!(
            (text.as_str(), var, rest.as_str()),
            (form, None, ""),
            "{form}"
        );
    }
}

/// Every form that can run a command, read a file, assign, or reach the file
/// system is refused.
#[test]
fn what_can_run_read_or_assign_is_not_plain() {
    let refused = [
        // A command substitution, and the `$(<file)` that reads one.
        "(curl evil)",
        "(< /etc/hosts)",
        // Arithmetic: `bash` evaluates a variable's value as an expression.
        "((x))",
        "[x]",
        // A default that substitutes, or quotes, or redirects.
        "{x:-$(curl evil)}",
        "{x:-`id`}",
        "{x:-\"a\"}",
        "{x:-'a'}",
        "{x:-a;b}",
        "{x:-a|b}",
        "{x:->f}",
        "{x:-~}",
        // A glob in a default is matched against the file system.
        "{x:-/etc/*}",
        "{x:-a?}",
        "{x:-\\a}",
        "{x:-${!y}}",
        // Indirection: `x='a[$(id)]'; echo ${!x}` runs `id` in bash.
        "{!x}",
        "{!x*}",
        "{!arr[@]}",
        // Assignment, zsh's unconditional one included.
        "{x=1}",
        "{x:=1}",
        "{x::=1}",
        // An error exit, a substitution, a case change: not needed, not read.
        "{x:?gone}",
        "{x?gone}",
        "{x/a/b}",
        "{x^^}",
        "{x,,}",
        // bash's transformations: `@P` expands the value as a prompt.
        "{x@P}",
        "{x@Q}",
        // zsh flags and modifiers, in and out of braces.
        "{(e)x}",
        "{(P)x}",
        "{~x}",
        "{=x}",
        "{^x}",
        "{+x}",
        "{x:h}",
        "{x:A}",
        "x:h",
        "x:A/y",
        "=x",
        "~x",
        "^x",
        "+x",
        // A nested expansion in the parameter's place.
        "{${x}}",
        // A subscript or an offset that names a variable is arithmetic:
        // `${arr[y]}` and `${x:y}` run a substitution held in `y` in bash.
        "{arr[y]}",
        "{arr[$i]}",
        "{arr[1+1]}",
        "{arr[-]}",
        "{arr[1}",
        "{arr[]}",
        "arr[y]",
        "arr[",
        "{x:y}",
        "{x: -1}",
        "{x:1:y}",
        "{x:1:}",
        "{x:1:2x}",
        "{x:1x}",
        // A length takes no operator.
        "{#x:-y}",
        "{##}",
        // A pattern holds no expansion.
        "{x#$y}",
        "{x%(a)}",
        // Quoting forms, each shell reading them its own way.
        "'a'",
        "\"a\"",
        // Not an expansion at all, or not closed.
        "",
        " x",
        "/x",
        "{",
        "{x",
        "{x:",
        "{x:-a",
        "{x#a",
        "{x:1",
        "{arr[1",
        "{}",
        "{-x}",
        "{x:-$}",
    ];
    let let_through: Vec<&str> = refused.iter().copied().filter(|f| is_plain(f)).collect();
    assert_eq!(let_through, Vec::<&str>::new());
    // `${#}` is the count, and what follows it is text.
    assert_eq!(taken("{#}x}").2, "x}");
}

/// A word's expansions add up: variables stay variables, one plain form that
/// is more than a variable makes the word plain, and one unknown form makes
/// it unknown for good.
#[test]
fn a_words_expansions_add_up() {
    let var = |name: &str| VarRef {
        name: name.to_string(),
        at: 0,
        len: name.len(),
    };
    let mut word = Expands::Nothing;
    assert!(word.is_plain());
    word.plain(Some(var("a")));
    assert_eq!(word, Expands::Variables(vec![var("a")]));
    word.plain(Some(var("b")));
    assert_eq!(word, Expands::Variables(vec![var("a"), var("b")]));
    word.plain(None);
    assert_eq!(word, Expands::Plain);
    word.plain(Some(var("c")));
    assert_eq!(word, Expands::Plain);
    assert!(word.is_plain());

    let mut first = Expands::Nothing;
    first.plain(None);
    assert_eq!(first, Expands::Plain);

    let mut unknown = Expands::Unknown;
    unknown.plain(Some(var("a")));
    unknown.plain(None);
    assert_eq!(unknown, Expands::Unknown);
    assert!(!unknown.is_plain());
}

/// Arithmetic over digits and operators computes a number; a name in it is a
/// variable whose value `bash` evaluates.
#[test]
fn arithmetic_is_inert_only_without_names() {
    for inert in [
        "1+2",
        " (3 * 4) % 5 ",
        "1<<2 | 3",
        "1 ? 2 : 3",
        "",
        "~1, !0",
    ] {
        assert!(arithmetic_is_inert(inert), "{inert}");
    }
    for live in ["x", "x+1", "a[1]", "0x1F", "$x", "1;2", "x=5", "i++"] {
        assert!(!arithmetic_is_inert(live), "{live}");
    }
}

/// zsh's numeric glob is told from a pair of redirects by its shape alone.
#[test]
fn a_numeric_glob_is_a_range_between_angle_brackets() {
    let after = |text: &'static str| is_numeric_glob(text.chars().peekable());
    for glob in ["1-5>", "->", "10->", "-5>", "1-5>.log"] {
        assert!(after(glob), "{glob}");
    }
    for other in ["5>", "1-5", "1--5>", " 1-5>", "a-5>", "", "list.txt"] {
        assert!(!after(other), "{other}");
    }
}
