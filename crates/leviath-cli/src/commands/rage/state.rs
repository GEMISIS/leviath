//! The screen's state and what a key does to it. Nothing here draws.
//!
//! Four steps: what the problem was about, which run or blueprint (when the
//! category needs one), what happened in the user's words, and the summary
//! of what was written. A flag answers a step ahead of time, and the screen
//! opens on the first step nothing has answered.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leviath_core::run_meta::RunMeta;
use ratatui_textarea::TextArea;

use super::collect::{Selection, installed_blueprints, list_metas, resolve_run_id};
use super::{About, Outcome, RageArgs, RageEnv};
use crate::tui::widgets::picker::{Picker, PickerOption, PickerOutcome};

/// Where the screen is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The category picker.
    About,
    /// The run picker.
    Run,
    /// The blueprint picker.
    Agent,
    /// The free-text note.
    Note,
    /// What was written, and the warning.
    Summary,
}

impl Step {
    /// The breadcrumb: the run and blueprint pickers share one slot, since a
    /// bundle has at most one of them.
    pub(crate) const TITLES: [&'static str; 4] = [
        "What went wrong",
        "Which one",
        "What happened",
        "Your bundle",
    ];

    /// This step's slot in [`Self::TITLES`].
    pub(crate) fn index(self) -> usize {
        match self {
            Step::About => 0,
            Step::Run | Step::Agent => 1,
            Step::Note => 2,
            Step::Summary => 3,
        }
    }
}

/// The screen.
pub(crate) struct Rage {
    pub step: Step,
    pub about: Option<About>,
    pub run_id: Option<String>,
    pub agent: Option<PathBuf>,
    /// The note, as typed.
    pub note: TextArea<'static>,
    /// Which answers came from flags, so going back skips them.
    about_preset: bool,
    which_preset: bool,
    note_preset: bool,
    pub include_blobs: bool,
    pub output: Option<PathBuf>,
    /// The open chooser, on the steps that have one.
    pub picker: Option<Picker>,
    /// Every run on disk, newest first.
    pub runs: Vec<RunMeta>,
    /// Every installed blueprint, by name.
    pub blueprints: Vec<(String, PathBuf)>,
    /// A line for the footer: a refusal, or a hint.
    pub message: Option<String>,
    /// What was written, once the summary step has built it.
    pub outcome: Option<Outcome>,
    /// Why nothing was written, when the build failed.
    pub error: Option<String>,
    pub finished: bool,
    pub should_quit: bool,
}

impl Rage {
    /// A screen open on the first step the flags did not answer. A `--run`
    /// that names no run is refused here, before any terminal is taken.
    pub(crate) fn new(args: &RageArgs, env: &RageEnv) -> anyhow::Result<Self> {
        let runs = list_metas(&env.runs_dir);
        let blueprints = installed_blueprints(&env.agents_dir);
        let run_id = match &args.run {
            Some(given) => Some(resolve_run_id(&runs, given).map_err(|e| anyhow::anyhow!(e))?),
            None => None,
        };
        let about = args.about.or(if run_id.is_some() {
            Some(About::Run)
        } else if args.agent.is_some() {
            Some(About::Agent)
        } else {
            None
        });
        let mut note = TextArea::new(
            args.note
                .as_deref()
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect(),
        );
        note.set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
        note.move_cursor(ratatui_textarea::CursorMove::Bottom);
        note.move_cursor(ratatui_textarea::CursorMove::End);
        let which_preset = run_id.is_some() || args.agent.is_some();
        let mut ui = Self {
            step: Step::About,
            about,
            run_id,
            agent: args.agent.clone(),
            note,
            about_preset: about.is_some(),
            which_preset,
            note_preset: args.note.is_some(),
            include_blobs: !args.no_blobs,
            output: args.output.clone(),
            picker: None,
            runs,
            blueprints,
            message: None,
            outcome: None,
            error: None,
            finished: false,
            should_quit: false,
        };
        ui.enter(ui.first_open_step());
        Ok(ui)
    }

    /// The first step no flag answered.
    fn first_open_step(&self) -> Step {
        match self.about {
            None => Step::About,
            Some(about) => self.step_after_about(about),
        }
    }

    /// Where choosing a category leads.
    fn step_after_about(&self, about: About) -> Step {
        match about {
            About::Run if !self.which_preset => Step::Run,
            About::Agent if !self.which_preset => Step::Agent,
            _ if !self.note_preset => Step::Note,
            _ => Step::Summary,
        }
    }

    /// Move to `step`, opening its chooser.
    fn enter(&mut self, step: Step) {
        self.step = step;
        self.message = None;
        self.picker = match step {
            Step::About => Some(about_picker(self.about)),
            Step::Run => Some(run_picker(&self.runs)),
            Step::Agent => Some(agent_picker(&self.blueprints)),
            Step::Note | Step::Summary => None,
        };
        if step == Step::Run && self.runs.is_empty() {
            self.message =
                Some("No runs on disk. Esc goes back; pick another category.".to_string());
        }
        if step == Step::Agent && self.blueprints.is_empty() {
            self.message =
                Some("No installed blueprints. Esc goes back, or pass --agent <dir>.".to_string());
        }
    }

    /// The step before this one that a person can change. Leaving the first
    /// step quits. The summary never comes here (its keys all finish), and is
    /// grouped with the first step so the match stays exhaustive.
    fn back(&mut self) {
        match self.step {
            Step::About | Step::Summary => self.should_quit = true,
            Step::Run | Step::Agent => {
                if self.about_preset {
                    self.should_quit = true;
                } else {
                    self.enter(Step::About);
                }
            }
            Step::Note => match (self.about, self.which_preset, self.about_preset) {
                (Some(About::Run), false, _) => self.enter(Step::Run),
                (Some(About::Agent), false, _) => self.enter(Step::Agent),
                (_, _, false) => self.enter(Step::About),
                (_, _, true) => self.should_quit = true,
            },
        }
    }

    /// Whether the summary step still has to build the bundle.
    pub(crate) fn needs_build(&self) -> bool {
        self.step == Step::Summary && self.outcome.is_none() && self.error.is_none()
    }

    /// What the answers so far describe.
    pub(crate) fn selection(&self) -> Selection {
        Selection {
            about: self.about.unwrap_or(About::Other),
            run_id: self.run_id.clone(),
            agent: self.agent.clone(),
            note: self.note.lines().join("\n"),
            include_blobs: self.include_blobs,
        }
    }

    /// Apply one key.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.step {
            Step::About => self.picker_step(&key, Self::choose_about),
            Step::Run => self.picker_step(&key, Self::choose_run),
            Step::Agent => self.picker_step(&key, Self::choose_agent),
            Step::Note => self.handle_note_key(key),
            Step::Summary => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q')) {
                    self.finished = true;
                }
            }
        }
    }

    /// A key on a step with a chooser: the chooser decides, and an answer
    /// goes to `choose`. The pickers here are single-select, so a
    /// multi-select answer is treated like no answer.
    fn picker_step(&mut self, key: &KeyEvent, choose: fn(&mut Self, usize)) {
        let picker = self
            .picker
            .as_mut()
            .expect("every picker step opens its chooser on entry");
        match picker.handle_key(key) {
            PickerOutcome::Chosen(index) => choose(self, index),
            PickerOutcome::Cancelled => self.back(),
            PickerOutcome::Pending | PickerOutcome::ChosenMany(_) => {}
        }
    }

    fn choose_about(&mut self, index: usize) {
        let about = ABOUT_ORDER[index.min(ABOUT_ORDER.len() - 1)];
        self.about = Some(about);
        let next = self.step_after_about(about);
        self.enter(next);
    }

    /// The chooser's rows mirror `runs` one to one, so its index is ours.
    fn choose_run(&mut self, index: usize) {
        self.run_id = Some(self.runs[index].run_id.clone());
        self.enter(self.step_after_which());
    }

    fn choose_agent(&mut self, index: usize) {
        self.agent = Some(self.blueprints[index].1.clone());
        self.enter(self.step_after_which());
    }

    /// Where the run or blueprint choice leads.
    fn step_after_which(&self) -> Step {
        if self.note_preset {
            Step::Summary
        } else {
            Step::Note
        }
    }

    fn handle_note_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.back(),
            KeyCode::Char('s') if ctrl => self.enter(Step::Summary),
            _ => {
                self.note.input(ratatui_textarea::Input::from(key));
            }
        }
    }
}

/// The category picker's rows, in the order the picker shows them.
const ABOUT_ORDER: [About; 4] = [About::Setup, About::Run, About::Agent, About::Other];

fn about_picker(current: Option<About>) -> Picker {
    let cursor = current
        .and_then(|about| ABOUT_ORDER.iter().position(|a| *a == about))
        .unwrap_or(0);
    Picker::new(
        "What went wrong?",
        vec![
            "This decides what the bundle carries. Everything else (config, logs, doctor) is always in."
                .to_string(),
        ],
        vec![
            option("Setting up", "keys, providers, the wizard"),
            option("A run", "an agent that failed, hung or misbehaved"),
            option("Building a blueprint", "validate, deps, tools, a stage graph"),
            option("Something else", "the dashboard, the API, the CLI itself"),
        ],
        cursor,
    )
}

fn run_picker(runs: &[RunMeta]) -> Picker {
    let options = runs
        .iter()
        .map(|meta| {
            let started = chrono::DateTime::from_timestamp(meta.started_at, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            option(
                &meta.run_id,
                &format!("{} · {} · {started}", meta.agent_name, meta.status.wire()),
            )
        })
        .collect();
    Picker::new(
        "Which run?",
        vec!["Newest first. Type to filter by id, agent or status.".to_string()],
        options,
        0,
    )
}

fn agent_picker(blueprints: &[(String, PathBuf)]) -> Picker {
    let options = blueprints
        .iter()
        .map(|(name, dir)| option(name, &dir.display().to_string()))
        .collect();
    Picker::new(
        "Which blueprint?",
        vec![
            "Installed blueprints. For one that is not installed, run again with --agent <dir>."
                .to_string(),
        ],
        options,
        0,
    )
}

fn option(value: &str, detail: &str) -> PickerOption {
    PickerOption {
        value: value.to_string(),
        detail: detail.to_string(),
    }
}
