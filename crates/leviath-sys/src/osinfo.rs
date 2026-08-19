//! The operating system's display name and version.
//!
//! `std::env::consts::OS` names the *target* (`macos`, `linux`, `windows`) and
//! nothing more. The release a machine actually runs is a per-platform file, so
//! it is read here rather than in a caller: Linux publishes `/etc/os-release`
//! and macOS a `SystemVersion.plist`, both plain text this can parse without a
//! dependency.
//!
//! Windows has no equivalent file. Its version lives in the registry or behind
//! `cmd /c ver`, and neither is reachable without either a new dependency or
//! spawning a process - which this crate will not do to answer a question this
//! minor. [`version_for`] reports `None` there, and the caller says so plainly
//! rather than guessing.
//!
//! Every function is pure over an injected reader except [`current_version`], so
//! the Linux and macOS branches are both reachable under test from any host.

/// Where each platform records its release, or `None` for a platform that does
/// not record it in a readable file.
///
/// Pure over `os` rather than `#[cfg]`-switched, following
/// [`crate::editor::default_editors_for`], so every arm is testable anywhere.
pub fn version_path_for(os: &str) -> Option<&'static str> {
    match os {
        "linux" | "android" => Some("/etc/os-release"),
        "macos" | "ios" => Some("/System/Library/CoreServices/SystemVersion.plist"),
        _ => None,
    }
}

/// A human display name for the target `os`, for a model that would otherwise
/// read the bare target triple word.
///
/// Falls back to the raw value, which is right for a platform this predates: a
/// caller reporting `freebsd` verbatim is more use than one reporting `unknown`.
pub fn display_name_for(os: &str) -> &str {
    match os {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        "netbsd" => "NetBSD",
        "openbsd" => "OpenBSD",
        "android" => "Android",
        "ios" => "iOS",
        other => other,
    }
}

/// `PRETTY_NAME` from an `/etc/os-release`, falling back to `VERSION_ID`.
///
/// `PRETTY_NAME` first because it is the one line that already reads as an
/// answer (`Ubuntu 24.04.1 LTS`); `VERSION_ID` alone (`24.04`) is the useful
/// remainder on a distribution that omits it. Values may be quoted with either
/// quote character, or not at all - all three spellings occur in the wild.
pub fn parse_os_release(contents: &str) -> Option<String> {
    let value_of = |key: &str| {
        contents.lines().find_map(|line| {
            let rest = line.trim().strip_prefix(key)?.strip_prefix('=')?;
            let unquoted = rest
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .or_else(|| rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
                .unwrap_or(rest);
            (!unquoted.trim().is_empty()).then(|| unquoted.trim().to_string())
        })
    };
    value_of("PRETTY_NAME").or_else(|| value_of("VERSION_ID"))
}

/// `ProductVersion` from a macOS `SystemVersion.plist`.
///
/// Scanned rather than parsed as XML: the file is a fixed, Apple-generated
/// `<key>`/`<string>` sequence, and a plist parser would be a dependency bought
/// to read one value out of one known file.
pub fn parse_system_version_plist(contents: &str) -> Option<String> {
    let after_key = contents.split("<key>ProductVersion</key>").nth(1)?;
    // `split_once` rather than `find` plus a range: byte offsets into a `str`
    // panic when they land inside a multi-byte character, and the workspace
    // denies `clippy::string_slice` for exactly that reason.
    let (_, after_open) = after_key.split_once("<string>")?;
    let (value, _) = after_open.split_once("</string>")?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The OS release for `os`, reading through `read`.
///
/// `&dyn Fn` rather than `impl Fn`: one monomorphization, so coverage is
/// attributed to a single instantiation rather than split across several.
pub fn version_for(os: &str, read: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let contents = read(version_path_for(os)?)?;
    match os {
        "macos" | "ios" => parse_system_version_plist(&contents),
        _ => parse_os_release(&contents),
    }
}

/// Read a file to a string, discarding the reason it could not be read.
///
/// A named `fn` rather than a closure at the [`current_version`] call site: a
/// closure written there would be a region the non-matching platform never
/// reaches, and so an uncovered one.
fn read_to_string(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The running OS release, or `None` where the platform does not publish one.
pub fn current_version() -> Option<String> {
    version_for(std::env::consts::OS, &read_to_string)
}

/// This machine's hostname, or `None` when the platform will not say.
///
/// Delegated to this crate's internal platform module, where the per-OS branch
/// lives: Unix asks `gethostname`, Windows reads `COMPUTERNAME`, and a target
/// that is neither reports nothing.
pub fn hostname() -> Option<String> {
    crate::platform::hostname()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_paths_are_known_for_the_platforms_that_publish_one() {
        assert_eq!(version_path_for("linux"), Some("/etc/os-release"));
        assert_eq!(version_path_for("android"), Some("/etc/os-release"));
        assert_eq!(
            version_path_for("macos"),
            Some("/System/Library/CoreServices/SystemVersion.plist")
        );
        assert_eq!(version_path_for("ios"), version_path_for("macos"));
        // Windows publishes no readable file, which is a `None` rather than a
        // path that would always fail to open.
        assert_eq!(version_path_for("windows"), None);
        assert_eq!(version_path_for("freebsd"), None);
    }

    #[test]
    fn display_names_are_capitalized_and_unknown_ones_pass_through() {
        assert_eq!(display_name_for("macos"), "macOS");
        assert_eq!(display_name_for("linux"), "Linux");
        assert_eq!(display_name_for("windows"), "Windows");
        assert_eq!(display_name_for("freebsd"), "FreeBSD");
        assert_eq!(display_name_for("netbsd"), "NetBSD");
        assert_eq!(display_name_for("openbsd"), "OpenBSD");
        assert_eq!(display_name_for("android"), "Android");
        assert_eq!(display_name_for("ios"), "iOS");
        // A platform this predates reports its own name rather than "unknown".
        assert_eq!(display_name_for("solaris"), "solaris");
    }

    /// All three quoting styles occur across distributions, and the whole line
    /// must match the key - `VERSION_ID` must not be answered by
    /// `IMAGE_VERSION_ID`.
    #[test]
    fn os_release_prefers_pretty_name_over_version_id() {
        let text = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n";
        assert_eq!(parse_os_release(text), Some("Ubuntu 24.04.1 LTS".into()));
    }

    #[test]
    fn os_release_falls_back_to_version_id_and_accepts_every_quoting() {
        assert_eq!(
            parse_os_release("NAME=\"Alpine Linux\"\nVERSION_ID=3.20.3\n"),
            Some("3.20.3".into())
        );
        assert_eq!(
            parse_os_release("PRETTY_NAME='Debian GNU/Linux 12'\n"),
            Some("Debian GNU/Linux 12".into())
        );
    }

    /// An empty value is not an answer: reporting `""` as the OS version would
    /// read downstream as a version that is known to be blank.
    #[test]
    fn os_release_without_a_usable_value_is_none() {
        assert_eq!(parse_os_release(""), None);
        assert_eq!(
            parse_os_release("ID=ubuntu\nHOME_URL=\"https://x\"\n"),
            None
        );
        assert_eq!(parse_os_release("PRETTY_NAME=\"\"\nID=x\n"), None);
        // A key with no `=` is not a definition.
        assert_eq!(parse_os_release("PRETTY_NAME\n"), None);
    }

    #[test]
    fn plist_reads_the_product_version() {
        let text = "<dict>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n\
                    \t<key>ProductVersion</key>\n\t<string>15.5</string>\n</dict>";
        assert_eq!(parse_system_version_plist(text), Some("15.5".into()));
    }

    #[test]
    fn plist_without_the_key_or_its_value_is_none() {
        assert_eq!(parse_system_version_plist(""), None);
        assert_eq!(
            parse_system_version_plist("<key>ProductName</key><string>macOS</string>"),
            None
        );
        // The key is present but its `<string>` is never opened or never closed.
        assert_eq!(
            parse_system_version_plist("<key>ProductVersion</key>\n"),
            None
        );
        assert_eq!(
            parse_system_version_plist("<key>ProductVersion</key><string>15.5"),
            None
        );
        assert_eq!(
            parse_system_version_plist("<key>ProductVersion</key><string> </string>"),
            None
        );
    }

    /// Both readable platforms are exercised from whichever host runs this, and
    /// each is routed to its own parser - a plist parsed as an os-release, or
    /// the reverse, yields nothing.
    #[test]
    fn version_for_routes_each_platform_to_its_own_format() {
        let linux = |path: &str| {
            (path == "/etc/os-release").then(|| "PRETTY_NAME=\"Fedora 41\"\n".to_string())
        };
        assert_eq!(version_for("linux", &linux), Some("Fedora 41".into()));

        let mac = |_: &str| Some("<key>ProductVersion</key><string>14.6.1</string>".to_string());
        assert_eq!(version_for("macos", &mac), Some("14.6.1".into()));

        // Crossed formats parse to nothing rather than to something wrong.
        assert_eq!(version_for("macos", &linux), None);
        assert_eq!(version_for("linux", &mac), None);
    }

    #[test]
    fn version_for_is_none_when_the_platform_or_the_file_has_no_answer() {
        let missing = |_: &str| None;
        // No path for the platform, so the reader is never consulted at all.
        assert_eq!(version_for("windows", &missing), None);
        // A platform that has a path, but a file that cannot be read.
        assert_eq!(version_for("linux", &missing), None);
    }

    /// The real lookup runs against the real host. There is nothing to assert
    /// about the value - a container may publish no release file at all - and
    /// the "never blank" rule is already proved against every input shape by
    /// the parser tests above.
    #[test]
    fn the_real_version_lookup_runs_on_this_host() {
        let _ = current_version();
        // And the reader it injects really does report an unreadable path as
        // no answer, rather than propagating the io error.
        assert_eq!(read_to_string("/definitely/not/a/file"), None);
    }

    /// The hostname is whatever this machine is called, and CI runners are
    /// called different things - so what is asserted is the invariant the
    /// callers rely on: an answer, when there is one, names something.
    #[test]
    fn the_hostname_is_absent_or_names_the_machine() {
        assert!(hostname().is_none_or(|h| !h.trim().is_empty()));
    }
}
