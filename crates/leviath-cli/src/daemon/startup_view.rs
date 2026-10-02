//! What a person waiting on a starting daemon sees.
//!
//! A daemon answers every request with what it is doing until it is ready
//! (see [`leviath_runtime::control_socket::StartupBoard`]), and the first
//! start after an upgrade from an earlier release can take a while: it backs
//! up the home, upgrades its blueprints and converts its old runs. Whoever is
//! waiting is shown how far along it is, on stderr: a progress line redrawn
//! in place on a terminal, and on anything else (a pipe, a log) a plain line
//! as each counted step begins, with no escape codes. When the daemon is
//! ready the line is cleared, and the summary of the upgrade is shown once
//! (see [`crate::home_backup::announce`]).

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use leviath_runtime::control_socket::{StartupBoard, StartupEvent, StartupProgress, StartupWatch};

/// How wide the progress bar is, in characters.
const BAR: u64 = 24;

/// Erase the terminal line the cursor is on and go back to its start.
const CLEAR_LINE: &str = "\r\x1b[2K";

/// How often the daemon's own view reads its board.
const FOLLOW_EVERY: Duration = Duration::from_millis(100);

/// Where the view writes.
pub type Out = Box<dyn Write + Send>;

/// What a view has shown so far.
#[derive(Default)]
struct Seen {
    step: Option<String>,
    detail: Option<String>,
    /// Whether a progress line is on the terminal now.
    drawn: bool,
}

/// Shows a starting daemon's progress to the person waiting on it.
pub struct StartupView {
    out: Mutex<Out>,
    tty: bool,
    /// The data root whose upgrade summary is shown once the daemon is
    /// ready, when this view is the one to show it.
    root: Option<PathBuf>,
    seen: Mutex<Seen>,
}

impl StartupView {
    /// A view writing to `out`, a terminal when `tty`, that shows the upgrade
    /// summary of the home at `root` when the daemon is ready.
    pub fn new(out: Out, tty: bool, root: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            out: Mutex::new(out),
            tty,
            root,
            seen: Mutex::default(),
        })
    }

    /// A view on this process's stderr, redrawn in place when that is a
    /// terminal. With `announce` it shows the summary of an upgrade of this
    /// home nobody has seen once the daemon is ready; a daemon nobody watches
    /// leaves that for the next command a person runs.
    pub fn on_stderr(announce: bool) -> Arc<Self> {
        Self::new(
            Box::new(std::io::stderr()),
            std::io::IsTerminal::is_terminal(&std::io::stderr()),
            leviath_core::paths::data_dir().filter(|_| announce),
        )
    }

    /// Show where the daemon's start-up is now. A daemon that has not begun
    /// its first step yet has nothing to show.
    pub fn show(&self, now: &StartupProgress) {
        if now.step.is_empty() {
            return;
        }
        let mut seen = leviath_core::sync::lock(&self.seen);
        let mut text = String::new();
        if now.detail.is_some() && seen.detail != now.detail {
            text.push_str(self.cleared(&mut seen));
            text.push_str(&format!(
                "leviath: {}\n",
                now.detail.as_deref().unwrap_or_default()
            ));
        }
        seen.detail.clone_from(&now.detail);
        let new_step = seen.step.as_deref() != Some(now.step.as_str());
        seen.step = Some(now.step.clone());
        match self.tty {
            true => {
                text.push_str(CLEAR_LINE);
                text.push_str(&progress_line(now));
                seen.drawn = true;
            }
            false if new_step && now.total > 0 => text.push_str(&format!("leviath: {now}\n")),
            false => {}
        }
        self.write(&text);
    }

    /// The daemon is ready: clear the progress line, and show the upgrade's
    /// summary when there is one nobody has seen.
    pub fn ready(&self) {
        let mut seen = leviath_core::sync::lock(&self.seen);
        let mut text = self.cleared(&mut seen).to_string();
        for line in self
            .root
            .iter()
            .flat_map(|r| crate::home_backup::announce(r))
        {
            text.push_str(&line);
            text.push('\n');
        }
        self.write(&text);
    }

    /// What erases the progress line, when one is drawn.
    fn cleared(&self, seen: &mut Seen) -> &'static str {
        match std::mem::take(&mut seen.drawn) {
            true => CLEAR_LINE,
            false => "",
        }
    }

    fn write(&self, text: &str) {
        let mut out = leviath_core::sync::lock(&self.out);
        let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
    }

    /// This view as the watch a control client tells.
    pub fn watch(self: &Arc<Self>) -> StartupWatch {
        let view = self.clone();
        Arc::new(move |event| match event {
            StartupEvent::Progress(now) => view.show(now),
            StartupEvent::Ready => view.ready(),
        })
    }

    /// Follow `board` until the returned [`Following`] is finished: the
    /// view a daemon in the foreground keeps of its own start-up.
    pub fn follow(self: &Arc<Self>, board: StartupBoard) -> Following {
        let view = self.clone();
        let task = tokio::spawn(async move {
            loop {
                view.show(&board.current());
                tokio::time::sleep(FOLLOW_EVERY).await;
            }
        });
        Following {
            task,
            view: self.clone(),
        }
    }
}

/// A view following a board; see [`StartupView::follow`].
pub struct Following {
    task: tokio::task::JoinHandle<()>,
    view: Arc<StartupView>,
}

impl Following {
    /// Stop following: the daemon is ready.
    pub async fn finish(self) {
        self.task.abort();
        let _ = self.task.await;
        self.view.ready();
    }
}

/// `leviath daemon starting: converting runs [######------] 412/982`.
fn progress_line(now: &StartupProgress) -> String {
    let head = format!("leviath daemon starting: {}", now.step);
    match now.total {
        0 => format!("{head}..."),
        total => {
            let filled = (now.done.min(total) * BAR / total) as usize;
            format!(
                "{head} [{}{}] {}/{total}",
                "#".repeat(filled),
                "-".repeat(BAR as usize - filled),
                now.done
            )
        }
    }
}

#[cfg(test)]
#[path = "startup_view_tests.rs"]
mod tests;
