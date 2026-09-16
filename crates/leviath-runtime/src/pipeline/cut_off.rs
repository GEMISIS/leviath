//! What a stage does with a reply the output cap cut off.
//!
//! A cut-off text reply is sent back with a nudge, and a cut-off tool call is
//! refused with advice on splitting it. Both draw on one budget of cut-offs in
//! a row. Kept together so the counting, the words the model reads and the
//! stage error that ends it cannot drift apart.

/// How many cut-off replies in a row a stage sends back before it stops. The
/// first retry goes out with the cap raised to the model's maximum, so a
/// second cut-off means the reply does not fit the model at all and the
/// model is asked for it in pieces; a third means the model is not listening.
/// On the next one the stage stops paying: a cut-off text reply is accepted
/// as the answer, and a cut-off tool call, which has nothing to accept, ends
/// the stage with an error. Text replies and tool calls draw on the same
/// count, and any reply that was not cut off resets it.
pub(crate) const MAX_CUT_OFF_NUDGES: usize = 3;

/// Why a stage ended on cut-off tool calls: the stage error the run's status,
/// its stage log and any `error` edge's recovery stage all read.
pub(crate) fn cut_off_stage_error(cut_offs: usize, tools: &[&str]) -> String {
    format!(
        "{cut_offs} replies in a row were cut off by the output limit in the middle of a \
         tool call ({}), with the limit already raised to the model's maximum. The call \
         is too large for one reply and has to be split into smaller calls",
        tools.join(", ")
    )
}

/// The `[System]` line sent back with a cut-off reply.
///
/// It names the cause and the two ways out, because the reply that got cut
/// off was almost always a single oversized write, and a model told only "you
/// have not written the file yet" sends the same write again.
pub(crate) fn cut_off_nudge(cut_off_at: usize) -> String {
    format!(
        "Your previous reply was cut off by the output limit after {cut_off_at} output tokens, \
         so it was not used. Do not send it again as it was. Either make it shorter, or split \
         the work into smaller pieces: for a file, write the first part, then add each further \
         part with a separate call. The output limit has been raised to the model's maximum \
         for your next reply."
    )
}

/// The refusal for a tool call whose arguments were not JSON.
///
/// Names the size and the tail of what arrived, because that is what tells
/// the model (and a person reading the log) that the call was cut off rather
/// than mistyped: an argument string that ends mid-word at a round number of
/// tokens is the output cap, every time.
///
/// What follows depends on `in_a_row`, the stage's count of cut-off replies
/// in a row including this one (see `StageProgress::cut_off_nudges`). The
/// first is often the stage's own cap, which the next request lifts to the
/// model's maximum, so sending the call again can work. From the second on
/// the call does not fit the model at all: the refusal says so, says how to
/// split this tool's call, and says how many more cut-offs end the stage with
/// an error, because a model that knows the stakes stops resending.
pub(crate) fn cut_off_arguments_refusal(name: &str, raw: &str, in_a_row: usize) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(40)..].iter().collect();
    let how = match leviath_tools::canonical_tool_name(name) {
        "write_file" => {
            "write the first part with write_file, then add each later part with \
             write_file and \"append\": true"
        }
        "edit_file" => "change a smaller piece of text in each edit_file call",
        _ => "send less in each call and spread the work over several calls",
    };
    let next = if in_a_row < 2 {
        format!(
            "Your next reply may use up to the model's maximum output. Send the call \
             again, or if it is large, split it: {how}."
        )
    } else {
        let left = (MAX_CUT_OFF_NUDGES + 1).saturating_sub(in_a_row);
        let ends = match left {
            0 | 1 => "if your next reply is cut off too".to_string(),
            n => format!("if your next {n} replies are cut off too"),
        };
        format!(
            "That is {in_a_row} replies in a row, so this call does not fit in one reply \
             even at the model's maximum. Do not send it again as it was. Split it: {how}. \
             The stage ends with an error {ends}."
        )
    };
    format!(
        "[error] '{name}' was not run: the reply was cut off by the output limit partway \
         through its arguments ({} characters arrived, ending `{tail}`). {next}",
        chars.len()
    )
}
