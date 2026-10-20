//! Where a line's redirects land, as paths, for a fence that has to say
//! where every write goes.
//!
//! [`write_targets_for`](super::write_targets_for) names the targets as the
//! line spells them. A fence needs more: a relative target lands under
//! whatever directory the line has moved to by then, so `cd src && echo x >
//! docs/a` writes `src/docs/a`. A `cd` may also fail and leave the line where
//! it was, so every directory the line may be in is kept, and a target is
//! resolved against each of them. A fence passes a target only when it passes
//! from every one.

use std::path::{Path, PathBuf};

use super::{WriteTarget, classify_write, tokenize_for};

/// The most directories a line is followed into before it counts as lost.
const MAX_CWDS: usize = 64;

/// Where a line's redirects may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Landing {
    /// Each target as written, with every path it may land on.
    Known(Vec<(String, PathBuf)>),
    /// The line does not parse, and has a `>` in it.
    Unparsed,
    /// A target is not literal: `> $OUT`, `>& $fd`, `/dev/tcp/...`.
    Unnamed,
    /// The line writes after moving to a directory that is not literal.
    Lost,
}

/// Where `command`'s redirects may write, run from `workdir`.
///
/// `cd`, `pushd` and `popd` move the directory later targets resolve
/// against. A move to a literal directory adds it to the directories the line
/// may be in (the move may fail, so the old one stays); any other move (`cd`
/// with no argument, `cd -`, `popd`, a target with an expansion) leaves the
/// directory unknown, and a write after it is [`Landing::Lost`]. With
/// `backslash_escapes` (a POSIX shell), `> NUL` is a file named `NUL` like any
/// other; only `cmd.exe` discards it.
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
    let mut cwds = vec![workdir.to_path_buf()];
    let mut lost = false;
    let mut out = Vec::new();
    for segment in &segments {
        for target in &segment.writes {
            let text = match classify_write(target) {
                WriteTarget::Path(text) => text,
                WriteTarget::Discarded
                    if backslash_escapes && target.text.eq_ignore_ascii_case("NUL") =>
                {
                    target.text.clone()
                }
                WriteTarget::Discarded => continue,
                WriteTarget::Unreadable => return Landing::Unnamed,
            };
            if lost {
                return Landing::Lost;
            }
            for cwd in &cwds {
                out.push((text.clone(), cwd.join(&text)));
            }
        }
        let [program, rest @ ..] = segment.words.as_slice() else {
            continue;
        };
        if !matches!(program.text.as_str(), "cd" | "pushd" | "popd") {
            continue;
        }
        let moved_to = match (program.text.as_str(), rest) {
            ("popd", _) => None,
            (_, [target, ..]) if target.literal && target.text != "-" => {
                leviath_core::paths::expand_home(&target.text, home)
            }
            _ => None,
        };
        match moved_to {
            // Each move doubles the directories the line may be in; past a
            // handful of them the line is treated as lost rather than walked.
            Some(next) if cwds.len() < MAX_CWDS => {
                let more: Vec<PathBuf> = cwds.iter().map(|cwd| cwd.join(&next)).collect();
                cwds.extend(more);
            }
            _ => lost = true,
        }
    }
    Landing::Known(out)
}

#[cfg(test)]
#[path = "targets_tests.rs"]
mod tests;
