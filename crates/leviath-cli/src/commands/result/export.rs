//! Handing a run's files to the user: to stdout, into a directory, or to
//! whatever the operating system opens that kind of file with.
//!
//! `lev result`, `lev blobs` and the dashboard all end here, so the rules
//! live in one place: a name is reduced to a file name before it touches a
//! path, an artifact's bytes come from the run's blob store when it holds
//! them and from the workdir otherwise, and "open" means the system's own
//! opener. Nothing in `lev` plays or draws a file.

use std::io::Write;
use std::path::{Path, PathBuf};

use leviath_core::media::human_size;
use leviath_core::output::{Artifact, FinalOutput};

/// What hands a URL to the operating system: `leviath_sys::open_url` in the
/// binary, a recording stub in tests.
pub(crate) type Opener = fn(&str) -> bool;

/// What `lev result` was asked to do with the run's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileRequest<'a> {
    /// Hand the named artifact to the OS.
    Open(&'a str),
    /// Write every artifact, or the one named, into a directory.
    Into {
        /// Where the files go.
        dir: &'a Path,
        /// Only this artifact, when given.
        only: Option<&'a str>,
    },
    /// Write the named artifact's bytes to stdout.
    Stdout(&'a str),
}

impl<'a> FileRequest<'a> {
    /// The request the flags spell, or `None` when none of them was given.
    /// `--open` wins over the others; clap already refuses the overlaps.
    pub(crate) fn from_flags(
        artifact: Option<&'a str>,
        out: Option<&'a Path>,
        open: Option<&'a str>,
    ) -> Option<Self> {
        if let Some(name) = open {
            return Some(Self::Open(name));
        }
        if let Some(dir) = out {
            return Some(Self::Into {
                dir,
                only: artifact,
            });
        }
        artifact.map(Self::Stdout)
    }
}

/// The last path component of `name`, with anything a file name cannot be
/// replaced, so a caller can join it under a directory it chose.
pub(crate) fn safe_file_name(name: &str) -> String {
    let last = name
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let safe: String = last
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                true => c,
                false => '_',
            },
        )
        .collect();
    let trimmed = safe.trim_matches(|c| c == '.' || c == ' ');
    match trimmed.is_empty() {
        true => "part".to_string(),
        false => trimmed.to_string(),
    }
}

/// An artifact's bytes: the run's blob store when it holds them (the hash
/// the answer recorded), else the file in the workdir.
pub(crate) fn artifact_bytes(
    run_id: &str,
    workdir: &str,
    artifact: &Artifact,
) -> anyhow::Result<Vec<u8>> {
    if !artifact.sha256.is_empty()
        && let Ok(bytes) = crate::blobs::read(run_id, &artifact.sha256)
    {
        return Ok(bytes);
    }
    let root = Path::new(workdir);
    let path = root.join(&artifact.path);
    if !leviath_core::resolves_within(&path, root) {
        anyhow::bail!(
            "artifact '{}' points outside the run's working directory ({})",
            artifact.name,
            artifact.path
        );
    }
    std::fs::read(&path).map_err(|e| {
        anyhow::anyhow!(
            "artifact '{}' could not be read from {}: {e}",
            artifact.name,
            path.display()
        )
    })
}

/// Write `bytes` as `file_name` under `dir`, creating the directory.
pub(crate) fn write_into(dir: &Path, file_name: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("could not create {}: {e}", dir.display()))?;
    let path = dir.join(safe_file_name(file_name));
    std::fs::write(&path, bytes)
        .map_err(|e| anyhow::anyhow!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

/// Where a file goes before it is opened: a per-run directory under the
/// system temp dir, so it has a name and an extension the opener can type
/// it by, which a bare hash in the blob store has not.
pub(crate) fn export_dir(run_id: &str) -> PathBuf {
    std::env::temp_dir()
        .join("leviath")
        .join("exports")
        .join(run_id)
}

/// A `file://` URL for `path`, made absolute against the current directory
/// and percent-encoded the way a browser or `open` expects.
pub(crate) fn file_url(path: &Path) -> String {
    let absolute = match path.is_absolute() {
        true => path.to_path_buf(),
        false => std::env::current_dir().unwrap_or_default().join(path),
    };
    format!("file://{}", url_path(&absolute.to_string_lossy()))
}

/// An absolute path as the path of a `file://` URL: forward slashes, a
/// leading slash in front of a Windows drive, and percent-encoding for
/// anything a URL cannot carry bare.
fn url_path(path: &str) -> String {
    let mut text = path.replace('\\', "/");
    if !text.starts_with('/') {
        // A Windows drive: `C:/x` becomes `/C:/x`.
        text.insert(0, '/');
    }
    text.bytes()
        .map(
            |b| match b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
                true => (b as char).to_string(),
                false => format!("%{b:02X}"),
            },
        )
        .collect()
}

/// Write `bytes` under the run's export directory as `file_name` and hand
/// the file to the operating system. The path it was written to.
pub(crate) fn export_and_open(
    run_id: &str,
    file_name: &str,
    bytes: &[u8],
    opener: Opener,
) -> anyhow::Result<PathBuf> {
    let path = write_into(&export_dir(run_id), file_name, bytes)?;
    open_file(&path, opener)?;
    Ok(path)
}

/// Hand `path` to the operating system to open with whatever it associates
/// with that kind of file.
pub(crate) fn open_file(path: &Path, opener: Opener) -> anyhow::Result<()> {
    match opener(&file_url(path)) {
        true => Ok(()),
        false => anyhow::bail!(
            "the system could not open {}; the file is there to open by hand",
            path.display()
        ),
    }
}

/// Deliver the files a run produced, as `lev result` was asked. The lines
/// to print afterwards; none when the bytes themselves went to `stdout`.
pub(crate) fn deliver(
    run_id: &str,
    workdir: &str,
    output: &FinalOutput,
    request: FileRequest<'_>,
    opener: Opener,
    stdout: &mut dyn Write,
) -> anyhow::Result<Vec<String>> {
    if output.artifacts.is_empty() {
        anyhow::bail!("run '{run_id}' produced no files; its answer is `lev result {run_id}`");
    }
    // The named artifact and its bytes, or why not: a name the run never
    // produced, or a file that cannot be read.
    let fetch = |name: &str| -> anyhow::Result<(&Artifact, Vec<u8>)> {
        let artifact = output
            .artifacts
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| {
                let names: Vec<&str> = output.artifacts.iter().map(|a| a.name.as_str()).collect();
                anyhow::anyhow!(
                    "run '{run_id}' produced no file named '{name}'; it has: {}",
                    names.join(", ")
                )
            })?;
        Ok((artifact, artifact_bytes(run_id, workdir, artifact)?))
    };
    let label = |artifact: &Artifact, path: &Path, len: usize| {
        format!(
            "{}  {}  {}",
            path.display(),
            artifact.media_type,
            human_size(len as u64)
        )
    };
    match request {
        FileRequest::Open(name) => {
            let (artifact, bytes) = fetch(name)?;
            let path = export_and_open(run_id, &artifact.path, &bytes, opener)?;
            Ok(vec![format!(
                "opened {}",
                label(artifact, &path, bytes.len())
            )])
        }
        FileRequest::Into { dir, only } => {
            let named: Vec<&str> = match only {
                Some(name) => vec![name],
                None => output.artifacts.iter().map(|a| a.name.as_str()).collect(),
            };
            let mut lines = Vec::with_capacity(named.len());
            for name in named {
                let (artifact, bytes) = fetch(name)?;
                let path = write_into(dir, &artifact.path, &bytes)?;
                lines.push(label(artifact, &path, bytes.len()));
            }
            Ok(lines)
        }
        FileRequest::Stdout(name) => {
            let (_, bytes) = fetch(name)?;
            stdout.write_all(&bytes)?;
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::media::MediaType;
    use std::sync::Mutex;

    /// The last URL an opener was handed, for the tests that inject one.
    static OPENED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn record_open(url: &str) -> bool {
        OPENED.lock().unwrap().push(url.to_string());
        true
    }

    fn refuse_open(_url: &str) -> bool {
        false
    }

    fn artifact(name: &str, path: &str, sha256: &str) -> Artifact {
        Artifact {
            name: name.to_string(),
            path: path.to_string(),
            media_type: MediaType::parse("image/png").unwrap(),
            size: 3,
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn a_file_name_is_the_last_component_made_safe() {
        assert_eq!(safe_file_name("out/hero.png"), "hero.png");
        assert_eq!(safe_file_name("a\\b\\c d.wav"), "c d.wav");
        assert_eq!(safe_file_name("we ird/na\"me.png"), "na_me.png");
        assert_eq!(safe_file_name("..."), "part");
        assert_eq!(safe_file_name(""), "part");
        assert_eq!(safe_file_name("/"), "part");
    }

    #[test]
    fn the_flags_spell_one_request() {
        let dir = Path::new("/tmp/x");
        assert_eq!(FileRequest::from_flags(None, None, None), None);
        assert_eq!(
            FileRequest::from_flags(Some("a"), None, None),
            Some(FileRequest::Stdout("a"))
        );
        assert_eq!(
            FileRequest::from_flags(Some("a"), Some(dir), None),
            Some(FileRequest::Into {
                dir,
                only: Some("a")
            })
        );
        assert_eq!(
            FileRequest::from_flags(None, Some(dir), None),
            Some(FileRequest::Into { dir, only: None })
        );
        assert_eq!(
            FileRequest::from_flags(Some("a"), Some(dir), Some("b")),
            Some(FileRequest::Open("b"))
        );
    }

    #[test]
    fn a_file_url_is_absolute_and_encoded() {
        let url = file_url(Path::new("/tmp/my dir/a%b.png"));
        assert_eq!(url, "file:///tmp/my%20dir/a%25b.png");
        let relative = file_url(Path::new("rel.png"));
        assert!(relative.starts_with("file:///"), "{relative}");
        assert!(relative.ends_with("/rel.png"), "{relative}");
        // A Windows path gets forward slashes and a leading slash.
        assert_eq!(url_path("C:\\x\\y z.png"), "/C:/x/y%20z.png");
    }

    /// A writer that refuses everything, for the arm that streams bytes out.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn opening_reports_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        std::fs::write(&path, b"x").unwrap();
        assert!(open_file(&path, record_open).is_ok());
        assert!(OPENED.lock().unwrap().iter().any(|u| u.ends_with("/a.png")));
        let err = open_file(&path, refuse_open).unwrap_err();
        assert!(err.to_string().contains("could not open"), "{err}");
    }

    #[test]
    fn artifact_bytes_come_from_the_store_then_the_workdir() {
        crate::runstate::with_isolated_runs_dir("export-artifact-bytes", |_d| {
            use leviath_core::media::{Blob, BlobStore, MediaRegistry};
            let run_id = "export-run";
            crate::runstate::create_run(&crate::test_support::fixtures::run_meta(run_id)).unwrap();
            let workdir = tempfile::tempdir().unwrap();
            std::fs::write(workdir.path().join("hero.png"), b"from workdir").unwrap();
            let wd = workdir.path().to_string_lossy().to_string();

            let store = leviath_runtime::blob_store::FsBlobStore::new(crate::runstate::runs_dir());
            let blob = Blob::new(
                MediaType::parse("image/png").unwrap(),
                b"from store".to_vec(),
            );
            let sha = store
                .put(run_id, &blob, &MediaRegistry::builtin())
                .unwrap()
                .sha256;

            // The store wins when it has the hash; a hash it lacks falls back
            // to the file; no hash reads the file; a missing file is an error;
            // a path outside the workdir is refused.
            let stored = artifact("hero", "hero.png", &sha);
            assert_eq!(artifact_bytes(run_id, &wd, &stored).unwrap(), b"from store");
            let lost = artifact("hero", "hero.png", &"e".repeat(64));
            assert_eq!(artifact_bytes(run_id, &wd, &lost).unwrap(), b"from workdir");
            let bare = artifact("hero", "hero.png", "");
            assert_eq!(artifact_bytes(run_id, &wd, &bare).unwrap(), b"from workdir");
            let missing = artifact("gone", "gone.png", "");
            assert!(
                artifact_bytes(run_id, &wd, &missing)
                    .unwrap_err()
                    .to_string()
                    .contains("could not be read")
            );
            let outside = artifact("up", "../outside.png", "");
            assert!(
                artifact_bytes(run_id, &wd, &outside)
                    .unwrap_err()
                    .to_string()
                    .contains("outside")
            );
        });
    }

    #[test]
    fn deliver_writes_opens_or_streams_the_files() {
        crate::runstate::with_isolated_runs_dir("export-deliver", |_d| {
            let run_id = "deliver-run";
            crate::runstate::create_run(&crate::test_support::fixtures::run_meta(run_id)).unwrap();
            let workdir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(workdir.path().join("out")).unwrap();
            std::fs::write(workdir.path().join("out/final.png"), b"png!").unwrap();
            std::fs::write(workdir.path().join("notes.md"), b"# n").unwrap();
            let wd = workdir.path().to_string_lossy().to_string();
            let output = FinalOutput::new("done", None, "s".to_string(), 1).with_artifacts(vec![
                artifact("final", "out/final.png", ""),
                artifact("notes", "notes.md", ""),
            ]);

            // To stdout: the bytes and nothing else.
            let mut sink = Vec::new();
            let lines = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Stdout("final"),
                refuse_open,
                &mut sink,
            )
            .unwrap();
            assert!(lines.is_empty());
            assert_eq!(sink, b"png!");

            // Into a directory: every file, or one.
            let dest = tempfile::tempdir().unwrap();
            let lines = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Into {
                    dir: dest.path(),
                    only: None,
                },
                refuse_open,
                &mut sink,
            )
            .unwrap();
            assert_eq!(lines.len(), 2);
            assert!(lines[0].contains("final.png") && lines[0].contains("image/png"));
            assert_eq!(
                std::fs::read(dest.path().join("final.png")).unwrap(),
                b"png!"
            );
            assert!(dest.path().join("notes.md").is_file());
            let one = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Into {
                    dir: &dest.path().join("one"),
                    only: Some("notes"),
                },
                refuse_open,
                &mut sink,
            )
            .unwrap();
            assert_eq!(one.len(), 1);

            // Open: exported under the temp dir, then handed to the opener.
            let lines = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Open("final"),
                record_open,
                &mut sink,
            )
            .unwrap();
            let first = &lines[0];
            assert!(first.starts_with("opened "), "{first}");
            assert!(export_dir(run_id).join("final.png").is_file());
            let _ = std::fs::remove_dir_all(export_dir(run_id));
            // An opener that refuses, a stdout that is closed, a directory
            // that cannot take the file, and a file the workdir lost.
            let err = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Open("final"),
                refuse_open,
                &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("could not open"), "{err}");
            let _ = std::fs::remove_dir_all(export_dir(run_id));
            // A name the run never produced, on the open path; and an export
            // directory already holding a directory under the file's name.
            let err = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Open("nope"),
                refuse_open,
                &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("no file named 'nope'"), "{err}");
            std::fs::create_dir_all(export_dir(run_id).join("final.png")).unwrap();
            let err = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Open("final"),
                record_open,
                &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("could not write"), "{err}");
            let _ = std::fs::remove_dir_all(export_dir(run_id));
            let err = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Stdout("final"),
                refuse_open,
                &mut Broken,
            )
            .unwrap_err();
            assert!(err.to_string().contains("closed"), "{err}");
            assert!(Broken.flush().is_ok());
            let blocked = dest.path().join("blocked");
            std::fs::write(&blocked, b"a file, not a directory").unwrap();
            let err = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Into {
                    dir: &blocked,
                    only: Some("final"),
                },
                refuse_open,
                &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("could not create"), "{err}");
            std::fs::remove_file(workdir.path().join("notes.md")).unwrap();
            let err = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Into {
                    dir: dest.path(),
                    only: None,
                },
                refuse_open,
                &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("could not be read"), "{err}");

            // A name the run never produced, and a run with no files.
            let err = deliver(
                run_id,
                &wd,
                &output,
                FileRequest::Stdout("nope"),
                refuse_open,
                &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("it has: final, notes"), "{err}");
            let none = FinalOutput::new("done", None, "s".to_string(), 1);
            let err = deliver(
                run_id,
                &wd,
                &none,
                FileRequest::Stdout("final"),
                refuse_open,
                &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("produced no files"), "{err}");
        });
    }

    #[test]
    fn writing_into_an_impossible_directory_is_an_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        // A directory that is a file cannot be created.
        let err = write_into(file.path(), "a.png", b"x").unwrap_err();
        assert!(err.to_string().contains("could not create"), "{err}");
        // A file name that is a directory cannot be written.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("taken")).unwrap();
        let err = write_into(dir.path(), "taken", b"x").unwrap_err();
        assert!(err.to_string().contains("could not write"), "{err}");
    }
}
