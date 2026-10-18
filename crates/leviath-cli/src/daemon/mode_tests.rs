use super::*;
use clap::Parser;

/// A command line with nothing but the shared flag, to parse it as a front
/// door does.
#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    daemon: DaemonModeArg,
}

fn parsed(argv: &[&str]) -> DaemonModeArg {
    Cli::try_parse_from(std::iter::once("lev").chain(argv.iter().copied()))
        .expect("the flag parses")
        .daemon
}

/// `attach` replaces nothing; `ensure` replaces only a daemon known to be
/// older; `replace` also replaces one of the same version it cannot put in
/// order, such as a rebuild of one commit. None replaces the same build or a
/// newer one.
#[test]
fn each_mode_replaces_only_what_it_says() {
    use Standing::{Newer, Older, Same, Unordered};
    let replaced = |mode: DaemonMode| {
        [Same, Older, Newer, Unordered]
            .into_iter()
            .filter(|standing| mode.replaces(*standing))
            .collect::<Vec<_>>()
    };
    assert_eq!(replaced(DaemonMode::Attach), []);
    assert_eq!(replaced(DaemonMode::Ensure), [Older]);
    assert_eq!(replaced(DaemonMode::Replace), [Older, Unordered]);
    assert_eq!(DaemonMode::default(), DaemonMode::Ensure);
}

/// Only `attach` refuses to start a daemon.
#[test]
fn only_attach_never_starts_one() {
    assert!(!DaemonMode::Attach.starts());
    assert!(DaemonMode::Ensure.starts());
    assert!(DaemonMode::Replace.starts());
}

/// The flag beats the environment; the environment beats the default; an
/// empty value counts as unset, case does not matter, and a value that names
/// no mode is refused with what it should have been.
#[test]
fn the_flag_beats_the_env_which_beats_the_default() {
    assert_eq!(parsed(&[]).resolve(None).unwrap(), DaemonMode::Ensure);
    let attach = parsed(&["--daemon", "attach"]);
    assert_eq!(attach.resolve(None).unwrap(), DaemonMode::Attach);
    assert_eq!(attach.resolve(Some("replace")).unwrap(), DaemonMode::Attach);
    assert_eq!(
        parsed(&[]).resolve(Some(" Replace ")).unwrap(),
        DaemonMode::Replace
    );
    assert_eq!(parsed(&[]).resolve(Some("")).unwrap(), DaemonMode::Ensure);
    let refused = parsed(&[]).resolve(Some("sometimes")).unwrap_err();
    assert_eq!(
        refused.to_string(),
        r#"LEVIATH_DAEMON is "sometimes"; expected attach, ensure or replace"#
    );
    assert!(Cli::try_parse_from(["lev", "--daemon", "sometimes"]).is_err());
}

/// `resolve_here` reads `LEVIATH_DAEMON` from this process's environment.
#[test]
fn resolve_here_reads_the_environment() {
    temp_env::with_var(ENV, Some("attach"), || {
        assert_eq!(
            DaemonModeArg::default().resolve_here().unwrap(),
            DaemonMode::Attach
        );
    });
    temp_env::with_var_unset(ENV, || {
        assert_eq!(
            DaemonModeArg::default().resolve_here().unwrap(),
            DaemonMode::Ensure
        );
    });
}
