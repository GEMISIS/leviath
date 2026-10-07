use super::*;

/// A writer whose bytes a test can read back.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

fn step(step: &str, done: u64, total: u64, detail: Option<&str>) -> StartupProgress {
    StartupProgress {
        step: step.to_string(),
        done,
        total,
        detail: detail.map(str::to_string),
    }
}

/// A home whose upgrade left a summary nobody has seen.
fn upgraded_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let run = home.path().join("old-run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("meta.json"), "{}").unwrap();
    let backup = crate::home_backup::Backup::of_home(home.path());
    backup.save_run(&run).unwrap();
    backup.announce_later(&["Upgraded this home: 1 old run converted.".to_string()]);
    home
}

/// On a terminal the line is redrawn in place, the backup line is printed
/// once above it, and readiness clears it and shows the summary once.
#[test]
fn a_terminal_sees_a_bar_redrawn_in_place_and_then_the_summary() {
    let home = upgraded_home();
    let out = Shared::default();
    let view = StartupView::new(Box::new(out.clone()), true, Some(home.path().to_path_buf()));
    let watch = view.watch();
    watch(StartupEvent::Progress(&StartupProgress::default()));
    assert_eq!(out.text(), "", "no step begun yet");
    watch(StartupEvent::Progress(&step(
        "connecting MCP servers",
        0,
        0,
        None,
    )));
    assert_eq!(
        out.text(),
        "\r\x1b[2Kleviath daemon starting: connecting MCP servers..."
    );
    let saving = Some("saving everything it changes to /b first");
    watch(StartupEvent::Progress(&step(
        "converting runs",
        0,
        4,
        saving,
    )));
    assert_eq!(
        out.text(),
        "\r\x1b[2Kleviath: saving everything it changes to /b first\n\
         \r\x1b[2Kleviath daemon starting: converting runs [------------------------] 0/4"
    );
    watch(StartupEvent::Progress(&step(
        "converting runs",
        3,
        4,
        saving,
    )));
    assert_eq!(
        out.text(),
        "\r\x1b[2Kleviath daemon starting: converting runs [##################------] 3/4"
    );
    watch(StartupEvent::Ready);
    assert_eq!(
        out.text(),
        "\r\x1b[2KUpgraded this home: 1 old run converted.\n"
    );
    // Shown once.
    view.ready();
    assert_eq!(out.text(), "");
}

/// Anything but a terminal gets plain lines, one as each counted step
/// begins, and no escape codes; a step that is not counted says nothing.
#[test]
fn a_pipe_sees_plain_lines_and_no_escapes() {
    let home = upgraded_home();
    let out = Shared::default();
    let view = StartupView::new(
        Box::new(out.clone()),
        false,
        Some(home.path().to_path_buf()),
    );
    view.show(&step("connecting MCP servers", 0, 0, None));
    let saving = Some("saving everything it changes to /b first");
    view.show(&step("upgrading blueprints", 0, 2, saving));
    view.show(&step("upgrading blueprints", 1, 2, saving));
    view.show(&step("converting runs", 0, 982, saving));
    view.ready();
    let text = out.text();
    assert_eq!(
        text,
        "leviath: saving everything it changes to /b first\n\
         leviath: upgrading blueprints 0/2\n\
         leviath: converting runs 0/982\n\
         Upgraded this home: 1 old run converted.\n"
    );
    assert!(!text.contains('\x1b'));
}

/// A view with no home to speak for only clears its line when the daemon is
/// ready: the summary is left for the next command.
#[test]
fn a_view_with_no_home_leaves_the_summary_for_later() {
    let home = upgraded_home();
    let out = Shared::default();
    let view = StartupView::new(Box::new(out.clone()), true, None);
    view.ready();
    assert_eq!(out.text(), "", "nothing drawn, nothing to clear");
    assert_eq!(crate::home_backup::announce(home.path()).len(), 1);
}

/// A daemon in the foreground follows its own board until it is ready, and
/// a board with no step yet shows nothing.
#[tokio::test]
async fn a_foreground_daemon_follows_its_own_board() {
    let out = Shared::default();
    let view = StartupView::new(Box::new(out.clone()), true, None);
    let board = StartupBoard::default();
    let following = view.follow(board.clone());
    tokio::time::sleep(FOLLOW_EVERY * 2).await;
    assert_eq!(out.text(), "", "no step yet");
    board.begin("converting runs", 2);
    board.done(1);
    tokio::time::sleep(FOLLOW_EVERY * 3).await;
    following.finish().await;
    let text = out.text();
    assert!(text.contains("converting runs ["), "{text}");
    assert!(text.ends_with(CLEAR_LINE), "{text:?}");
}

/// A view on stderr that is not to announce speaks for no home, so readiness
/// leaves the summary where it is.
#[test]
fn a_quiet_view_on_stderr_speaks_for_no_home() {
    assert!(StartupView::on_stderr(false).root.is_none());
}
