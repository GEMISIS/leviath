//! `lev schema`: print the JSON Schemas Leviath publishes.
//!
//! `lev schema spawn-request` prints the schema of a spawn request: what
//! `lev run --request` reads, what `POST /api/runs` takes, and what an agent
//! writing a request for another run is checked against.

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
}

/// Print the schema `args` names.
pub async fn execute(args: SchemaArgs) -> anyhow::Result<()> {
    println!("{}", render(args.schema));
    Ok(())
}

/// The schema, as printed.
pub(crate) fn render(schema: Schema) -> String {
    let value = match schema {
        Schema::SpawnRequest => leviath_runtime::runfile::spawn_request_schema(),
    };
    serde_json::to_string_pretty(&value).expect("a schema is plain JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_spawn_request_schema_is_the_published_one() {
        let printed = render(Schema::SpawnRequest);
        let value: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(value, leviath_runtime::runfile::spawn_request_schema());
        assert!(printed.contains("\"source\""), "{printed}");
        execute(SchemaArgs {
            schema: Schema::SpawnRequest,
        })
        .await
        .unwrap();
    }
}
