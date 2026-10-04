//! What upgrading a home did, said once to the person who is waiting.
//!
//! The first daemon start after an upgrade from an earlier release backs up
//! the home, upgrades its installed blueprints and converts its old runs
//! (see [`crate::blueprint_upgrade`] and [`crate::daemon::convert_old`]).
//! Each step reports its progress on the daemon's
//! [`StartupBoard`](leviath_runtime::control_socket::StartupBoard) while it
//! runs, and adds what it did to an [`Upgrade`]. At the end the upgrade is
//! summed up in a few lines: how many blueprints were upgraded and runs
//! converted, where the backup is, and a warning for every key the upgrade
//! dropped. They go to the daemon's log and are kept in the backup for the
//! first command a person runs to show once
//! ([`Backup::announce_later`](crate::home_backup::Backup::announce_later)).

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::home_backup::Backup;

/// What one upgrade of a home did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Upgrade {
    /// Blueprints migrated to `agent.toml` or replaced by the bundled one.
    pub(crate) blueprints: usize,
    /// Blueprints left as they were, because they could not be upgraded.
    pub(crate) blueprints_failed: usize,
    /// The directory of each blueprint left as it was.
    pub(crate) left: Vec<String>,
    /// Old runs converted into run files.
    pub(crate) converted: usize,
    /// Old runs tried this time that did not convert.
    pub(crate) failed: usize,
    /// One warning per key dropped from an installed blueprint, and per
    /// blueprint that could not be upgraded.
    pub(crate) warnings: Vec<String>,
    /// Each key dropped from the blueprints of converted runs, by blueprint
    /// and line, with how many runs it was dropped from: a thousand runs of
    /// one blueprint warn once.
    pub(crate) dropped_in_runs: BTreeMap<(String, String), usize>,
    /// Where the runs that did not convert are listed.
    pub(crate) unconverted: Option<PathBuf>,
}

impl Upgrade {
    /// Whether it did anything worth saying.
    fn is_empty(&self) -> bool {
        self.blueprints + self.blueprints_failed + self.converted + self.failed == 0
    }

    /// Add what converting the old runs did, to an upgrade that has not
    /// converted any.
    pub(crate) fn add_runs(&mut self, runs: Upgrade) {
        self.converted = runs.converted;
        self.failed = runs.failed;
        self.unconverted = runs.unconverted;
        self.dropped_in_runs = runs.dropped_in_runs;
    }

    /// Add a dropped key of the blueprint `name`, from one converted run.
    #[cfg(feature = "legacy-runs")]
    pub(crate) fn dropped_in_run(&mut self, name: &str, line: String) {
        *self
            .dropped_in_runs
            .entry((name.to_string(), line))
            .or_default() += 1;
    }

    /// The summary of what was done, a line each, the warnings after it.
    /// Nothing when nothing was done.
    pub(crate) fn lines(&self, backup: &Backup) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut done = vec![
            count(self.blueprints, "blueprint", "upgraded"),
            count(self.converted, "old run", "converted"),
        ];
        if self.blueprints_failed > 0 {
            done.push(count(
                self.blueprints_failed,
                "blueprint",
                "left as it was (see below)",
            ));
        }
        if self.failed > 0 {
            let list = self
                .unconverted
                .as_ref()
                .map_or_else(String::new, |p| format!(", listed in {}", p.display()));
            done.push(format!(
                "{} left as {} (could not be converted{list})",
                count(self.failed, "old run", ""),
                plural(self.failed, "it was", "they were")
            ));
        }
        let mut lines = vec![format!(
            "Upgraded this home for Leviath {}: {}. Everything it changed was saved first to {}; \
             Leviath never deletes it.",
            env!("CARGO_PKG_VERSION"),
            done.join(", "),
            backup.dir().display()
        )];
        lines.extend(self.warnings.iter().map(|w| format!("warning: {w}")));
        lines.extend(self.dropped_in_runs.iter().map(|((name, line), runs)| {
            format!(
                "warning: blueprint '{name}' of {}: {line}",
                count(*runs, "converted run", "")
            )
        }));
        lines
    }

    /// Log the summary, and keep it in `backup` for the next command to
    /// show once.
    ///
    /// A blueprint that cannot be upgraded is tried again on every start and
    /// left as it was each time. A start that did nothing else, with every
    /// blueprint it left already named by an earlier summary, says nothing:
    /// `lev list` names what is still waiting.
    pub(crate) fn finish(&self, backup: &Backup) -> Vec<String> {
        let named_before = backup.name_left(&self.left);
        let only_left =
            self.blueprints + self.converted + self.failed == 0 && self.dropped_in_runs.is_empty();
        if only_left && named_before {
            return Vec::new();
        }
        let lines = self.lines(backup);
        for line in &lines {
            tracing::info!(upgrade = %line, "upgraded this home from an earlier release");
        }
        backup.announce_later(&lines);
        lines
    }
}

/// `3 blueprints upgraded`, `1 old run converted`.
fn count(n: usize, what: &str, done: &str) -> String {
    let many = format!("{what}s");
    format!("{n} {} {done}", plural(n, what, &many))
        .trim_end()
        .to_string()
}

/// `one` for 1, `many` for any other number.
fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    match n {
        1 => one,
        _ => many,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upgrade_that_did_nothing_says_nothing() {
        let home = tempfile::tempdir().unwrap();
        let backup = Backup::of_home(home.path());
        assert!(Upgrade::default().finish(&backup).is_empty());
        assert!(Upgrade::default().lines(&backup).is_empty());
    }

    #[test]
    fn the_summary_counts_what_was_done_and_warns_once_per_key() {
        let home = tempfile::tempdir().unwrap();
        let backup = Backup::of_home(home.path());
        let mut upgrade = Upgrade {
            blueprints: 3,
            converted: 1,
            warnings: vec!["blueprint 'a': [agent]: `x = 1` was dropped".to_string()],
            ..Upgrade::default()
        };
        upgrade.dropped_in_run("r", "region 'log': `max_stored = 5` was dropped".into());
        upgrade.dropped_in_run("r", "region 'log': `max_stored = 5` was dropped".into());
        let lines = upgrade.lines(&backup);
        assert_eq!(lines.len(), 3, "{lines:?}");
        let first = &lines[0];
        assert!(
            first.contains("3 blueprints upgraded, 1 old run converted."),
            "{first}"
        );
        assert!(lines[0].contains(&backup.dir().display().to_string()));
        assert_eq!(
            lines[1],
            "warning: blueprint 'a': [agent]: `x = 1` was dropped"
        );
        assert_eq!(
            lines[2],
            "warning: blueprint 'r' of 2 converted runs: region 'log': `max_stored = 5` was dropped"
        );
    }

    /// A blueprint that cannot be upgraded is tried again on every start. The
    /// summary names it once; later starts that change nothing else say
    /// nothing, and `lev list` keeps naming it.
    #[test]
    fn a_blueprint_left_as_it_was_is_announced_once() {
        let home = tempfile::tempdir().unwrap();
        let backup = Backup::of_home(home.path());
        // The first start saved something, so there is a backup to speak of.
        let saved = home.path().join("saved");
        std::fs::create_dir_all(&saved).unwrap();
        backup.save_blueprint(&saved, None).unwrap();
        let left = |extra: usize| Upgrade {
            blueprints: extra,
            blueprints_failed: 1,
            left: vec!["/h/agents/broken".to_string()],
            warnings: vec!["blueprint 'broken' at /h/agents/broken was left as it was".into()],
            ..Upgrade::default()
        };
        assert_eq!(left(1).finish(&backup).len(), 2);
        assert_eq!(crate::home_backup::announce(home.path()).len(), 2);
        for _ in 0..3 {
            assert!(left(0).finish(&backup).is_empty());
            assert!(crate::home_backup::announce(home.path()).is_empty());
        }
        // Something new is said, with the blueprint still left beside it.
        assert_eq!(left(2).finish(&backup).len(), 2);
        // A second blueprint left as it was is news too.
        let two = Upgrade {
            left: vec![
                "/h/agents/broken".to_string(),
                "/h/agents/other".to_string(),
            ],
            ..left(0)
        };
        assert!(!two.finish(&backup).is_empty());
    }

    #[test]
    fn what_was_left_as_it_was_is_counted_and_where_it_is_listed() {
        let home = tempfile::tempdir().unwrap();
        let backup = Backup::of_home(home.path());
        let one = Upgrade {
            blueprints_failed: 1,
            failed: 1,
            unconverted: Some(PathBuf::from("/h/runs.unconverted")),
            ..Upgrade::default()
        };
        let line = &one.lines(&backup)[0];
        assert!(
            line.contains(
                "0 blueprints upgraded, 0 old runs converted, 1 blueprint left as it was (see \
                 below), 1 old run left as it was (could not be converted, listed in \
                 /h/runs.unconverted)"
            ),
            "{line}"
        );
        let two = Upgrade {
            failed: 2,
            ..Upgrade::default()
        };
        let line = &two.lines(&backup)[0];
        assert!(
            line.contains("2 old runs left as they were (could not be converted)"),
            "{line}"
        );
    }
}
