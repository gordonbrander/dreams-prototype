use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use subconscious::{ListQuery, PutInput, Store, StoreError, mcp};

/// Subconscious: a versioned document vault in SQLite, with a CLI and an MCP server.
#[derive(Parser)]
#[command(name = "subconscious", version, about)]
struct Cli {
    /// Path to the SQLite database file.
    #[arg(long, global = true, default_value = "vault.db")]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the database if needed and apply migrations.
    Init,
    /// Serve MCP (2026-07-28, stateless) over stdio.
    Serve,
    /// Documents.
    #[command(subcommand)]
    Doc(DocCmd),
    /// JSON Schemas that validate typed documents.
    #[command(subcommand)]
    Schema(SchemaCmd),
}

#[derive(Args)]
struct JsonInput {
    /// Read JSON from this file instead of stdin.
    #[arg(long)]
    file: Option<PathBuf>,
}

#[derive(Args, Default)]
struct Filter {
    /// Only documents with this _type.
    #[arg(long = "type")]
    type_id: Option<String>,
    /// Only documents whose current revision has this tag.
    #[arg(long)]
    tag: Option<String>,
    /// Keyset cursor from a previous page's `next`.
    #[arg(long)]
    before: Option<i64>,
    /// Page size (1..=1000).
    #[arg(long)]
    limit: Option<usize>,
}

impl From<Filter> for ListQuery {
    fn from(f: Filter) -> Self {
        ListQuery {
            type_id: f.type_id,
            tag: f.tag,
            before: f.before,
            limit: f.limit,
        }
    }
}

#[derive(Subcommand)]
enum DocCmd {
    /// Create or update a document from JSON. Pass _parent to update.
    Put(JsonInput),
    /// Print the current revision of a document.
    Get { id: String },
    /// Write a tombstone. --parent is the current _rev.
    Delete {
        id: String,
        #[arg(long)]
        parent: String,
    },
    /// List current documents, most recently modified first.
    List(Filter),
    /// Revision history, newest first.
    History {
        id: String,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Every revision committed after --since, in order.
    Changes {
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Full-text search over title, content and tags.
    Search {
        query: String,
        #[command(flatten)]
        filter: Filter,
    },
}

#[derive(Subcommand)]
enum SchemaCmd {
    /// Register a JSON Schema (needs $id, title, description).
    Register(JsonInput),
    /// Print a registered schema.
    Get { id: String },
    /// List registered schemas.
    List,
}

fn read_json<T: serde::de::DeserializeOwned>(input: &JsonInput) -> Result<T> {
    let text = match &input.file {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).context("reading stdin")?;
            s
        }
    };
    Ok(serde_json::from_str(&text)?)
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => {
            Store::open(&cli.db)?;
            println!("initialized {}", cli.db.display());
            Ok(())
        }
        Command::Serve => {
            let store = Store::open(&cli.db)?;
            tokio::runtime::Runtime::new()?.block_on(mcp::serve(store))
        }
        Command::Doc(cmd) => {
            let mut store = Store::open(&cli.db)?;
            match cmd {
                DocCmd::Put(input) => {
                    let put: PutInput = read_json(&input)?;
                    print_json(&store.put(put)?)
                }
                DocCmd::Get { id } => print_json(&store.get(&id)?),
                DocCmd::Delete { id, parent } => print_json(&store.delete(&id, &parent)?),
                DocCmd::List(filter) => print_json(&store.list(&filter.into())?),
                DocCmd::History { id, limit } => print_json(&store.history(&id, limit)?),
                DocCmd::Changes { since, limit } => print_json(&store.changes(since, limit)?),
                DocCmd::Search { query, filter } => print_json(&store.search(&query, &filter.into())?),
            }
        }
        Command::Schema(cmd) => {
            let mut store = Store::open(&cli.db)?;
            match cmd {
                SchemaCmd::Register(input) => {
                    let schema: serde_json::Value = read_json(&input)?;
                    print_json(&store.register_schema(schema)?)
                }
                SchemaCmd::Get { id } => print_json(&store.get_schema(&id)?),
                SchemaCmd::List => print_json(&store.list_schemas()?),
            }
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        // Store errors print as JSON so scripts can discriminate on `name`.
        match err.downcast_ref::<StoreError>() {
            Some(store_err) => {
                let mut v = serde_json::to_value(store_err).unwrap_or_default();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("message".into(), serde_json::Value::String(store_err.to_string()));
                }
                eprintln!("{v}");
            }
            None => eprintln!("error: {err:#}"),
        }
        std::process::exit(1);
    }
}
