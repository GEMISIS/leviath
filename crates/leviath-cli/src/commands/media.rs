//! `lev media` - the media registry as this install sees it, and what a file
//! resolves to under it.
//!
//! The registry decides what every attached file *is*: its family, whether
//! its bytes are text, how many tokens it is budgeted at, and what a model
//! that cannot take it sees instead. Those answers come from two layers
//! (the compiled defaults, a `[media_types]` table in the config, then
//! `media_types.toml` beside it), and this is the place to see the result of
//! the layering before a run depends on it.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use leviath_core::media::{Blob, MediaInfo, MediaRegistry, MediaType, human_size};

/// Arguments for `lev media`.
#[derive(Args, Debug)]
pub struct MediaArgs {
    /// Which media subcommand to run.
    #[command(subcommand)]
    pub command: MediaCommand,
}

/// The `lev media` subcommands.
#[derive(Subcommand, Debug)]
pub enum MediaCommand {
    /// List the effective media registry, with where each row came from
    List(ListArgs),
    /// Say what a file resolves to: its type, family, token estimate, and
    /// what a model sees
    Check(CheckArgs),
    /// Write a commented example media_types.toml beside your config
    Init(InitArgs),
}

/// Arguments for `lev media init`.
#[derive(Args, Debug, Default)]
pub struct InitArgs {
    /// Overwrite a file that is already there.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `lev media list`.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// Print the table as JSON, one object per row.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `lev media check`.
#[derive(Args, Debug)]
pub struct CheckArgs {
    /// The file to resolve.
    pub file: PathBuf,
    /// Take the file as this type instead of sniffing one, as a sender
    /// declaring it would.
    #[arg(long = "type", value_name = "MEDIA_TYPE")]
    pub media_type: Option<String>,
    /// Print the answer as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Execute `lev media`.
pub(crate) async fn execute(args: MediaArgs) -> anyhow::Result<()> {
    // The registry as the daemon builds it, read only by the commands that
    // look at it: `init` writes the file the others would read.
    let registry = || -> anyhow::Result<MediaRegistry> {
        crate::config::Config::load()?
            .media_registry()
            .map_err(|e| anyhow::anyhow!("the media registry does not load: {e}"))
    };
    let out = match args.command {
        MediaCommand::List(list) => render_list(&registry()?, list.json),
        MediaCommand::Check(check) => render_check(
            &registry()?,
            &check.file,
            check.media_type.as_deref(),
            check.json,
        )?,
        MediaCommand::Init(init_args) => init(&crate::config::media_types_path(), init_args.force)?,
    };
    print!("{out}");
    Ok(())
}

/// Write the example, refusing to replace a file that is there unless told.
fn init(path: &Path, force: bool) -> anyhow::Result<String> {
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; edit it, or pass --force to replace it with the example",
            path.display()
        );
    }
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    std::fs::write(path, crate::config::MEDIA_TYPES_EXAMPLE)?;
    Ok(format!(
        "wrote {}\n  `lev media list` shows the table it makes; edit the rows and they reach the next run\n",
        path.display()
    ))
}

/// Every row of the registry, resolved: what each type is once the layers
/// have been applied.
fn rows(registry: &MediaRegistry) -> Vec<MediaInfo> {
    let mut keys = registry.keys();
    keys.sort();
    keys.into_iter()
        .filter_map(|(key, _)| MediaType::parse(&key).ok())
        .map(|t| registry.info(&t))
        .collect()
}

/// The registry as a table or as JSON.
fn render_list(registry: &MediaRegistry, json: bool) -> String {
    let rows = rows(registry);
    if json {
        return format!(
            "{}\n",
            serde_json::to_string_pretty(&rows).expect("registry rows always serialize")
        );
    }
    let type_width = rows
        .iter()
        .map(|r| r.media_type.as_str().len())
        .max()
        .unwrap_or(4)
        .max(4);
    let family_width = rows
        .iter()
        .map(|r| r.family.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let mut out = format!(
        "{:<type_width$}  {:<family_width$}  TEXT  EXTENSIONS            SOURCE\n",
        "TYPE", "FAMILY"
    );
    for row in &rows {
        out.push_str(&format!(
            "{:<type_width$}  {:<family_width$}  {:<4}  {:<20}  {}\n",
            row.media_type,
            row.family,
            match row.text {
                true => "yes",
                false => "no",
            },
            row.extensions.join(","),
            row.source,
        ));
    }
    out.push_str(&format!(
        "\n{} type{}. Rows layer: compiled defaults, [media_types] in the config, then \
         media_types.toml beside it.\n",
        rows.len(),
        match rows.len() {
            1 => "",
            _ => "s",
        }
    ));
    out
}

/// What a file resolves to, and how a model would see it.
fn render_check(
    registry: &MediaRegistry,
    file: &Path,
    declared: Option<&str>,
    json: bool,
) -> anyhow::Result<String> {
    let bytes = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("could not read {}: {e}", file.display()))?;
    let declared = declared
        .map(|t| MediaType::parse(t).map_err(|e| anyhow::anyhow!("--type {t}: {e}")))
        .transpose()?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let media_type = registry.resolve(declared.as_ref(), Some(&name), &bytes);
    let info = registry.info(&media_type);
    let reference = Blob::new(media_type.clone(), bytes)
        .named(&name)
        .describe(registry);
    let reaches = match info.text {
        true => "as text, to any model".to_string(),
        false => format!(
            "natively, to a model that lists {media_type} (`lev models list --accepts \
             {media_type}`); as its stand-in to any other"
        ),
    };
    if json {
        let value = serde_json::json!({
            "file": name,
            "media_type": media_type,
            "family": info.family,
            "text": info.text,
            "source": info.source,
            "size": reference.size,
            "width": reference.width,
            "height": reference.height,
            "duration_ms": reference.duration_ms,
            "tokens": reference.tokens,
            "stand_in": reference.stand_in,
            "reaches_a_model": reaches,
        });
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&value).expect("a check always serializes")
        ));
    }
    let mut out = format!("{name}\n");
    out.push_str(&format!("  type        {media_type} ({})\n", info.source));
    out.push_str(&format!("  family      {}\n", info.family));
    out.push_str(&format!(
        "  text        {}\n",
        match info.text {
            true => "yes",
            false => "no",
        }
    ));
    out.push_str(&format!("  size        {}\n", human_size(reference.size)));
    if let Some((w, h)) = reference.dims() {
        out.push_str(&format!("  dimensions  {w}x{h}\n"));
    }
    if let Some(ms) = reference.duration_ms {
        out.push_str(&format!("  duration    {:.1}s\n", ms as f64 / 1000.0));
    }
    out.push_str(&format!("  tokens      ~{}\n", reference.tokens));
    out.push_str(&format!("  stand-in    {}\n", reference.stand_in));
    out.push_str(&format!("  to a model  {reaches}\n"));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_names_every_row_with_its_source() {
        let mut registry = MediaRegistry::builtin();
        let table: toml::Table = toml::from_str(
            "[\"model/obj\"]\nextensions = [\"obj\"]\nfamily = \"model\"\ntext = true\n",
        )
        .unwrap();
        registry.layer(&table, "config").unwrap();
        let out = render_list(&registry, false);
        assert!(out.starts_with("TYPE"), "{out}");
        assert!(out.contains("image/png"), "{out}");
        assert!(out.contains("model/obj"), "{out}");
        assert!(out.contains("config"), "{out}");
        assert!(out.contains("Rows layer"), "{out}");
        let json = render_list(&registry, true);
        let rows: Vec<MediaInfo> = serde_json::from_str(&json).unwrap();
        let obj = rows
            .iter()
            .find(|r| r.media_type.as_str() == "model/obj")
            .expect("the layered row is listed");
        assert!(obj.text);
        assert_eq!(obj.source, "config");
    }

    #[test]
    fn a_singular_registry_reads_as_one_type() {
        let mut registry = MediaRegistry::empty();
        let table: toml::Table = toml::from_str("[\"x/y\"]\nfamily = \"odd\"\n").unwrap();
        registry.layer(&table, "test").unwrap();
        let out = render_list(&registry, false);
        assert!(out.contains("\n1 type. Rows layer"), "{out}");
    }

    #[test]
    fn a_check_says_what_a_file_is_and_how_a_model_sees_it() {
        let registry = MediaRegistry::builtin();
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("hero.png");
        // A PNG header with a 4x3 IHDR, so the probe finds dimensions.
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        std::fs::write(&png, &bytes).unwrap();
        let out = render_check(&registry, &png, None, false).unwrap();
        assert!(out.starts_with("hero.png\n"), "{out}");
        assert!(out.contains("type        image/png"), "{out}");
        assert!(out.contains("family      image"), "{out}");
        assert!(out.contains("text        no"), "{out}");
        assert!(out.contains("dimensions  4x3"), "{out}");
        assert!(out.contains("stand-in    [image/png 4x3"), "{out}");
        assert!(out.contains("--accepts image/png"), "{out}");

        // Text is text, to any model; a declared type wins over the sniff.
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, "# hi").unwrap();
        let out = render_check(&registry, &notes, None, false).unwrap();
        assert!(out.contains("type        text/markdown"), "{out}");
        assert!(out.contains("to a model  as text, to any model"), "{out}");
        let json = render_check(&registry, &notes, Some("text/plain"), true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["media_type"], "text/plain");
        assert_eq!(parsed["text"], true);
        assert_eq!(parsed["file"], "notes.md");

        // A WAV with a duration, declared: its RIFF header sniffs as WebP first.
        let mut wav = b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00".to_vec();
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&[2, 0, 16, 0]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&16000u32.to_le_bytes());
        let clip = dir.path().join("clip.wav");
        std::fs::write(&clip, &wav).unwrap();
        let out = render_check(&registry, &clip, Some("audio/wav"), false).unwrap();
        assert!(out.contains("type        audio/wav"), "{out}");
        assert!(out.contains("duration    1.0s"), "{out}");

        // A file that is not there, and a type that is not one.
        let err = render_check(&registry, &dir.path().join("gone"), None, false).unwrap_err();
        assert!(err.to_string().contains("could not read"), "{err}");
        let err = render_check(&registry, &notes, Some("nope"), false).unwrap_err();
        assert!(err.to_string().contains("--type nope"), "{err}");
    }

    #[tokio::test]
    async fn execute_reads_the_config_registry() {
        crate::config::with_isolated_config_path_async("media-execute", |_dir| async move {
            assert!(
                execute(MediaArgs {
                    command: MediaCommand::List(ListArgs { json: true }),
                })
                .await
                .is_ok()
            );
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("a.txt");
            std::fs::write(&file, "text").unwrap();
            assert!(
                execute(MediaArgs {
                    command: MediaCommand::Check(CheckArgs {
                        file,
                        media_type: None,
                        json: false,
                    }),
                })
                .await
                .is_ok()
            );
        })
        .await;
    }

    /// `lev media init` writes the example once, replaces it only when
    /// forced, and says where it went.
    #[tokio::test]
    async fn init_writes_the_example_beside_the_config() {
        crate::config::with_isolated_config_path_async("media-init", |dir| async move {
            let path = dir.join("media_types.toml");
            assert!(
                execute(MediaArgs {
                    command: MediaCommand::Init(InitArgs::default()),
                })
                .await
                .is_ok()
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                crate::config::MEDIA_TYPES_EXAMPLE
            );
            let err = init(&path, false).unwrap_err();
            assert!(err.to_string().contains("--force"), "{err}");
            assert!(
                execute(MediaArgs {
                    command: MediaCommand::Init(InitArgs::default()),
                })
                .await
                .is_err()
            );
            std::fs::write(&path, "[\"x/y\"]\nfamily = \"custom\"\n").unwrap();
            init(&path, true).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                crate::config::MEDIA_TYPES_EXAMPLE
            );
            // The rows the file adds show up in the listing, with their source.
            std::fs::write(&path, "[\"x/y\"]\nfamily = \"custom\"\n").unwrap();
            let registry = crate::config::Config::load()
                .unwrap()
                .media_registry()
                .unwrap();
            let out = render_list(&registry, false);
            assert!(out.contains("media_types.toml"), "{out}");
            // A parent that is a file cannot be created; a path that is a
            // directory cannot be written.
            let blocker = dir.join("blocker");
            std::fs::write(&blocker, "").unwrap();
            assert!(init(&blocker.join("media_types.toml"), false).is_err());
            let taken = dir.join("taken");
            std::fs::create_dir_all(&taken).unwrap();
            assert!(init(&taken, true).is_err());
        })
        .await;
    }

    /// A `[media_types]` table that does not layer, and a config that does
    /// not parse at all, are each refused with a reason rather than run on
    /// the defaults.
    #[tokio::test]
    async fn execute_refuses_a_config_whose_registry_does_not_load() {
        crate::config::with_isolated_config_path_async("media-bad-config", |dir| async move {
            let config = dir.join("config.toml");
            std::fs::write(
                &config,
                "[media_types.\"image/png\"]\ntokens = { per_byte = 0.5, fixed = 3 }\n",
            )
            .unwrap();
            let err = execute(MediaArgs {
                command: MediaCommand::List(ListArgs { json: false }),
            })
            .await
            .unwrap_err();
            assert!(err.to_string().contains("[media_types]"), "{err}");
            assert!(
                execute(MediaArgs {
                    command: MediaCommand::Check(CheckArgs {
                        file: dir.join("config.toml"),
                        media_type: None,
                        json: false,
                    }),
                })
                .await
                .is_err()
            );
            std::fs::write(&config, "this is not toml = = =\n").unwrap();
            assert!(
                execute(MediaArgs {
                    command: MediaCommand::List(ListArgs { json: false }),
                })
                .await
                .is_err()
            );
        })
        .await;
    }
}
