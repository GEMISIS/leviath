//! Where a line's redirects land, as paths, for a fence that has to say
//! where every write goes.
//!
//! [`write_targets_for`](super::write_targets_for) names the targets as the
//! line spells them. A fence needs more: a relative target lands under
//! whatever directory the line has moved to by then, so `cd src && echo x >
//! docs/a` writes `src/docs/a`.
//!
//! Only the plainest move is followed: a segment that is exactly `cd <dir>`
//! or `pushd <dir>` with a literal directory. Such a move may fail, so the
//! directory before it stays possible too, and a target is resolved against
//! every directory the line may be in. Anything else on the line that can
//! change directory (a `cd` anywhere but first, behind `{`, `!`, `then`,
//! `command` or an assignment, `popd`, `chdir`, `CDPATH`, `.`, `source`,
//! `eval`, or a program that is itself an expansion) leaves every relative
//! target unknown. An absolute target lands where it says whatever the line
//! did first.

use std::path::{Path, PathBuf};

use super::{Segment, WriteTarget, classify_write, tokenize_for};

/// The most directories a line is followed into before it counts as lost.
const MAX_CWDS: usize = 64;

/// Words that change the shell's directory, or run text that may.
const MOVERS: &[&str] = &["cd", "pushd", "popd", "chdir", ".", "source", "eval"];

/// One redirect target: as the line spells it, and every path it may land on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The target as written.
    pub(crate) written: String,
    /// Every path it may land on, from each directory the line may be in.
    pub(crate) lands: Vec<PathBuf>,
}

/// Where a line's redirects may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Landing {
    /// Every target, with where it may land.
    Known(Vec<Target>),
    /// The line does not parse, and has a `>` in it.
    Unparsed,
    /// A target is not literal: `> $OUT`, `>& $fd`, `/dev/tcp/...`.
    Unnamed,
    /// A relative target on a line that may change directory in a way this
    /// does not follow.
    Lost,
}

/// Where `command`'s redirects may write, run from `workdir`. With
/// `backslash_escapes` (a POSIX shell), `> NUL` is a file named `NUL` like
/// any other; only `cmd.exe` discards it.
pub(crate) fn landing_for(
    command: &str,
    backslash_escapes: bool,
    workdir: &Path,
    home: Option<&Path>,
) -> Landing {
    let Some(segments) = tokenize_for(command, backslash_escapes) else {
        return match command.contains('>') {
            true => Landing::Unparsed,
            false => Landing::Known(Vec::new()),
        };
    };
    let mut followed = segments
        .iter()
        .all(|s| !moves(s) || plain_move(s, home).is_some());
    let mut cwds = vec![workdir.to_path_buf()];
    let mut out = Vec::new();
    for segment in &segments {
        for target in &segment.writes {
            let written = match classify_write(target) {
                WriteTarget::Path(text) => text,
                WriteTarget::Discarded
                    if backslash_escapes && target.text.eq_ignore_ascii_case("NUL") =>
                {
                    target.text.clone()
                }
                WriteTarget::Discarded => continue,
                WriteTarget::Unreadable => return Landing::Unnamed,
            };
            let lands = match Path::new(&written).is_absolute() {
                true => vec![PathBuf::from(&written)],
                false if followed => cwds.iter().map(|cwd| cwd.join(&written)).collect(),
                false => return Landing::Lost,
            };
            out.push(Target { written, lands });
        }
        match plain_move(segment, home) {
            // Each move doubles the directories the line may be in; past a
            // handful of them the line is no longer followed.
            Some(next) if cwds.len() < MAX_CWDS => {
                let more: Vec<PathBuf> = cwds.iter().map(|cwd| cwd.join(&next)).collect();
                cwds.extend(more);
            }
            Some(_) => followed = false,
            None => {}
        }
    }
    Landing::Known(out)
}

/// Whether `segment` may change the shell's directory, or run something that
/// might: a mover anywhere in it, `CDPATH` anywhere in it, or a program that
/// is itself an expansion.
fn moves(segment: &Segment) -> bool {
    let program_unknown = segment.words.first().is_some_and(|w| !w.literal);
    program_unknown
        || segment
            .words
            .iter()
            .any(|w| MOVERS.contains(&w.text.as_str()) || w.text.contains("CDPATH"))
}

/// The directory `segment` moves to when it is exactly `cd <dir>` or
/// `pushd <dir>`, `<dir>` literal and not an option or `-`.
fn plain_move(segment: &Segment, home: Option<&Path>) -> Option<PathBuf> {
    let [program, target] = segment.words.as_slice() else {
        return None;
    };
    let plain = matches!(program.text.as_str(), "cd" | "pushd")
        && program.literal
        && target.literal
        && !target.text.starts_with('-');
    plain
        .then(|| leviath_core::paths::expand_home(&target.text, home))
        .flatten()
}

#[cfg(test)]
#[path = "targets_tests.rs"]
mod tests;
