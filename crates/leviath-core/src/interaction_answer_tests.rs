//! Tests for [`super`]: the options every surface lists, and reading an answer
//! against them.

use super::*;

fn approval() -> InteractionRequest {
    InteractionRequest::tool_approval("a", "bash", serde_json::json!({}), "s", &[])
}

fn choice(labels: &[&str]) -> InteractionRequest {
    InteractionRequest::multiple_choice(
        "c",
        "Pick",
        labels.iter().map(|l| l.to_string()).collect(),
        "s",
    )
}

/// The words, in listing order.
fn words(req: &InteractionRequest) -> Vec<String> {
    answer_options(req).into_iter().map(|o| o.id).collect()
}

#[test]
fn a_tool_approval_lists_its_options_under_fixed_words_numbered_from_one() {
    let options = answer_options(&InteractionRequest::tool_approval(
        "run-approve-1",
        "bash",
        serde_json::json!({"command": "ls"}),
        "s",
        &["shell:ls".to_string()],
    ));
    let rows: Vec<(&str, usize)> = options.iter().map(|o| (o.id.as_str(), o.number)).collect();
    assert_eq!(
        rows,
        [
            ("allow", 1),
            ("allow-stage", 2),
            ("allow-run", 3),
            ("deny", 4),
            ("deny-feedback", 5)
        ]
    );
    assert_eq!(options[1].label, "Allow ls for this stage");
    assert_eq!(options[0].answer, "lev respond run-approve-1 allow");
    assert_eq!(
        options[4].answer,
        "lev respond run-approve-1 deny --feedback \"TEXT\""
    );
    // What an agent reads: the word, the label, the number and the command,
    // and nothing internal.
    let json = serde_json::to_value(&options[2]).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "id": "allow-run",
            "label": "Allow ls for this run",
            "number": 3,
            "answer": "lev respond run-approve-1 allow-run",
        })
    );
}

/// A gate offers no stage scope, so its run option is number 2, and the word
/// still says run: the meaning is read off the label, not the position.
#[test]
fn a_gate_approval_numbers_its_own_three_options() {
    let gate = InteractionRequest::gate_approval("g", "web_fetch", serde_json::json!({}), "s");
    assert_eq!(words(&gate), ["allow", "allow-run", "deny"]);
    assert_eq!(
        parse_answer(&gate, "2", None),
        Ok(InteractionResponse::approval("g", true, ApprovalScope::Run))
    );
    assert_eq!(
        parse_answer(&gate, "allow-stage", None).unwrap_err(),
        "\"allow-stage\" is not an answer to this approval; answer allow, allow-run or deny (or 1-3)"
    );
}

/// An approval that lists nothing still answers to the four words, and a
/// label nobody knows is a deny under a word made from it: an answer this
/// cannot read must never let a call through.
#[test]
fn an_approval_with_no_or_unknown_options_never_approves_by_accident() {
    let mut bare = approval();
    bare.options.clear();
    assert_eq!(words(&bare), ["allow", "allow-stage", "allow-run", "deny"]);

    let mut odd = approval();
    odd.options = vec!["Ship it!".to_string()];
    assert_eq!(words(&odd), ["ship-it"]);
    assert_eq!(
        parse_answer(&odd, "ship-it", None),
        Ok(InteractionResponse::approval(
            "a",
            false,
            ApprovalScope::Once
        ))
    );
}

#[test]
fn an_approval_answers_by_word_number_or_label_in_any_case() {
    let req = approval();
    for (typed, scope) in [
        ("allow", ApprovalScope::Once),
        ("ALLOW", ApprovalScope::Once),
        ("1", ApprovalScope::Once),
        (" Allow once ", ApprovalScope::Once),
        ("allow-stage", ApprovalScope::Stage),
        ("2", ApprovalScope::Stage),
        ("allow-run", ApprovalScope::Run),
        ("3", ApprovalScope::Run),
    ] {
        assert_eq!(
            parse_answer(&req, typed, None),
            Ok(InteractionResponse::approval("a", true, scope)),
            "{typed}"
        );
    }
    let deny = InteractionResponse::approval("a", false, ApprovalScope::Once);
    assert_eq!(parse_answer(&req, "deny", None), Ok(deny.clone()));
    assert_eq!(parse_answer(&req, "4", None), Ok(deny));
}

#[test]
fn a_deny_carries_feedback_and_nothing_else_does() {
    let req = approval();
    let redirect = InteractionResponse::deny_with_feedback("a", "use git log");
    assert_eq!(
        parse_answer(&req, "deny", Some("use git log")),
        Ok(redirect.clone())
    );
    assert_eq!(
        parse_answer(&req, "deny-feedback", Some("use git log")),
        Ok(redirect.clone())
    );
    assert_eq!(parse_answer(&req, "5", Some("use git log")), Ok(redirect));
    assert_eq!(
        parse_answer(&req, "5", None).unwrap_err(),
        "deny-feedback needs the words for the model: answer deny with feedback, as in \
         lev respond a deny --feedback \"TEXT\""
    );
    assert_eq!(
        parse_answer(&req, "allow", Some("why")).unwrap_err(),
        "feedback goes with a denial, and allow is not one"
    );
}

/// A wrong answer is refused with every right one, ready to copy. An approval
/// takes no guesses at a start: "al" could be three grants.
#[test]
fn a_wrong_approval_answer_lists_the_right_ones() {
    let mut req = approval();
    req.options.pop();
    for typed in ["maybe", "al", "0", "9", ""] {
        assert_eq!(
            parse_answer(&req, typed, None).unwrap_err(),
            format!(
                "\"{typed}\" is not an answer to this approval; answer allow, allow-stage, \
                 allow-run or deny (or 1-4)"
            )
        );
    }
}

#[test]
fn a_confirm_answers_yes_or_no_by_word_or_number() {
    let req = InteractionRequest::confirm("y", "Sure?", "s");
    let yes = InteractionResponse::approval("y", true, ApprovalScope::Once);
    let no = InteractionResponse::approval("y", false, ApprovalScope::Once);
    for (typed, want) in [
        ("yes", &yes),
        ("YES", &yes),
        ("1", &yes),
        ("no", &no),
        ("2", &no),
    ] {
        assert_eq!(
            parse_answer(&req, typed, None).as_ref(),
            Ok(want),
            "{typed}"
        );
    }
    assert_eq!(
        parse_answer(&req, "no", Some("not yet")),
        Ok(InteractionResponse::deny_with_feedback("y", "not yet"))
    );
    assert_eq!(
        parse_answer(&req, "maybe", None).unwrap_err(),
        "\"maybe\" is not an answer to this confirmation; answer yes or no (or 1-2)"
    );
    // A confirm with no labels of its own still reads Yes and No.
    let mut bare = req.clone();
    bare.options.clear();
    let labels: Vec<String> = answer_options(&bare).into_iter().map(|o| o.label).collect();
    assert_eq!(labels, ["Yes", "No"]);
}

/// A choice's word is made from its own label, so it stays with the option
/// when the list is reordered.
#[test]
fn a_choice_word_comes_from_its_label() {
    let req = choice(&[
        "Use Postgres",
        "SQLite (embedded)",
        "  ",
        "Use Postgres",
        "Use Postgres",
    ]);
    assert_eq!(
        words(&req),
        [
            "use-postgres",
            "sqlite-embedded",
            "option",
            "use-postgres-2",
            "use-postgres-3"
        ]
    );
    let reordered = choice(&["SQLite (embedded)", "Use Postgres"]);
    assert_eq!(words(&reordered), ["sqlite-embedded", "use-postgres"]);
}

#[test]
fn a_choice_answers_by_label_word_start_or_number() {
    let req = choice(&["Postgres", "SQLite", "SQL Server"]);
    for (typed, index) in [
        ("Postgres", 0),
        ("postgres", 0),
        ("post", 0),
        ("P", 0),
        ("sqlite", 1),
        ("sql-server", 2),
        ("SQL S", 2),
        ("3", 2),
    ] {
        assert_eq!(
            parse_answer(&req, typed, None),
            Ok(InteractionResponse::choice("c", index)),
            "{typed}"
        );
    }
    assert_eq!(
        parse_answer(&req, "sql", None).unwrap_err(),
        "\"sql\" could be any of sqlite or sql-server; give more of it, or its number"
    );
    assert_eq!(
        parse_answer(&req, "mysql", None).unwrap_err(),
        "\"mysql\" is not an answer to this choice; answer with an option or the start of one: \
         postgres, sqlite or sql-server (or 1-3)"
    );
    assert!(parse_answer(&req, "  ", None).is_err());
    assert_eq!(
        parse_answer(&req, "postgres", Some("why")).unwrap_err(),
        "feedback goes with a denial, and postgres is not one"
    );
}

/// A label that is itself a number another option is listed under could mean
/// either, so it is refused naming both rather than settled by a rule nobody
/// can see. A whole label wins over the start of a longer one.
#[test]
fn a_number_that_is_also_a_label_is_refused_as_ambiguous() {
    let req = choice(&["2", "1"]);
    assert_eq!(
        parse_answer(&req, "1", None).unwrap_err(),
        "\"1\" could be any of 2 or 1; give more of it, or its number"
    );
    let one = choice(&["go"]);
    assert_eq!(
        parse_answer(&one, "1", None),
        Ok(InteractionResponse::choice("c", 0))
    );
    assert_eq!(
        parse_answer(&one, "stop", None).unwrap_err(),
        "\"stop\" is not an answer to this choice; answer with an option or the start of one: go (or 1-1)"
    );
    let nested = choice(&["Go", "Go fast"]);
    assert_eq!(
        parse_answer(&nested, "go", None),
        Ok(InteractionResponse::choice("c", 0))
    );
}

/// A text question takes the words as written: "1" is the text "1", and an
/// empty answer is an answer.
#[test]
fn a_text_question_takes_the_words_as_written() {
    let req = InteractionRequest::free_text("t", "Why?", "s", true);
    assert_eq!(
        parse_answer(&req, "1", None),
        Ok(InteractionResponse::text("t", "1"))
    );
    assert_eq!(
        parse_answer(&req, "", None),
        Ok(InteractionResponse::text("t", ""))
    );
    assert_eq!(
        parse_answer(&req, "x", Some("why")).unwrap_err(),
        "feedback goes with a denial; 't' is a text question"
    );
    assert!(answer_options(&InteractionRequest::edit_text("e", "Edit", "s", "doc")).is_empty());
}

/// `--choice` counts from 0 over the same list, for every kind with one.
#[test]
fn an_option_is_picked_by_its_zero_based_position() {
    assert_eq!(
        answer_at(&approval(), 1, None),
        Ok(InteractionResponse::approval(
            "a",
            true,
            ApprovalScope::Stage
        ))
    );
    assert_eq!(
        answer_at(&approval(), 3, Some("no")),
        Ok(InteractionResponse::deny_with_feedback("a", "no"))
    );
    assert_eq!(
        answer_at(&choice(&["a", "b"]), 1, None),
        Ok(InteractionResponse::choice("c", 1))
    );
    assert_eq!(
        answer_at(&choice(&["a", "b"]), 2, None).unwrap_err(),
        "'c' has no option 2: counting from 0, its options are 0-1"
    );
    assert_eq!(
        answer_at(
            &InteractionRequest::free_text("t", "Why?", "s", true),
            0,
            None
        )
        .unwrap_err(),
        "'t' is a text question: answer it with text"
    );
}

/// An API caller names an option by its word; a text question has none, so
/// the word is refused rather than taken as the text.
#[test]
fn an_option_word_answers_only_a_question_with_options() {
    assert_eq!(
        answer_with_option(&approval(), "allow-run", None),
        Ok(InteractionResponse::approval("a", true, ApprovalScope::Run))
    );
    assert_eq!(
        answer_with_option(
            &InteractionRequest::free_text("t", "Why?", "s", true),
            "allow",
            None
        )
        .unwrap_err(),
        "'t' is a text question: answer it with text"
    );
}

#[test]
fn every_kind_says_how_it_is_answered() {
    let text = InteractionRequest::free_text("t", "Why?", "s", true);
    assert_eq!(how_to_answer(&text), "lev respond t \"your answer\"");
    let edit = InteractionRequest::edit_text("e", "Edit", "s", "doc");
    assert_eq!(how_to_answer(&edit), "lev respond e \"your answer\"");
    let confirm = InteractionRequest::confirm("y", "Sure?", "s");
    assert_eq!(how_to_answer(&confirm), "lev respond y yes|no  (or 1-2)");
    assert_eq!(
        how_to_answer(&approval()),
        "lev respond a allow|allow-stage|allow-run|deny|deny-feedback  \
         (or 1-5; deny takes --feedback TEXT)"
    );
    assert_eq!(
        how_to_answer(&choice(&["Postgres", "SQLite"])),
        "lev respond c postgres|sqlite  (or the start of one, or 1-2)"
    );
}
