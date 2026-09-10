//! `lev blobs <run-id>` - the files a run holds as stored parts, and their bytes.
//!
//! A run's images, recordings and other non-text parts live in its blob
//! store, named in the context by hash. `lev context` shows where each sits;
//! this lists them in one place and hands one out: to stdout for a pipeline,
//! to a path, or to the program the operating system opens it with.
//! Read-only and daemon-free, like `lev result`.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;
use leviath_core::mime::{MimeRegistry, human_size};

use super::result::export::{self, Opener};
use crate::blobs::BlobEntry;

/// Arguments for `lev blobs`.
#[derive(Args, Debug)]
pub struct BlobsArgs {
    /// The run whose stored parts to show.
    pub run_id: String,

    /// One part, by its name or by a prefix of its sha256 (six characters or
    /// more). Its bytes go to stdout unless `--out` or `--open` says where.
    pub part: Option<String>,

    /// Write the part to this path. A directory takes it under its own name.
    #[arg(long, value_name = "PATH", requires = "part")]
    pub out: Option<PathBuf>,

    /// Hand the part to the operating system to open.
    #[arg(long, requires = "part", conflicts_with = "out")]
    pub open: bool,

    /// Print the listing as JSON.
    #[arg(long, conflicts_with = "part")]
    pub json: bool,
}

/// Execute `lev blobs`.
pub(crate) async fn execute(args: BlobsArgs) -> anyhow::Result<()> {
    let entries = crate::blobs::list(&args.run_id).ok_or_else(|| {
        anyhow::anyhow!(
            "no context for run '{}' (no readable context.json)",
            args.run_id
        )
    })?;
    match &args.part {
        None => {
            print!("{}", render(&args.run_id, &entries, args.json));
            Ok(())
        }
        Some(needle) => {
            let mut stdout = std::io::stdout().lock();
            let lines = fetch(
                &args.run_id,
                &entries,
                needle,
                args.out.as_deref(),
                args.open,
                &leviath_sys::open_url,
                &mut stdout,
            )?;
            for line in lines {
                println!("{line}");
            }
            Ok(())
        }
    }
}

/// The listing, as a table or as JSON. Pure, so the layout is testable.
fn render(run_id: &str, entries: &[BlobEntry], json: bool) -> String {
    if json {
        return format!(
            "{}\n",
            serde_json::to_string_pretty(entries).expect("a blob listing always serializes")
        );
    }
    if entries.is_empty() {
        return format!("Run '{run_id}' holds no stored parts.\n");
    }
    let registry = MimeRegistry::builtin();
    let name_width = entries
        .iter()
        .map(|e| e.file_name(&registry).len())
        .max()
        .unwrap_or(0);
    let type_width = entries.iter().map(|e| e.mime_type.len()).max().unwrap_or(0);
    let mut out = format!("Stored parts of run '{run_id}' ({}):\n", entries.len());
    for entry in entries {
        let shape = entry.shape();
        let missing = match entry.stored {
            true => "",
            false => "  (bytes missing from the store)",
        };
        out.push_str(&format!(
            "  {:<name_width$}  {:<type_width$}  {:>8}  {:>9}  {:>6} tok  sha256:{}  in: {}{missing}\n",
            entry.file_name(&registry),
            entry.mime_type,
            human_size(entry.size),
            shape,
            entry.tokens,
            entry.sha256.chars().take(12).collect::<String>(),
            entry.regions.join(", "),
        ));
    }
    out
}

/// Hand one part out: to `stdout`, to a path, or to the OS. The lines to
/// print afterwards; none when the bytes went to `stdout`.
fn fetch(
    run_id: &str,
    entries: &[BlobEntry],
    needle: &str,
    out: Option<&Path>,
    open: bool,
    opener: Opener<'_>,
    stdout: &mut dyn Write,
) -> anyhow::Result<Vec<String>> {
    let entry = crate::blobs::find(entries, needle).ok_or_else(|| {
        anyhow::anyhow!(
            "run '{run_id}' holds no part named '{needle}' (a name, or six or more characters \
             of a sha256, of exactly one part); `lev blobs {run_id}` lists them"
        )
    })?;
    let registry = MimeRegistry::builtin();
    let file_name = entry.file_name(&registry);
    let bytes = crate::blobs::read(run_id, &entry.sha256).map_err(|e| {
        anyhow::anyhow!("the bytes of '{file_name}' are not in the run's store: {e}")
    })?;
    let label = |path: &Path| {
        format!(
            "{}  {}  {}",
            path.display(),
            entry.mime_type,
            human_size(bytes.len() as u64)
        )
    };
    if open {
        let path = export::export_and_open(run_id, &file_name, &bytes, opener)?;
        return Ok(vec![format!("opened {}", label(&path))]);
    }
    // A directory takes the part under its own name; any other path is the
    // file to write, created under its parent.
    let target = out.map(|path| match path.is_dir() {
        true => (path, file_name.clone()),
        false => (
            path.parent().unwrap_or(Path::new(".")),
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(file_name.clone()),
        ),
    });
    match target {
        Some((dir, name)) => {
            let written = export::write_into(dir, &name, &bytes)?;
            Ok(vec![label(&written)])
        }
        None => {
            stdout.write_all(&bytes)?;
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runstate;
    use leviath_core::mime::{Blob, BlobStore, MimeType, Part};
    use leviath_core::region::EntryContent;
    use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

    fn no_open(_url: &str) -> bool {
        false
    }

    fn yes_open(_url: &str) -> bool {
        true
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

    fn entry(name: Option<&str>, stored: bool) -> BlobEntry {
        BlobEntry {
            sha256: "0123456789abcdef".repeat(4),
            mime_type: "image/png".to_string(),
            name: name.map(str::to_string),
            size: 240 * 1024,
            width: Some(1024),
            height: Some(768),
            duration_ms: None,
            tokens: 1092,
            regions: vec!["task".to_string(), "art".to_string()],
            stored,
        }
    }

    /// A run whose context names one stored PNG and one lost WAV.
    fn seed(run_id: &str) -> String {
        runstate::create_run(&crate::test_support::fixtures::run_meta(run_id)).unwrap();
        let registry = MimeRegistry::builtin();
        let store = leviath_runtime::blob_store::FsBlobStore::new(runstate::runs_dir());
        let png = Blob::new(
            MimeType::parse("image/png").unwrap(),
            b"\x89PNG\r\n\x1a\nhero".to_vec(),
        )
        .named("hero.png");
        let stored = Part::stored(store.put(run_id, &png, &registry).unwrap()).named("hero.png");
        let sha = stored.blob().unwrap().sha256.clone();
        let snapshot = ContextSnapshot {
            stage_name: "s".to_string(),
            total_tokens: 1,
            max_tokens: 100,
            regions: vec![RegionSnapshot {
                name: "task".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 1,
                max_tokens: 100,
                entries: vec![RegionEntrySnapshot {
                    content: EntryContent::from_parts(vec![Part::text("see"), stored]),
                    tokens: 1,
                    kind: Default::default(),
                    metadata: None,
                    key: None,
                    taint: leviath_core::taint::TaintLevel::Public,
                    reasoning: None,
                }],
                description: None,
            }],
        };
        runstate::write_context_snapshot(run_id, &snapshot).unwrap();
        sha
    }

    #[test]
    fn the_listing_names_types_sizes_and_where_each_part_sits() {
        let entries = vec![entry(Some("hero.png"), true), entry(None, false)];
        let out = render("run-1", &entries, false);
        assert!(
            out.starts_with("Stored parts of run 'run-1' (2):\n"),
            "{out}"
        );
        assert!(out.contains("hero.png"), "{out}");
        assert!(out.contains("image/png"), "{out}");
        assert!(out.contains("240 KB"), "{out}");
        assert!(out.contains("1024x768"), "{out}");
        assert!(out.contains("1092 tok"), "{out}");
        assert!(out.contains("sha256:0123456789ab"), "{out}");
        assert!(out.contains("in: task, art"), "{out}");
        // The unnamed one exports as its hash with an extension, and says its
        // bytes are gone.
        assert!(out.contains("0123456789ab.png"), "{out}");
        assert_eq!(out.matches("bytes missing").count(), 1, "{out}");

        assert_eq!(
            render("run-1", &[], false),
            "Run 'run-1' holds no stored parts.\n"
        );
        let json = render("run-1", &entries, true);
        let parsed: Vec<BlobEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, entries);
    }

    #[test]
    fn a_part_is_streamed_written_or_opened() {
        runstate::with_isolated_runs_dir("blobs-fetch", |_d| {
            let run_id = "blobs-run";
            let sha = seed(run_id);
            let entries = crate::blobs::list(run_id).unwrap();
            assert_eq!(entries.len(), 1);

            // To stdout, by name and by hash prefix.
            let mut sink = Vec::new();
            let lines = fetch(
                run_id, &entries, "hero.png", None, false, &no_open, &mut sink,
            )
            .unwrap();
            assert!(lines.is_empty());
            assert_eq!(sink, b"\x89PNG\r\n\x1a\nhero");
            let prefix: String = sha.chars().take(8).collect();
            assert!(fetch(run_id, &entries, &prefix, None, false, &no_open, &mut sink).is_ok());

            // Into a directory, then to an explicit file path.
            let dest = tempfile::tempdir().unwrap();
            let lines = fetch(
                run_id,
                &entries,
                "hero.png",
                Some(dest.path()),
                false,
                &no_open,
                &mut sink,
            )
            .unwrap();
            let first = &lines[0];
            assert!(
                first.contains("hero.png") && first.contains("image/png"),
                "{first}"
            );
            assert!(dest.path().join("hero.png").is_file());
            let explicit = dest.path().join("sub").join("copy.png");
            let lines = fetch(
                run_id,
                &entries,
                "hero.png",
                Some(&explicit),
                false,
                &no_open,
                &mut sink,
            )
            .unwrap();
            let first = &lines[0];
            assert!(first.contains("copy.png"), "{first}");
            assert!(explicit.is_file());
            // A path whose parent is a file cannot be created, and a closed
            // stdout cannot be written.
            let under_file = explicit.join("deeper.png");
            let err = fetch(
                run_id,
                &entries,
                "hero.png",
                Some(&under_file),
                false,
                &no_open,
                &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("could not create"), "{err}");
            let err = fetch(
                run_id,
                &entries,
                "hero.png",
                None,
                false,
                &no_open,
                &mut Broken,
            )
            .unwrap_err();
            assert!(err.to_string().contains("closed"), "{err}");
            assert!(Broken.flush().is_ok());

            // Opened: exported under the temp dir first.
            let lines = fetch(
                run_id, &entries, "hero.png", None, true, &yes_open, &mut sink,
            )
            .unwrap();
            let first = &lines[0];
            assert!(first.starts_with("opened "), "{first}");
            assert!(export::export_dir(run_id).join("hero.png").is_file());
            let _ = std::fs::remove_dir_all(export::export_dir(run_id));
            let err = fetch(
                run_id, &entries, "hero.png", None, true, &no_open, &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("could not open"), "{err}");

            // A part the run does not hold, and one whose bytes are gone.
            let err = fetch(
                run_id, &entries, "nope.png", None, false, &no_open, &mut sink,
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("holds no part named 'nope.png'"),
                "{err}"
            );
            std::fs::remove_file(crate::blobs::blob_path(run_id, &sha)).unwrap();
            let err = fetch(
                run_id, &entries, "hero.png", None, false, &no_open, &mut sink,
            )
            .unwrap_err();
            assert!(err.to_string().contains("not in the run's store"), "{err}");
        });
    }

    #[test]
    fn execute_lists_and_fetches_and_reports_a_missing_run() {
        runstate::with_isolated_runs_dir("blobs-execute", |_d| {
            let run_id = "blobs-exec";
            seed(run_id);
            let rt = tokio::runtime::Runtime::new().unwrap();
            let listing = BlobsArgs {
                run_id: run_id.to_string(),
                part: None,
                out: None,
                open: false,
                json: true,
            };
            assert!(rt.block_on(execute(listing)).is_ok());
            let dest = tempfile::tempdir().unwrap();
            let one = BlobsArgs {
                run_id: run_id.to_string(),
                part: Some("hero.png".to_string()),
                out: Some(dest.path().to_path_buf()),
                open: false,
                json: false,
            };
            assert!(rt.block_on(execute(one)).is_ok());
            assert!(dest.path().join("hero.png").is_file());
            let missing = BlobsArgs {
                run_id: "no-such-run".to_string(),
                part: None,
                out: None,
                open: false,
                json: false,
            };
            let err = rt.block_on(execute(missing)).unwrap_err();
            assert!(err.to_string().contains("no context for run"), "{err}");
            let no_part = BlobsArgs {
                run_id: run_id.to_string(),
                part: Some("nope.png".to_string()),
                out: None,
                open: false,
                json: false,
            };
            let err = rt.block_on(execute(no_part)).unwrap_err();
            assert!(err.to_string().contains("holds no part"), "{err}");
        });
    }
}
