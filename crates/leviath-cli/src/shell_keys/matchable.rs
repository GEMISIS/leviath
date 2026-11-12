//! A line as a yolo profile's shell rules match it: the commands in order,
//! each with the words a rule would be held against.

use super::{Stripped, WriteTarget, classify_write, strip_to_program, tokenize_for};

/// One command of a line as a yolo profile's shell rules see it: the words
/// that would be matched, and whether they can be trusted to say what runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatchableSegment {
    /// The program and its arguments, quotes removed, in order. Empty when the
    /// segment runs nothing a rule could name.
    pub words: Vec<String>,
    /// The words do not fully decide what runs: the segment binds a variable
    /// in front of its program (`PATH=x cargo`), one of its words is an
    /// expansion (`rm -r $DIR`), or its program is one no key can name. An
    /// opaque segment can still be refused; it can never be matched by a rule
    /// that would let it through.
    pub opaque: bool,
    /// Whether it redirects a write somewhere that keeps it.
    pub writes: bool,
}

/// The commands of a line, in order, for rule matching. `None` when the line
/// cannot be read at all, the same cases [`super::command_keys`] treats as
/// ungrantable. Segments that run nothing and bind nothing (`for i in ...`,
/// `fi`) are left out; a binding on its own (`export FOO=1`) stays, as an
/// opaque segment, because it changes what a later one resolves to.
///
/// The same tokenizer and the same reading of what a segment runs as the grant
/// keys use; only the shape differs, because a rule wants the words and a key
/// wants a name.
pub(crate) fn matchable_segments(
    command: &str,
    backslash_escapes: bool,
) -> Option<Vec<MatchableSegment>> {
    let segments = tokenize_for(command, backslash_escapes)?;
    let mut out = Vec::new();
    for segment in &segments {
        let writes = segment
            .writes
            .iter()
            .any(|t| classify_write(t) != WriteTarget::Discarded);
        let (words, opaque) = match strip_to_program(&segment.words) {
            Stripped::NothingRuns(keys) if keys.is_empty() && !writes => continue,
            Stripped::NothingRuns(_) | Stripped::Unreadable(_) => (Vec::new(), true),
            Stripped::Program { env_keys, words } => (
                words.iter().map(|w| w.text.clone()).collect(),
                !env_keys.is_empty() || words.iter().any(|w| !w.literal),
            ),
        };
        out.push(MatchableSegment {
            words,
            opaque,
            writes,
        });
    }
    Some(out)
}
