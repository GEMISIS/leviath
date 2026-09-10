//! `lev mime` - the mime registry as this install sees it, what a file
//! resolves to under it, and the rows in `mime_types.toml`.
//!
//! The registry decides what every attached file *is*: its family, whether
//! its bytes are text, how many tokens it is budgeted at, what a model that
//! cannot take it sees instead, and which check its bytes must pass. Those
//! answers come from layers (the compiled defaults, a `[mime_types]` table
//! in the config, `mime_types.toml` beside it, then a blueprint's own rows
//! for its runs), and this is the place to see the result of the layering
//! before a run depends on it, and to add a row without opening an editor.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use leviath_core::mime::{Blob, MimeInfo, MimeRegistry, MimeType, human_size};

use super::mime_rows::{Added, RowEdit, TokenSpec, add_row, remove_row};

/// Arguments for `lev mime`.
#[derive(Args, Debug)]
pub struct MimeArgs {
    /// Which mime subcommand to run.
    #[command(subcommand)]
    pub command: MimeCommand,
}

/// The `lev mime` subcommands.
///
/// The registry is layered: the compiled defaults, `[mime_types]` in the
/// config, `mime_types.toml` beside it, then a blueprint's own rows for its
/// runs. `list`, `show` and `check` read the result; `init`, `add` and
/// `remove` write the file beside the config. An edit reaches the next run
/// at once and every run already under way within the daemon's housekeeping
/// interval.
#[derive(Subcommand, Debug)]
pub enum MimeCommand {
    /// List the effective mime registry, with where each row came from
    List(ListArgs),
    /// Show one type as the registry resolves it: every field, its source,
    /// and the check its bytes must pass
    Show(ShowArgs),
    /// Say what a file resolves to: its type, family, token estimate, what
    /// a model sees, and whether its bytes pass the type's check
    Check(CheckArgs),
    /// Write a commented example mime_types.toml beside your config
    ///
    /// Optional: `lev mime add` writes a row without it, and the registry
    /// works with no file at all. The example is a starting point to edit by
    /// hand, with every field commented, and `lev mime list` shows the
    /// table it makes.
    Init(InitArgs),
    /// Add a row to mime_types.toml, or set fields on one that is there
    ///
    /// Name only what the row changes; every other field resolves from the
    /// built-in table. The file is checked before it is written, so a flag
    /// that would leave it unloadable is refused with the reason.
    ///
    /// Examples:
    ///   lev mime add model/obj --text --extensions obj
    ///   lev mime add application/x-acme-scene --family model \
    ///       --extensions scene --magic 41434D45 --check checks/scene.rhai
    ///   lev mime add image/gif --no-check      # lift a check image/* put on it
    #[command(verbatim_doc_comment)]
    Add(AddArgs),
    /// Take a row out of mime_types.toml
    Remove(RemoveArgs),
}

/// Arguments for `lev mime init`.
#[derive(Args, Debug, Default)]
pub struct InitArgs {
    /// Overwrite a file that is already there.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `lev mime show`.
#[derive(Args, Debug)]
pub struct ShowArgs {
    /// The type to show, `type/subtype`.
    pub mime_type: String,
    /// Print the answer as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `lev mime add`.
#[derive(Args, Debug, Default)]
pub struct AddArgs {
    /// The type the row is for: `type/subtype`, or `type/*` for a whole family.
    pub mime_type: String,
    /// What providers key their encoders on: text, image, audio, video,
    /// document, model, binary, or a name of your own.
    #[arg(long)]
    pub family: Option<String>,
    /// The bytes are UTF-8 and may reach any text model as text.
    #[arg(long, conflicts_with = "binary")]
    pub text: bool,
    /// The bytes are not text (undoes a `text = true`).
    #[arg(long)]
    pub binary: bool,
    /// How to estimate tokens: `per_byte=0.25`, `per_pixel=750,max=1600`,
    /// `per_second=32` or `fixed=1000`.
    #[arg(long, value_name = "RULE")]
    pub tokens: Option<String>,
    /// File extensions that imply the type, comma-separated, without dots.
    #[arg(long, value_name = "EXT,EXT", value_delimiter = ',')]
    pub extensions: Option<Vec<String>>,
    /// A hex prefix that identifies the bytes.
    #[arg(long, value_name = "HEX")]
    pub magic: Option<String>,
    /// What a consumer that cannot take the type sees; `{type}`, `{name}`,
    /// `{size}`, `{dims}` and `{duration}` are filled in.
    #[arg(long, value_name = "TEMPLATE")]
    pub stand_in: Option<String>,
    /// A Rhai script, relative to the config's directory, whose
    /// `check(bytes, mime_type)` refuses bytes that are not this type.
    #[arg(long, value_name = "PATH", conflicts_with = "no_check")]
    pub check: Option<String>,
    /// Lift a check a broader row (`image/*`, `*/*`) put on this type.
    #[arg(long)]
    pub no_check: bool,
}

impl AddArgs {
    /// The row edit these flags spell, or why they do not.
    fn edit(&self) -> anyhow::Result<RowEdit> {
        let tokens = self
            .tokens
            .as_deref()
            .map(|raw| TokenSpec::parse(raw).map_err(|e| anyhow::anyhow!("--tokens {raw}: {e}")))
            .transpose()?;
        let check = match (self.no_check, &self.check) {
            (true, _) => Some(String::new()),
            (false, check) => check.clone(),
        };
        Ok(RowEdit {
            family: self.family.clone(),
            text: match (self.text, self.binary) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            },
            tokens,
            extensions: self.extensions.clone(),
            magic: self.magic.clone(),
            stand_in: self.stand_in.clone(),
            check,
        })
    }
}

/// Arguments for `lev mime remove`.
#[derive(Args, Debug)]
pub struct RemoveArgs {
    /// The row's type, exactly as `lev mime list` prints it.
    pub mime_type: String,
}

/// Arguments for `lev mime list`.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// Print the table as JSON, one object per row.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `lev mime check`.
#[derive(Args, Debug)]
pub struct CheckArgs {
    /// The file to resolve.
    pub file: PathBuf,
    /// Take the file as this type instead of sniffing one, as a sender
    /// declaring it would.
    #[arg(long = "type", value_name = "MIME_TYPE")]
    pub mime_type: Option<String>,
    /// Print the answer as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Execute `lev mime`.
pub(crate) async fn execute(args: MimeArgs) -> anyhow::Result<()> {
    // The registry as the daemon builds it, read only by the commands that
    // look at it: `init` writes the file the others would read.
    let registry = || -> anyhow::Result<MimeRegistry> {
        crate::config::Config::load()?
            .mime_registry()
            .map_err(|e| anyhow::anyhow!("the mime registry does not load: {e}"))
    };
    let path = crate::config::mime_types_path();
    let out = match args.command {
        MimeCommand::List(list) => render_list(&registry()?, list.json),
        MimeCommand::Show(show) => render_show(&registry()?, &show.mime_type, show.json)?,
        MimeCommand::Check(check) => render_check(
            &registry()?,
            &check.file,
            check.mime_type.as_deref(),
            check.json,
        )?,
        MimeCommand::Init(init_args) => init(&path, init_args.force)?,
        MimeCommand::Add(add) => {
            // A `type/*` pattern spells like a type, so one parse covers
            // both a row for one type and a row for a family.
            let key = MimeType::parse(&add.mime_type)
                .map_err(|e| anyhow::anyhow!("{}: {e}", add.mime_type))?;
            let added =
                add_row(&path, key.as_str(), &add.edit()?).map_err(|e| anyhow::anyhow!("{e}"))?;
            let verb = match added {
                Added::Created => "added",
                Added::Updated => "updated",
            };
            let mut out = format!("{verb} {key} in {}\n", path.display());
            out.push_str(&show_lines(&registry()?, &key));
            out
        }
        MimeCommand::Remove(remove) => {
            let key = MimeType::parse(&remove.mime_type)
                .map_err(|e| anyhow::anyhow!("{}: {e}", remove.mime_type))?;
            remove_row(&path, key.as_str()).map_err(|e| anyhow::anyhow!("{e}"))?;
            format!("removed {key} from {}\n", path.display())
        }
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
    std::fs::write(path, crate::config::MIME_TYPES_EXAMPLE)?;
    Ok(format!(
        "wrote {}\n  `lev mime list` shows the table it makes; edit the rows and they reach the next run\n",
        path.display()
    ))
}

/// Every row of the registry, resolved: what each type is once the layers
/// have been applied.
fn rows(registry: &MimeRegistry) -> Vec<MimeInfo> {
    let mut keys = registry.keys();
    keys.sort();
    keys.into_iter()
        .filter_map(|(key, _)| MimeType::parse(&key).ok())
        .map(|t| registry.info(&t))
        .collect()
}

/// The registry as a table or as JSON.
fn render_list(registry: &MimeRegistry, json: bool) -> String {
    let rows = rows(registry);
    if json {
        return format!(
            "{}\n",
            serde_json::to_string_pretty(&rows).expect("registry rows always serialize")
        );
    }
    let type_width = rows
        .iter()
        .map(|r| r.mime_type.as_str().len())
        .max()
        .unwrap_or(4)
        .max(4);
    let family_width = rows
        .iter()
        .map(|r| r.family.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let source_width = rows
        .iter()
        .map(|r| r.source.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let mut out = format!(
        "{:<type_width$}  {:<family_width$}  TEXT  EXTENSIONS            {:<source_width$}  CHECK\n",
        "TYPE", "FAMILY", "SOURCE"
    );
    for row in &rows {
        out.push_str(&format!(
            "{:<type_width$}  {:<family_width$}  {:<4}  {:<20}  {:<source_width$}  {}\n",
            row.mime_type,
            row.family,
            match row.text {
                true => "yes",
                false => "no",
            },
            row.extensions.join(","),
            row.source,
            row.check.as_deref().unwrap_or("-"),
        ));
    }
    out.push_str(&format!(
        "\n{} type{}. Rows layer: compiled defaults, [mime_types] in the config, \
         mime_types.toml beside it, then a blueprint's own rows for its runs.\n",
        rows.len(),
        match rows.len() {
            1 => "",
            _ => "s",
        }
    ));
    out
}

/// One type, every field resolved.
fn render_show(registry: &MimeRegistry, raw: &str, json: bool) -> anyhow::Result<String> {
    let mime_type = MimeType::parse(raw).map_err(|e| anyhow::anyhow!("{raw}: {e}"))?;
    if json {
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&registry.info(&mime_type))
                .expect("a row always serializes")
        ));
    }
    Ok(show_lines(registry, &mime_type))
}

/// The lines `lev mime show` prints for one type.
fn show_lines(registry: &MimeRegistry, mime_type: &MimeType) -> String {
    let info = registry.info(mime_type);
    let tokens = match info.tokens {
        leviath_core::mime::TokenRule::PerByte(rate) => format!("{rate} per byte"),
        leviath_core::mime::TokenRule::PerPixel { divisor, max } => {
            format!("one per {divisor} pixels, at most {max}")
        }
        leviath_core::mime::TokenRule::PerSecond(rate) => format!("{rate} per second"),
        leviath_core::mime::TokenRule::Fixed(n) => format!("{n}, whatever the size"),
    };
    let mut out = format!("{mime_type}\n");
    out.push_str(&format!("  family      {}\n", info.family));
    out.push_str(&format!(
        "  text        {}\n",
        match info.text {
            true => "yes",
            false => "no",
        }
    ));
    out.push_str(&format!("  tokens      {tokens}\n"));
    out.push_str(&format!(
        "  extensions  {}\n",
        match info.extensions.is_empty() {
            true => "-".to_string(),
            false => info.extensions.join(", "),
        }
    ));
    out.push_str(&format!(
        "  stand-in    {}\n",
        info.stand_in
            .as_deref()
            .unwrap_or("[{type} {dims}, {size}] {name}")
    ));
    out.push_str(&format!(
        "  check       {}\n",
        match info.check.as_deref() {
            Some(script) if registry.check_for(mime_type).is_some() => script.to_string(),
            Some(script) => format!("{script} (not loaded)"),
            None => "none: bytes are taken at their word".to_string(),
        }
    ));
    out.push_str(&format!("  source      {}\n", info.source));
    out
}

/// What a file resolves to, and how a model would see it.
fn render_check(
    registry: &MimeRegistry,
    file: &Path,
    declared: Option<&str>,
    json: bool,
) -> anyhow::Result<String> {
    let bytes = std::fs::read(file)
        .map_err(|e| anyhow::anyhow!("could not read {}: {e}", file.display()))?;
    let declared = declared
        .map(|t| MimeType::parse(t).map_err(|e| anyhow::anyhow!("--type {t}: {e}")))
        .transpose()?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mime_type = registry.resolve(declared.as_ref(), Some(&name), &bytes);
    let info = registry.info(&mime_type);
    let blob = Blob::new(mime_type.clone(), bytes).named(&name);
    let reference = blob.describe(registry);
    let reaches = match info.text {
        true => "as text, to any model".to_string(),
        false => format!(
            "natively, to a model that lists {mime_type} (`lev models list --accepts \
             {mime_type}`); as its stand-in to any other"
        ),
    };
    // The verdict the run's store would reach: the row's check, run here.
    let (check_name, verdict) = match (info.check.as_deref(), registry.check_for(&mime_type)) {
        (Some(script), Some(check)) => (
            Some(script.to_string()),
            Some(match check.check(&mime_type, &blob.bytes) {
                Ok(()) => "passed".to_string(),
                Err(why) => format!("failed: {why}"),
            }),
        ),
        _ => (None, None),
    };
    if json {
        let value = serde_json::json!({
            "file": name,
            "mime_type": mime_type,
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
            "check": check_name,
            "check_verdict": verdict,
        });
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&value).expect("a check always serializes")
        ));
    }
    let mut out = format!("{name}\n");
    out.push_str(&format!("  type        {mime_type} ({})\n", info.source));
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
    if let (Some(script), Some(verdict)) = (check_name, verdict) {
        out.push_str(&format!("  check       {script}: {verdict}\n"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_names_every_row_with_its_source() {
        let mut registry = MimeRegistry::builtin();
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
        let rows: Vec<MimeInfo> = serde_json::from_str(&json).unwrap();
        let obj = rows
            .iter()
            .find(|r| r.mime_type.as_str() == "model/obj")
            .expect("the layered row is listed");
        assert!(obj.text);
        assert_eq!(obj.source, "config");
    }

    /// The listing names each type's check, and `show` prints every field
    /// with whether the check is loaded.
    #[test]
    fn the_list_and_show_name_a_types_check() {
        let mut registry = MimeRegistry::builtin();
        let table: toml::Table = toml::from_str(
            "[\"image/*\"]\ncheck = \"checks/image.rhai\"\n[\"model/obj\"]\nstand_in = \"[{type}] {name}\"\n",
        )
        .unwrap();
        registry.layer(&table, "config").unwrap();
        let out = render_list(&registry, false);
        assert!(out.contains("CHECK"), "{out}");
        assert!(out.contains("checks/image.rhai"), "{out}");
        assert!(out.contains("then a blueprint's own rows"), "{out}");

        let png = render_show(&registry, "image/png", false).unwrap();
        assert!(png.starts_with("image/png\n"), "{png}");
        assert!(png.contains("family      image"), "{png}");
        assert!(
            png.contains("tokens      one per 750 pixels, at most 1600"),
            "{png}"
        );
        assert!(png.contains("extensions  png"), "{png}");
        assert!(
            png.contains("check       checks/image.rhai (not loaded)"),
            "{png}"
        );
        // The most specific row is the compiled one; the check came down
        // from the family's.
        assert!(png.contains("source      builtin"), "{png}");
        registry
            .attach_check(
                "image/*",
                std::sync::Arc::new(leviath_core::mime::FnCheck::new(
                    "x",
                    |_: &MimeType, _: &[u8]| Ok(()),
                )),
            )
            .unwrap();
        assert_eq!(
            registry.verify(&MimeType::parse("image/png").unwrap(), b"x"),
            Ok(())
        );
        let png = render_show(&registry, "image/png", false).unwrap();
        assert!(png.contains("check       checks/image.rhai\n"), "{png}");
        let obj = render_show(&registry, "model/obj", false).unwrap();
        assert!(obj.contains("text        yes"), "{obj}");
        assert!(obj.contains("tokens      0.25 per byte"), "{obj}");
        assert!(obj.contains("stand-in    [{type}] {name}"), "{obj}");
        assert!(
            obj.contains("check       none: bytes are taken at their word"),
            "{obj}"
        );
        let odd = render_show(&registry, "application/x-nothing", false).unwrap();
        assert!(odd.contains("extensions  -"), "{odd}");
        assert!(
            odd.contains("stand-in    [{type} {dims}, {size}] {name}"),
            "{odd}"
        );
        let wav = render_show(&registry, "audio/wav", false).unwrap();
        assert!(wav.contains("per second"), "{wav}");
        let fixed: toml::Table = toml::from_str("[\"x/y\"]\ntokens = { fixed = 7 }\n").unwrap();
        registry.layer(&fixed, "t").unwrap();
        let xy = render_show(&registry, "x/y", false).unwrap();
        assert!(xy.contains("tokens      7, whatever the size"), "{xy}");
        let json: serde_json::Value =
            serde_json::from_str(&render_show(&registry, "image/png", true).unwrap()).unwrap();
        assert_eq!(json["check"], "checks/image.rhai");
        assert!(render_show(&registry, "nope", false).is_err());
    }

    /// `lev mime check` runs the type's check over the file and says how
    /// it went.
    #[test]
    fn a_check_reports_the_types_verdict_on_the_file() {
        let mut registry = MimeRegistry::builtin();
        let table: toml::Table =
            toml::from_str("[\"image/png\"]\ncheck = \"checks/png.rhai\"\n").unwrap();
        registry.layer(&table, "config").unwrap();
        registry
            .attach_check(
                "image/png",
                std::sync::Arc::new(leviath_core::mime::FnCheck::new(
                    "png",
                    |_: &MimeType, bytes: &[u8]| match bytes.len() > 8 {
                        true => Ok(()),
                        false => Err("only a header".to_string()),
                    },
                )),
            )
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("hero.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
        let out = render_check(&registry, &png, None, false).unwrap();
        assert!(
            out.contains("check       checks/png.rhai: failed: only a header"),
            "{out}"
        );
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nIHDRxx").unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&render_check(&registry, &png, None, true).unwrap()).unwrap();
        assert_eq!(json["check_verdict"], "passed");
        assert_eq!(json["check"], "checks/png.rhai");
        // A type with no check has no verdict.
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, "# hi").unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&render_check(&registry, &notes, None, true).unwrap()).unwrap();
        assert!(json["check_verdict"].is_null());
    }

    /// `lev mime add` and `remove` edit the file beside the config and
    /// echo the row as the registry then sees it.
    #[tokio::test]
    async fn add_and_remove_edit_the_file_beside_the_config() {
        crate::config::with_isolated_config_path_async("mime-add", |dir| async move {
            let path = dir.join("mime_types.toml");
            let _ = std::fs::remove_file(&path);
            let add = |args: AddArgs| async move {
                execute(MimeArgs {
                    command: MimeCommand::Add(args),
                })
                .await
            };
            add(AddArgs {
                mime_type: "Application/X-Acme-Scene".to_string(),
                family: Some("model".to_string()),
                extensions: Some(vec!["scene".to_string()]),
                magic: Some("41434D45".to_string()),
                tokens: Some("per_byte=0.1".to_string()),
                ..AddArgs::default()
            })
            .await
            .unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("[\"application/x-acme-scene\"]"), "{text}");
            assert!(text.contains("tokens = { per_byte = 0.1 }"), "{text}");
            // A family row, text flags, and a lifted check.
            add(AddArgs {
                mime_type: "model/*".to_string(),
                text: true,
                stand_in: Some("[{type}] {name}".to_string()),
                ..AddArgs::default()
            })
            .await
            .unwrap();
            add(AddArgs {
                mime_type: "image/gif".to_string(),
                binary: true,
                no_check: true,
                ..AddArgs::default()
            })
            .await
            .unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("[\"model/*\"]"), "{text}");
            assert!(text.contains("check = \"\""), "{text}");
            let registry = crate::config::Config::load()
                .unwrap()
                .mime_registry()
                .unwrap();
            assert!(registry.info(&"model/stl".parse().unwrap()).text);
            // A check script is compiled before the row is written.
            std::fs::create_dir_all(dir.join("checks")).unwrap();
            std::fs::write(
                dir.join("checks/scene.rhai"),
                "fn check(bytes, mime_type) { () }",
            )
            .unwrap();
            add(AddArgs {
                mime_type: "application/x-acme-scene".to_string(),
                check: Some("checks/scene.rhai".to_string()),
                ..AddArgs::default()
            })
            .await
            .unwrap();
            let registry = crate::config::Config::load()
                .unwrap()
                .mime_registry()
                .unwrap();
            assert!(
                registry
                    .check_for(&"application/x-acme-scene".parse().unwrap())
                    .is_some()
            );
            // Bad flags are refused with a reason, and the file is untouched.
            let before = std::fs::read_to_string(&path).unwrap();
            for (args, needle) in [
                (
                    AddArgs {
                        mime_type: "png".to_string(),
                        ..AddArgs::default()
                    },
                    "png:",
                ),
                (
                    AddArgs {
                        mime_type: "image/png".to_string(),
                        tokens: Some("rate=1".to_string()),
                        ..AddArgs::default()
                    },
                    "--tokens rate=1",
                ),
                (
                    AddArgs {
                        mime_type: "image/png".to_string(),
                        magic: Some("zz".to_string()),
                        ..AddArgs::default()
                    },
                    "magic must be hex",
                ),
            ] {
                let err = add(args).await.unwrap_err();
                assert!(err.to_string().contains(needle), "{err}");
            }
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

            execute(MimeArgs {
                command: MimeCommand::Remove(RemoveArgs {
                    mime_type: "model/*".to_string(),
                }),
            })
            .await
            .unwrap();
            assert!(!std::fs::read_to_string(&path).unwrap().contains("model/*"));
            for bad in ["model/*", "png"] {
                assert!(
                    execute(MimeArgs {
                        command: MimeCommand::Remove(RemoveArgs {
                            mime_type: bad.to_string(),
                        }),
                    })
                    .await
                    .is_err()
                );
            }
            execute(MimeArgs {
                command: MimeCommand::Show(ShowArgs {
                    mime_type: "application/x-acme-scene".to_string(),
                    json: false,
                }),
            })
            .await
            .unwrap();
            let err = execute(MimeArgs {
                command: MimeCommand::Show(ShowArgs {
                    mime_type: "nope".to_string(),
                    json: false,
                }),
            })
            .await
            .unwrap_err();
            assert!(err.to_string().starts_with("nope:"), "{err}");
        })
        .await;
    }

    #[test]
    fn a_singular_registry_reads_as_one_type() {
        let mut registry = MimeRegistry::empty();
        let table: toml::Table = toml::from_str("[\"x/y\"]\nfamily = \"odd\"\n").unwrap();
        registry.layer(&table, "test").unwrap();
        let out = render_list(&registry, false);
        assert!(out.contains("\n1 type. Rows layer"), "{out}");
    }

    #[test]
    fn a_check_says_what_a_file_is_and_how_a_model_sees_it() {
        let registry = MimeRegistry::builtin();
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
        assert_eq!(parsed["mime_type"], "text/plain");
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
        crate::config::with_isolated_config_path_async("mime-execute", |_dir| async move {
            assert!(
                execute(MimeArgs {
                    command: MimeCommand::List(ListArgs { json: true }),
                })
                .await
                .is_ok()
            );
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("a.txt");
            std::fs::write(&file, "text").unwrap();
            assert!(
                execute(MimeArgs {
                    command: MimeCommand::Check(CheckArgs {
                        file,
                        mime_type: None,
                        json: false,
                    }),
                })
                .await
                .is_ok()
            );
        })
        .await;
    }

    /// `lev mime init` writes the example once, replaces it only when
    /// forced, and says where it went.
    #[tokio::test]
    async fn init_writes_the_example_beside_the_config() {
        crate::config::with_isolated_config_path_async("mime-init", |dir| async move {
            let path = dir.join("mime_types.toml");
            assert!(
                execute(MimeArgs {
                    command: MimeCommand::Init(InitArgs::default()),
                })
                .await
                .is_ok()
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                crate::config::MIME_TYPES_EXAMPLE
            );
            let err = init(&path, false).unwrap_err();
            assert!(err.to_string().contains("--force"), "{err}");
            assert!(
                execute(MimeArgs {
                    command: MimeCommand::Init(InitArgs::default()),
                })
                .await
                .is_err()
            );
            std::fs::write(&path, "[\"x/y\"]\nfamily = \"custom\"\n").unwrap();
            init(&path, true).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                crate::config::MIME_TYPES_EXAMPLE
            );
            // The rows the file adds show up in the listing, with their source.
            std::fs::write(&path, "[\"x/y\"]\nfamily = \"custom\"\n").unwrap();
            let registry = crate::config::Config::load()
                .unwrap()
                .mime_registry()
                .unwrap();
            let out = render_list(&registry, false);
            assert!(out.contains("mime_types.toml"), "{out}");
            // A parent that is a file cannot be created; a path that is a
            // directory cannot be written.
            let blocker = dir.join("blocker");
            std::fs::write(&blocker, "").unwrap();
            assert!(init(&blocker.join("mime_types.toml"), false).is_err());
            let taken = dir.join("taken");
            std::fs::create_dir_all(&taken).unwrap();
            assert!(init(&taken, true).is_err());
        })
        .await;
    }

    /// A `[mime_types]` table that does not layer, and a config that does
    /// not parse at all, are each refused with a reason rather than run on
    /// the defaults.
    #[tokio::test]
    async fn execute_refuses_a_config_whose_registry_does_not_load() {
        crate::config::with_isolated_config_path_async("mime-bad-config", |dir| async move {
            let config = dir.join("config.toml");
            std::fs::write(
                &config,
                "[mime_types.\"image/png\"]\ntokens = { per_byte = 0.5, fixed = 3 }\n",
            )
            .unwrap();
            let err = execute(MimeArgs {
                command: MimeCommand::List(ListArgs { json: false }),
            })
            .await
            .unwrap_err();
            assert!(err.to_string().contains("[mime_types]"), "{err}");
            assert!(
                execute(MimeArgs {
                    command: MimeCommand::Check(CheckArgs {
                        file: dir.join("config.toml"),
                        mime_type: None,
                        json: false,
                    }),
                })
                .await
                .is_err()
            );
            // `show`, and the lines `add` prints after writing, read the
            // same registry and fail the same way; the row is written first.
            assert!(
                execute(MimeArgs {
                    command: MimeCommand::Show(ShowArgs {
                        mime_type: "image/png".to_string(),
                        json: false,
                    }),
                })
                .await
                .is_err()
            );
            let err = execute(MimeArgs {
                command: MimeCommand::Add(AddArgs {
                    mime_type: "model/obj".to_string(),
                    text: true,
                    ..AddArgs::default()
                }),
            })
            .await
            .unwrap_err();
            assert!(err.to_string().contains("[mime_types]"), "{err}");
            assert!(
                std::fs::read_to_string(dir.join("mime_types.toml"))
                    .unwrap()
                    .contains("[\"model/obj\"]")
            );
            std::fs::write(&config, "this is not toml = = =\n").unwrap();
            assert!(
                execute(MimeArgs {
                    command: MimeCommand::List(ListArgs { json: false }),
                })
                .await
                .is_err()
            );
        })
        .await;
    }
}
