//! `lev schema`: print the JSON Schemas Leviath publishes.
//!
//! `lev schema spawn-request` prints the schema of a spawn request: what
//! `lev run --request` reads, what `POST /api/runs` takes, and what an agent
//! writing a request for another run is checked against.
//!
//! `lev schema blueprint` prints the schema of an `agent.toml`, with this
//! machine's tool and provider names added as completions (see
//! [`crate::blueprint_schema`]). `--write` puts it in the data directory,
//! where an editor reads it.

use std::path::Path;

use clap::{Args, Subcommand};

/// Arguments for `lev schema`.
#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct SchemaArgs {
    /// Which schema.
    #[command(subcommand)]
    pub schema: Schema,
}

/// The schemas `lev schema` prints.
#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schema {
    /// The JSON Schema of a spawn request
    SpawnRequest,
    /// The JSON Schema of an agent.toml, with this machine's tools and providers
    Blueprint(BlueprintSchemaArgs),
}

/// Arguments for `lev schema blueprint`.
#[derive(Args, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlueprintSchemaArgs {
    /// Write it to agent.schema.json in the data directory instead of printing it
    #[arg(long)]
    pub write: bool,
}

/// Print the schema `args` names.
pub async fn execute(args: SchemaArgs) -> anyhow::Result<()> {
    let text = match args.schema {
        Schema::SpawnRequest => pretty(&leviath_runtime::runfile::spawn_request_schema()),
        Schema::Blueprint(args) => {
            let data_dir = leviath_core::data_dir().unwrap_or_default();
            blueprint(args, &data_dir, &crate::config::Config::load()?)?
        }
    };
    println!("{text}");
    Ok(())
}

/// What `lev schema blueprint` prints, against the data directory
/// `data_dir` and `config`: the schema, or with `--write`, where it went.
pub(crate) fn blueprint(
    args: BlueprintSchemaArgs,
    data_dir: &Path,
    config: &crate::config::Config,
) -> anyhow::Result<String> {
    match args.write {
        true => {
            let path = crate::blueprint_schema::write(data_dir, config, None)?;
            Ok(format!("Wrote {}", path.display()))
        }
        false => Ok(pretty(&crate::blueprint_schema::render(
            data_dir, config, None,
        ))),
    }
}

fn pretty(schema: &serde_json::Value) -> String {
    serde_json::to_string_pretty(schema).expect("a schema is plain JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_spawn_request_schema_is_the_published_one() {
        let printed = pretty(&leviath_runtime::runfile::spawn_request_schema());
        assert!(printed.contains("\"source\""), "{printed}");
        // Read no config: the spawn request schema is the same everywhere.
        execute(SchemaArgs {
            schema: Schema::SpawnRequest,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_blueprint_schema_reads_the_config() {
        crate::config::with_isolated_config_path_async("schema-blueprint", |dir| async move {
            let args = SchemaArgs {
                schema: Schema::Blueprint(Default::default()),
            };
            execute(args.clone()).await.unwrap();
            // A data directory that cannot be written to fails `--write`.
            let data = dir.join(".leviath");
            let _ = std::fs::remove_dir_all(&data);
            std::fs::write(&data, "").unwrap();
            let write = SchemaArgs {
                schema: Schema::Blueprint(BlueprintSchemaArgs { write: true }),
            };
            assert!(execute(write).await.is_err());
            // A config that does not read fails the command.
            let path = std::env::var_os("LEVIATH_CONFIG_PATH").unwrap();
            std::fs::write(path, "not = [toml").unwrap();
            assert!(execute(args).await.is_err());
        })
        .await;
    }

    #[test]
    fn the_blueprint_schema_prints_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config::default();
        let printed = blueprint(Default::default(), dir.path(), &config).unwrap();
        let value: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(
            value,
            crate::blueprint_schema::render(dir.path(), &config, None)
        );
        let path = dir.path().join(crate::blueprint_schema::FILE_NAME);
        assert!(!path.exists(), "printing writes nothing");

        let write = BlueprintSchemaArgs { write: true };
        let wrote = blueprint(write, dir.path(), &config).unwrap();
        assert_eq!(wrote, format!("Wrote {}", path.display()));
        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk, value);

        // A data directory that cannot be made is the command's failure.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        assert!(blueprint(write, &blocked, &config).is_err());
    }
}
