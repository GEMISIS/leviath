//! The crate's single key → semantic-action table.
//!
//! Every Leviath TUI resolves navigation and app-level keys through
//! [`resolve`], so "what does Esc do" has exactly one answer across surfaces:
//! arrows move (with `k`/`j`/`h`/`l` as vim aliases), Space toggles, Enter
//! acts on the focused thing, Esc goes back, Tab/Shift-Tab step between
//! screens or panes, `?` opens help, `q` quits, and Ctrl-C force-quits.
//!
//! Surfaces match their surface-specific keys (`o` signup, `x` kill, …)
//! *before* calling [`resolve`], and only fall through to it for the shared
//! set. That keeps this table frozen while surfaces evolve, and it means a
//! surface can shadow an alias that clashes with a local binding (the
//! dashboard's `l` = Logs wins over "l = Right") without touching this
//! module.
//!
//! [`resolve`] must never be called while a text field is being edited -
//! letters are letters there; text inputs own the raw key stream.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A key's shared meaning, independent of which surface received it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// `↑` / `k`: move the cursor up, or scroll up.
    Up,
    /// `↓` / `j`: move the cursor down, or scroll down.
    Down,
    /// `←` / `h`: adjust a value down, move focus left, or previous tab.
    Left,
    /// `→` / `l`: adjust a value up, move focus right, or next tab.
    Right,
    /// Space: toggle the focused row in a multi-select.
    Toggle,
    /// Enter: act on / affirm the focused thing. Never "silently advance".
    Activate,
    /// Esc: back / dismiss. Never quit-from-main.
    Back,
    /// Tab: next screen / pane.
    Next,
    /// Shift-Tab / BackTab: previous screen / pane.
    Prev,
    /// `?`: open the help overlay.
    Help,
    /// `q`: quit the app (surfaces confirm first when there is unsaved state).
    Quit,
    /// Ctrl-C: quit immediately, honored in every mode.
    ForceQuit,
}

/// Resolve a key event to its shared [`Action`], or `None` when the key has
/// no crate-wide meaning (the surface then handles or ignores it).
pub(crate) fn resolve(key: &KeyEvent) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(Action::ForceQuit),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Some(Action::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::Down),
        KeyCode::Left | KeyCode::Char('h') => Some(Action::Left),
        KeyCode::Right | KeyCode::Char('l') => Some(Action::Right),
        KeyCode::Char(' ') => Some(Action::Toggle),
        KeyCode::Enter => Some(Action::Activate),
        KeyCode::Esc => Some(Action::Back),
        // BackTab already implies Shift on most terminals; some deliver
        // Tab + SHIFT instead, so both spellings mean "previous".
        KeyCode::BackTab => Some(Action::Prev),
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => Some(Action::Prev),
        KeyCode::Tab => Some(Action::Next),
        KeyCode::Char('?') => Some(Action::Help),
        KeyCode::Char('q') => Some(Action::Quit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn arrows_and_vim_aliases_resolve_to_the_same_movement() {
        for (code, alias, action) in [
            (KeyCode::Up, 'k', Action::Up),
            (KeyCode::Down, 'j', Action::Down),
            (KeyCode::Left, 'h', Action::Left),
            (KeyCode::Right, 'l', Action::Right),
        ] {
            assert_eq!(resolve(&press(code)), Some(action));
            assert_eq!(resolve(&press(KeyCode::Char(alias))), Some(action));
        }
    }

    #[test]
    fn the_shared_app_keys_resolve() {
        assert_eq!(resolve(&press(KeyCode::Char(' '))), Some(Action::Toggle));
        assert_eq!(resolve(&press(KeyCode::Enter)), Some(Action::Activate));
        assert_eq!(resolve(&press(KeyCode::Esc)), Some(Action::Back));
        assert_eq!(resolve(&press(KeyCode::Tab)), Some(Action::Next));
        assert_eq!(resolve(&press(KeyCode::BackTab)), Some(Action::Prev));
        assert_eq!(resolve(&press(KeyCode::Char('?'))), Some(Action::Help));
        assert_eq!(resolve(&press(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn shift_tab_spelled_as_tab_plus_shift_means_previous() {
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(resolve(&key), Some(Action::Prev));
    }

    #[test]
    fn question_mark_still_resolves_when_the_terminal_reports_shift() {
        // Shift+/ produces '?' and some terminals set the SHIFT modifier.
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert_eq!(resolve(&key), Some(Action::Help));
    }

    #[test]
    fn ctrl_c_force_quits_and_other_ctrl_chords_resolve_to_nothing() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(resolve(&ctrl_c), Some(Action::ForceQuit));

        // Ctrl-K must NOT read as movement: chords are not plain keys.
        let ctrl_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(resolve(&ctrl_k), None);
    }

    #[test]
    fn unmapped_keys_resolve_to_nothing() {
        assert_eq!(resolve(&press(KeyCode::Char('z'))), None);
        assert_eq!(resolve(&press(KeyCode::F(5))), None);
        assert_eq!(resolve(&press(KeyCode::Home)), None);
    }
}
