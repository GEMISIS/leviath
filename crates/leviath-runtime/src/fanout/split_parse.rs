//! Parsing a fan-out split's answer into work items.
//!
//! The split is the one structured answer the framework asks a model for that
//! it may also give in prose. A stage advertises `submit_work_items` and a model
//! that calls it lands in [`work_items_from_value`] with arguments the provider
//! already validated; everything else in here exists for the models that answer
//! in text anyway, which is a real case because a blueprint picks its own model
//! per stage.
//!
//! The text path is deliberately generous. Each shape it refuses costs a
//! correction round, and the `deep-researcher` run this was hardened for had one
//! correction to spend.

use bevy_ecs::prelude::*;

/// One unit of work produced by a fan-out split.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct WorkItem {
    /// Stable id (used to label the worker in the consolidated report).
    #[serde(default)]
    pub id: String,
    /// Free-form context handed to the worker (seeded into its pinned context).
    #[serde(default)]
    pub context: serde_json::Value,
}

/// Envelope keys a model wraps the array in often enough to be worth accepting.
///
/// Asking for a bare array and being handed `{"items": [...]}` is the single
/// most common near miss, and refusing it costs a correction round to fix
/// something that is not actually ambiguous.
const WORK_ITEM_ENVELOPES: [&str; 4] = ["items", "work_items", "sub_questions", "tasks"];

/// The sentinel character in the text-protocol tool-call markers some models
/// emit as prose.
///
/// A fan-out stage advertises `submit_work_items` and nothing else, but a model
/// trained to write its calls out as text will do that regardless of what the
/// API offered. The run this parser was hardened for emitted literal
/// `</｜DSML｜tool_calls>` markers in the middle of a report, and the brackets
/// inside such a block are what a first-`[`-to-last-`]` scan latches onto.
///
/// Matched on the sentinel character rather than a list of protocol names: the
/// names differ per model family, `｜` (U+FF5C, fullwidth vertical line) is what
/// they have in common, and it does not otherwise appear in JSON.
const TOOL_PROTOCOL_SENTINEL: char = '｜';

/// How many words a generated work-item id keeps.
const SLUG_WORDS: usize = 6;

/// Parse a split response into work items.
///
/// Layered on purpose, most exact first: a fenced code block, then the whole
/// response as JSON, then the outermost `[ … ]`. This is the fallback path - a
/// model that calls `submit_work_items` never reaches here - so it is written to
/// accept the near misses rather than to re-enforce a shape the tool already
/// enforces.
///
/// Beyond a bare array it takes an `{"items": [...]}`-style envelope, a single
/// bare object (a fan-out of one), and an array of plain strings (each becomes a
/// one-question work item). Every one of those used to cost a correction round,
/// and the run that motivated this had exactly one left to spend.
pub fn parse_work_items(content: &str) -> Result<Vec<WorkItem>, String> {
    let cleaned = strip_tool_protocol_markup(content);
    let trimmed = cleaned.trim();
    // Every rejection folds into one error: this parses model output, so
    // "malformed input yields `Err`" has to hold for every shape of malformed.
    let candidates = [
        fenced_block(trimmed),
        Some(trimmed),
        outermost_array(trimmed),
    ];
    // The whole response is always one of the candidates, so a failing parse
    // always leaves a real message behind and there is no "nothing was tried"
    // case to invent an error for.
    let mut last_error = String::new();
    for candidate in candidates.into_iter().flatten() {
        match serde_json::from_str::<serde_json::Value>(candidate) {
            Ok(value) => match work_items_from_value(value) {
                Ok(items) => return Ok(items),
                Err(e) => last_error = e,
            },
            Err(e) => last_error = format!("split output is not valid JSON: {e}"),
        }
    }
    Err(last_error)
}

/// Read work items out of a parsed JSON value, accepting the shapes a model
/// reaches for when it is trying to answer the question that was asked.
pub(super) fn work_items_from_value(value: serde_json::Value) -> Result<Vec<WorkItem>, String> {
    match value {
        serde_json::Value::Array(items) => items.into_iter().map(work_item_from_value).collect(),
        serde_json::Value::Object(map) => {
            // An envelope first: `{"items": [...]}` is a wrapper, not a work
            // item, and reading it as one would spawn a single worker carrying
            // the whole list as its context.
            match WORK_ITEM_ENVELOPES
                .iter()
                .find_map(|key| map.get(*key).filter(|v| v.is_array()).cloned())
            {
                Some(inner) => work_items_from_value(inner),
                // A fan-out of one, which the split prompt explicitly allows for
                // a narrow topic - but only when the object actually looks like
                // a work item. Both of `WorkItem`'s fields default and unknown
                // ones are tolerated, so without this guard *every* JSON object
                // parses, and `{"items": "all of them"}` - a botched envelope -
                // would become one nameless worker instead of a correction.
                None if map.contains_key("id") || map.contains_key("context") => {
                    Ok(vec![work_item_from_value(serde_json::Value::Object(map))?])
                }
                None => Err(
                    "split output is a JSON object that is neither a work item (no `id` or \
                     `context`) nor a list of them (no `items` array)"
                        .to_string(),
                ),
            }
        }
        // Rendered through `Display`, which for a `Value` is its JSON, rather
        // than through a hand-written name per variant: the variants that could
        // reach here are only the scalars, so naming all six would leave two
        // arms that no input can take.
        other => Err(format!("split output is not a JSON array (found {other})")),
    }
}

/// One element of the array, as a work item.
///
/// A plain string is taken as the question itself: a model that answers
/// `["how does X work", "what happens after Y"]` has done the actual thinking
/// and only skipped the wrapper. The id it is given labels the worker in the
/// consolidated report; it is not something the worker reads.
fn work_item_from_value(value: serde_json::Value) -> Result<WorkItem, String> {
    match value {
        serde_json::Value::String(question) => Ok(WorkItem {
            id: slug(&question),
            context: serde_json::json!({ "question": question }),
        }),
        other => serde_json::from_value(other)
            .map_err(|e| format!("split output is not a valid JSON array of work items: {e}")),
    }
}

/// A short, stable id for a work item that arrived without one.
///
/// Lowercase ASCII words joined by dashes and bounded, so a whole question does
/// not become a section heading in the consolidated report.
fn slug(text: &str) -> String {
    text.chars()
        .map(|c| match c.is_ascii_alphanumeric() {
            true => c.to_ascii_lowercase(),
            false => '-',
        })
        .collect::<String>()
        .split('-')
        .filter(|word| !word.is_empty())
        .take(SLUG_WORDS)
        .collect::<Vec<_>>()
        .join("-")
}

/// The contents of the first fenced code block, if the response has one.
///
/// Preferred over the bracket scan because a response that fences its answer has
/// said exactly where the answer is, and the prose around it may carry brackets
/// of its own - one bibliography citation like `[6]` is enough to make the scan
/// slice from the wrong place.
fn fenced_block(text: &str) -> Option<&str> {
    let after_open = text.split_once("```")?.1;
    // The opening fence may carry a language tag on the rest of its line.
    let body = match after_open.split_once('\n') {
        Some((tag, rest)) if !tag.contains("```") => rest,
        _ => after_open,
    };
    Some(body.split_once("```")?.0.trim())
}

/// The outermost `[ … ]` in the text: the last-resort extraction.
fn outermost_array(text: &str) -> Option<&str> {
    match (text.find('['), text.rfind(']')) {
        (Some(start), Some(end)) if end > start => text.get(start..=end),
        _ => None,
    }
}

/// Drop text-protocol tool-call markup, keeping everything around it.
///
/// Only pays for itself when a marker is actually present, which is rare, so the
/// common path is one `contains` and a borrow.
fn strip_tool_protocol_markup(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(TOOL_PROTOCOL_SENTINEL) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // Split rather than index: the offsets a `find` returns are on character
    // boundaries, but model output is arbitrary UTF-8 and the workspace lint
    // does not distinguish a safe slice from an unsafe one. `split_once` cannot
    // land inside a character at all.
    while let Some((before, after_open)) = rest.split_once('<') {
        let Some((tag, after_close)) = after_open.split_once('>') else {
            // An unclosed `<` is ordinary prose ("3 < 4"), so the remainder -
            // which `rest` still holds whole - is copied as it stands.
            break;
        };
        out.push_str(before);
        if !tag.contains(TOOL_PROTOCOL_SENTINEL) {
            out.push('<');
            out.push_str(tag);
            out.push('>');
        }
        rest = after_close;
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The envelope near miss. Asking for a bare array and being handed
    /// `{"items": [...]}` is the shape a model reaches for most often, and
    /// reading the wrapper as a work item would spawn one worker carrying the
    /// whole list.
    #[test]
    fn parse_work_items_unwraps_an_envelope() {
        for key in ["items", "work_items", "sub_questions", "tasks"] {
            let json = format!(r#"{{"{key}": [{{"id":"a"}},{{"id":"b"}}]}}"#);
            let items = parse_work_items(&json).expect("the envelope is unwrapped");
            assert_eq!(items.len(), 2, "{key}");
            assert_eq!(items[0].id, "a", "{key}");
        }
    }

    /// A single bare object is a fan-out of one, which the split prompt
    /// explicitly allows for a narrow topic.
    #[test]
    fn parse_work_items_takes_a_single_bare_object() {
        let items = parse_work_items(r#"{"id":"only","context":{"question":"q"}}"#).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "only");
        assert_eq!(items[0].context["question"], "q");
    }

    /// The bare-object guard takes either field. An item with only a `context`
    /// is still a work item; the id is what labels it in the report, and a
    /// missing one is a cosmetic loss, not a malformation.
    #[test]
    fn parse_work_items_takes_a_bare_object_with_only_a_context() {
        let items = parse_work_items(r#"{"context":{"question":"q"}}"#).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "");
        assert_eq!(items[0].context["question"], "q");
    }

    /// A bare object shaped like a work item but with the wrong types is a real
    /// malformation, and the message says so rather than inventing an item.
    #[test]
    fn parse_work_items_rejects_a_bare_object_with_the_wrong_types() {
        let err = parse_work_items(r#"{"id": 42, "context": {}}"#).unwrap_err();
        assert!(err.contains("work items"), "{err}");
    }

    /// An array of plain strings: the model did the thinking and skipped the
    /// wrapper. Each string becomes its own question, with a generated id.
    #[test]
    fn parse_work_items_takes_an_array_of_questions() {
        let items =
            parse_work_items(r#"["How does semaglutide work?", "What happens after"]"#).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "how-does-semaglutide-work");
        assert_eq!(items[0].context["question"], "How does semaglutide work?");
        assert_eq!(items[1].id, "what-happens-after");
    }

    /// A generated id keeps a bounded number of words, so a long question does
    /// not become a section heading in the consolidated report.
    #[test]
    fn a_generated_id_is_bounded() {
        let long = "one two three four five six seven eight nine";
        let items = parse_work_items(&format!(r#"["{long}"]"#)).unwrap();
        assert_eq!(items[0].id, "one-two-three-four-five-six");
    }

    /// The bibliography case. A report with `[6]` in its citations used to make
    /// the bracket scan slice from the wrong place; a fenced block says exactly
    /// where the answer is, so it wins.
    #[test]
    fn parse_work_items_prefers_a_fenced_block_over_stray_brackets() {
        let response = "See [6] and [14] above.\n```json\n[{\"id\":\"real\"}]\n```\nThanks!";
        let items = parse_work_items(response).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "real");
    }

    /// A fence with no newline after it (so no language tag to strip) still
    /// yields its body.
    #[test]
    fn parse_work_items_reads_a_fence_with_no_language_tag() {
        assert_eq!(
            parse_work_items("x ```[{\"id\":\"a\"}]``` y").unwrap()[0].id,
            "a"
        );
    }

    /// An unterminated fence is not a fenced block, so the bracket scan is what
    /// answers.
    #[test]
    fn parse_work_items_falls_through_an_unterminated_fence() {
        assert_eq!(
            parse_work_items("```json\n[{\"id\":\"a\"}]").unwrap()[0].id,
            "a"
        );
    }

    /// Text-protocol tool-call markup emitted as prose. The run this parser was
    /// hardened for wrote a whole report with these markers in it, and the
    /// brackets inside them are what the bracket scan latched onto.
    #[test]
    fn parse_work_items_strips_text_protocol_tool_call_markup() {
        let response = concat!(
            "Here is the plan.\n",
            "</\u{ff5c}DSML\u{ff5c}parameter>\n",
            "</\u{ff5c}DSML\u{ff5c}tool_calls>\n",
            "[{\"id\":\"kept\"}]"
        );
        let items = parse_work_items(response).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "kept");
    }

    /// Markup stripping leaves ordinary tags alone, and survives a dangling `<`
    /// with nothing closing it.
    #[test]
    fn stripping_markup_keeps_real_tags_and_survives_a_dangling_bracket() {
        let response = concat!(
            "</\u{ff5c}X\u{ff5c}call> <b>bold</b> 3 < 4\n",
            "[{\"id\":\"a\"}]"
        );
        let items = parse_work_items(response).unwrap();
        assert_eq!(items[0].id, "a");
    }

    /// A JSON scalar is not a work-item list, and the message says what came
    /// back rather than only naming the rule.
    #[test]
    fn parse_work_items_rejects_a_scalar_and_says_what_it_got() {
        let err = parse_work_items("42").unwrap_err();
        assert!(err.contains("not a JSON array"), "{err}");
        assert!(err.contains("42"), "{err}");
    }

    /// An array whose elements are neither objects nor strings is a real
    /// malformation, not a near miss.
    #[test]
    fn parse_work_items_rejects_an_array_of_the_wrong_thing() {
        let err = parse_work_items("[1, 2, 3]").unwrap_err();
        assert!(err.contains("work items"), "{err}");
    }

    /// Nothing array-shaped anywhere: the last-resort scan finds no brackets and
    /// the error names that.
    #[test]
    fn parse_work_items_rejects_prose_with_no_json_at_all() {
        let err = parse_work_items("I have completed the research.").unwrap_err();
        assert!(!err.is_empty(), "{err}");
    }

    #[test]
    fn parse_work_items_handles_array_prose_and_errors() {
        let ok = parse_work_items(r#"[{"id":"a"},{"id":"b","context":{"k":1}}]"#).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].id, "a");
        assert_eq!(ok[1].context["k"], 1);
        // Missing fields default.
        assert_eq!(parse_work_items("[{}]").unwrap()[0].id, "");
        // Prose around the array is tolerated.
        assert_eq!(
            parse_work_items("Here you go:\n```json\n[{\"id\":\"x\"}]\n```")
                .unwrap()
                .len(),
            1
        );
        // No brackets at all.
        assert!(parse_work_items("no array here").is_err());
        // Closing before opening (e <= s).
        assert!(parse_work_items("]nope[").is_err());
        // Brackets but not valid JSON.
        assert!(parse_work_items("[not json]").is_err());
    }
}
