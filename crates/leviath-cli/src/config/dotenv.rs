//! Reading a repository's `.env` on `Config::load`.
//!
//! Only the variables `leviath_core::dotenv_var_allowed` admits are set; the
//! rest are named in one warning. A variable already in the environment wins.

/// Re-quote an already-parsed value so dotenvy reads it back unchanged.
///
/// Double quotes, not single. Single quotes look right - dotenvy's *value*
/// parser treats everything inside them literally - but its *line reader* is a
/// separate state machine that honours `\` escapes inside single quotes. The
/// two disagree, so a value ending in a backslash ate its own closing quote,
/// swallowed the next line, and failed the whole document. Since the load
/// result is discarded, every variable after it vanished with no warning.
///
/// Inside double quotes both layers agree on the same escape set, so escaping
/// `\`, `"`, `$` and a newline round-trips exactly. Escaping `$` is also what
/// stops a second substitution pass: these values were already `$VAR`-expanded
/// by the parse that produced them.
pub(super) fn requote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Set every variable in `path` that a repository's `.env` is allowed to set,
/// warning once about the rest.
///
/// Matches dotenvy's own precedence: a variable already present in the
/// environment wins, because the person who exported it meant it and a file in
/// a directory they happened to `cd` into did not.
///
/// A missing or unreadable `.env` is not an error - most working directories do
/// not have one.
pub(super) fn load_dotenv_filtered(path: &str) {
    let Ok(entries) = dotenvy::from_filename_iter(path) else {
        return;
    };
    // A malformed line is skipped rather than ending the read, so one bad entry
    // costs its own variable and not every variable after it.
    let (allowed, skipped): (Vec<_>, Vec<_>) = entries
        .flatten()
        .partition(|(key, _)| leviath_core::dotenv_var_allowed(key));

    // Hand the survivors back to dotenvy rather than calling `set_var` here:
    // the workspace forbids `unsafe`, and `std::env::set_var` is unsafe in
    // edition 2024.
    //
    // One path, not a fast path plus a filtered one. Re-reading the file when
    // nothing was filtered looked cheap, but it re-parsed content that could
    // have changed since the decision was made and gave the two paths
    // different error semantics for a malformed line. Always re-serializing
    // means what gets set is exactly what was inspected.
    let doc: String = allowed
        .iter()
        .map(|(key, value)| format!("{key}={}\n", requote(value)))
        .collect();
    let _ = dotenvy::from_read(doc.as_bytes());

    // The common case is that a `.env` sets nothing sensitive, and warning then
    // printed "Ignoring  from .env" with an empty list where a name belonged.
    if skipped.is_empty() {
        return;
    }

    // Joined before the macro rather than inside it: `tracing` does not
    // evaluate field expressions when no subscriber is interested, so an
    // argument built in place reads as an unexecuted region even on the run
    // that logged it.
    let names = skipped
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        "Ignoring {names} from {path}: these decide where configuration is read from or what \
         gets executed, so a repository may not set them. Export them yourself if you meant to."
    );
}
