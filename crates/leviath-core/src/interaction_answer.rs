//! Answering a question with what it showed.
//!
//! Every surface lists a question's options the same way: numbered from 1, in
//! the request's own order, each under a word that answers it. A tool approval
//! answers to `allow`, `allow-stage`, `allow-run` and `deny`, a confirm to
//! `yes` and `no`, and a multiple choice to a word made from the option's own
//! label, so the word does not move when the options are reordered. The number
//! is only ever a convenience for a person reading the list; a caller that
//! keeps an answer should keep the word.
//!
//! [`answer_options`] is the list, [`parse_answer`] reads a typed answer
//! against it, and [`answer_at`] picks an option by its 0-based position.

use serde::Serialize;

use super::{
    ApprovalScope, DENY_WITH_FEEDBACK, InteractionKind, InteractionRequest, InteractionResponse,
};

/// What picking one option answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    /// Let the call go ahead, for this long.
    Approve(ApprovalScope),
    /// Refuse it, with or without words for the model.
    Deny,
    /// Refuse it and tell the model what to do instead: the words are the
    /// answer, so this one cannot be sent without them.
    DenyWithFeedback,
    /// The multiple-choice option at this index.
    Pick(usize),
}

/// One option a question offers, as every surface lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnswerOption {
    /// The word that answers with this option. Stable: an approval's and a
    /// confirm's words are fixed, and a choice's is made from its label, so
    /// reordering the options does not move it.
    pub id: String,
    /// The option as the question words it.
    pub label: String,
    /// Where it is listed, counting from 1. `lev respond` takes it in place of
    /// the word.
    pub number: usize,
    /// The whole `lev respond` command that answers with this option.
    pub answer: String,
    #[serde(skip)]
    effect: Effect,
}

/// The labels an approval offers when its request lists none.
const APPROVAL_LABELS: [&str; 4] = [
    "Allow once",
    "Allow for this stage",
    "Allow for this run",
    "Deny",
];

/// The word and the effect of one approval option, read off its label.
///
/// The labels are built in this crate, so they are the one place the meaning
/// lives; a label this does not know is a deny, because an answer nobody
/// recognised must never let a call through.
fn approval_option(label: &str) -> (String, Effect) {
    match label {
        "Allow once" => ("allow".to_string(), Effect::Approve(ApprovalScope::Once)),
        "Deny" => ("deny".to_string(), Effect::Deny),
        DENY_WITH_FEEDBACK => ("deny-feedback".to_string(), Effect::DenyWithFeedback),
        stage if stage.contains("for this stage") => (
            "allow-stage".to_string(),
            Effect::Approve(ApprovalScope::Stage),
        ),
        run if run.contains("for this run") => {
            ("allow-run".to_string(), Effect::Approve(ApprovalScope::Run))
        }
        other => (slug(other), Effect::Deny),
    }
}

/// A word made from a label: lower case, letters and digits, a dash for
/// anything between them. `option` when nothing in the label survives.
fn slug(label: &str) -> String {
    let mut word = String::new();
    for c in label.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            word.push(c);
        } else if !word.is_empty() && !word.ends_with('-') {
            word.push('-');
        }
    }
    let word = word.trim_end_matches('-');
    match word.is_empty() {
        true => "option".to_string(),
        false => word.to_string(),
    }
}

/// The words of a multiple choice's options, one per label. Two labels that
/// make the same word are told apart by a count on the later one.
fn choice_words(labels: &[String]) -> Vec<String> {
    let mut words: Vec<String> = Vec::with_capacity(labels.len());
    for label in labels {
        let base = slug(label);
        let mut word = base.clone();
        let mut count = 2;
        while words.contains(&word) {
            word = format!("{base}-{count}");
            count += 1;
        }
        words.push(word);
    }
    words
}

/// Every option `req` offers, in the order it lists them, numbered from 1.
/// Empty for a text question, which takes its answer as written.
pub fn answer_options(req: &InteractionRequest) -> Vec<AnswerOption> {
    let rows: Vec<(String, String, Effect)> = match req.kind {
        InteractionKind::FreeText | InteractionKind::EditText => Vec::new(),
        InteractionKind::Confirm => vec![
            (
                "yes".to_string(),
                req.options
                    .first()
                    .map_or("Yes", String::as_str)
                    .to_string(),
                Effect::Approve(ApprovalScope::Once),
            ),
            (
                "no".to_string(),
                req.options.get(1).map_or("No", String::as_str).to_string(),
                Effect::Deny,
            ),
        ],
        InteractionKind::ToolApproval => {
            let labels: Vec<String> = match req.options.is_empty() {
                true => APPROVAL_LABELS.iter().map(|l| l.to_string()).collect(),
                false => req.options.clone(),
            };
            labels
                .into_iter()
                .map(|label| {
                    let (id, effect) = approval_option(&label);
                    (id, label, effect)
                })
                .collect()
        }
        InteractionKind::MultipleChoice => choice_words(&req.options)
            .into_iter()
            .zip(req.options.iter().cloned())
            .enumerate()
            .map(|(index, (id, label))| (id, label, Effect::Pick(index)))
            .collect(),
    };
    rows.into_iter()
        .enumerate()
        .map(|(index, (id, label, effect))| {
            let answer = match effect {
                Effect::DenyWithFeedback => {
                    format!("lev respond {} deny --feedback \"TEXT\"", req.id)
                }
                _ => format!("lev respond {} {id}", req.id),
            };
            AnswerOption {
                id,
                label,
                number: index + 1,
                answer,
                effect,
            }
        })
        .collect()
}

/// `a, b or c`.
fn or_list(words: &[&str]) -> String {
    match words.split_last() {
        Some((last, init)) if !init.is_empty() => format!("{} or {last}", init.join(", ")),
        _ => words.concat(),
    }
}

/// What a question is called in a refusal.
fn what(kind: &InteractionKind) -> &'static str {
    match kind {
        InteractionKind::ToolApproval => "approval",
        InteractionKind::Confirm => "confirmation",
        _ => "choice",
    }
}

/// The refusal for an answer `options` has no match for: every word that
/// would have answered, ready to copy, and the numbers they are listed under.
fn not_an_answer(req: &InteractionRequest, typed: &str, options: &[AnswerOption]) -> String {
    let words: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
    let start = match req.kind {
        InteractionKind::MultipleChoice => "answer with an option or the start of one: ",
        _ => "answer ",
    };
    format!(
        "\"{typed}\" is not an answer to this {}; {start}{} (or 1-{})",
        what(&req.kind),
        or_list(&words),
        options.len()
    )
}

/// The refusal for an answer that names more than one option.
fn ambiguous(typed: &str, several: &[&AnswerOption]) -> String {
    let words: Vec<&str> = several.iter().map(|o| o.id.as_str()).collect();
    format!(
        "\"{typed}\" could be any of {}; give more of it, or its number",
        or_list(&words)
    )
}

/// The one option `typed` names among `options`.
///
/// A number names the option listed under it and a word names the option it
/// is, either way regardless of case. A multiple choice also takes the start
/// of a label or a word, as long as only one option starts that way; an
/// approval and a confirm take only the whole word, since letting a call
/// through on a guess is the one mistake that cannot be undone. An answer that
/// could mean two options is refused naming both, never settled by picking
/// one.
fn find<'a>(
    req: &InteractionRequest,
    options: &'a [AnswerOption],
    typed: &str,
) -> Result<&'a AnswerOption, String> {
    let want = typed.trim().to_lowercase();
    let by_number = want
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|index| options.get(index));
    let by_name = options
        .iter()
        .find(|o| o.id == want || o.label.to_lowercase() == want);
    match (by_number, by_name) {
        (Some(number), Some(name)) if number.number != name.number => {
            Err(ambiguous(typed, &[number, name]))
        }
        (Some(found), _) | (None, Some(found)) => Ok(found),
        (None, None) if req.kind == InteractionKind::MultipleChoice && !want.is_empty() => {
            let starts: Vec<&AnswerOption> = options
                .iter()
                .filter(|o| o.id.starts_with(&want) || o.label.to_lowercase().starts_with(&want))
                .collect();
            match starts.as_slice() {
                [one] => Ok(one),
                [] => Err(not_an_answer(req, typed, options)),
                several => Err(ambiguous(typed, several)),
            }
        }
        (None, None) => Err(not_an_answer(req, typed, options)),
    }
}

/// The response that answers `req` with `option`, and `feedback` for a deny.
fn respond_with(
    req: &InteractionRequest,
    option: &AnswerOption,
    feedback: Option<&str>,
) -> Result<InteractionResponse, String> {
    let id = req.id.as_str();
    match (option.effect, feedback) {
        (Effect::Approve(scope), None) => Ok(InteractionResponse::approval(id, true, scope)),
        (Effect::Deny, None) => Ok(InteractionResponse::approval(
            id,
            false,
            ApprovalScope::Once,
        )),
        (Effect::Deny | Effect::DenyWithFeedback, Some(text)) => {
            Ok(InteractionResponse::deny_with_feedback(id, text))
        }
        (Effect::DenyWithFeedback, None) => Err(format!(
            "{} needs the words for the model: answer deny with feedback, as in {}",
            option.id, option.answer
        )),
        (Effect::Pick(index), None) => Ok(InteractionResponse::choice(id, index)),
        (Effect::Approve(_) | Effect::Pick(_), Some(_)) => Err(format!(
            "feedback goes with a denial, and {} is not one",
            option.id
        )),
    }
}

/// The refusal for a text question asked to take an option.
fn text_question(req: &InteractionRequest) -> String {
    format!("'{}' is a text question: answer it with text", req.id)
}

/// Read `typed` as the answer to `req`.
///
/// The question's kind decides how: a text question takes the words exactly as
/// written, so "1" answers it with the text "1", and every other kind reads
/// them as one of its [`answer_options`]. `feedback` is the words a deny
/// carries for the model, and nothing else takes it.
pub fn parse_answer(
    req: &InteractionRequest,
    typed: &str,
    feedback: Option<&str>,
) -> Result<InteractionResponse, String> {
    let options = answer_options(req);
    match (options.is_empty(), feedback) {
        (true, None) => Ok(InteractionResponse::text(&req.id, typed)),
        (true, Some(_)) => Err(format!(
            "feedback goes with a denial; '{}' is a text question",
            req.id
        )),
        (false, _) => respond_with(req, find(req, &options, typed)?, feedback),
    }
}

/// Answer `req` with the option `word` names: its word or its number, never
/// text. What an API caller sends after reading the request's options, so a
/// text question, which has none, is refused rather than handed the word as
/// its answer.
pub fn answer_with_option(
    req: &InteractionRequest,
    word: &str,
    feedback: Option<&str>,
) -> Result<InteractionResponse, String> {
    match answer_options(req).is_empty() {
        true => Err(text_question(req)),
        false => parse_answer(req, word, feedback),
    }
}

/// Answer `req` with the option at 0-based `index`, which is how a script that
/// counts from 0 names one.
pub fn answer_at(
    req: &InteractionRequest,
    index: usize,
    feedback: Option<&str>,
) -> Result<InteractionResponse, String> {
    let options = answer_options(req);
    match options.get(index) {
        Some(option) => respond_with(req, option, feedback),
        None if options.is_empty() => Err(text_question(req)),
        None => Err(format!(
            "'{}' has no option {index}: counting from 0, its options are 0-{}",
            req.id,
            options.len() - 1
        )),
    }
}

/// How to answer `req` with `lev respond`, in one line.
pub fn how_to_answer(req: &InteractionRequest) -> String {
    let id = &req.id;
    let options = answer_options(req);
    let words: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
    let words = words.join("|");
    let count = options.len();
    match req.kind {
        InteractionKind::FreeText | InteractionKind::EditText => {
            format!("lev respond {id} \"your answer\"")
        }
        InteractionKind::MultipleChoice => {
            format!("lev respond {id} {words}  (or the start of one, or 1-{count})")
        }
        InteractionKind::Confirm => format!("lev respond {id} {words}  (or 1-{count})"),
        InteractionKind::ToolApproval => {
            format!("lev respond {id} {words}  (or 1-{count}; deny takes --feedback TEXT)")
        }
    }
}

#[cfg(test)]
#[path = "interaction_answer_tests.rs"]
mod tests;
