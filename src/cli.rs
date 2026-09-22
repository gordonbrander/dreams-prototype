//! Command line interface. Every MCP tool has a subcommand here with the
//! same semantics. `run` is in-process so tests can drive it.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::doc::{Doc, PutInput};
use crate::error::StoreError;
use crate::store::{Changes, History, ListQuery, Page, Store};
use crate::{markdown, mcp, rev};

/// Subconscious: a versioned document vault in SQLite, with a CLI and an MCP server.
#[derive(Parser)]
#[command(name = "subconscious", version, about)]
pub struct Cli {
    /// Path to the SQLite database file.
    #[arg(long, global = true, default_value = "vault.db")]
    db: PathBuf,
    /// Print JSON (the same structures the MCP tools return) instead of human output.
    #[arg(long, global = true)]
    json: bool,
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
    /// Write current documents as Markdown files at <dir>/<_id>.
    Export {
        dir: PathBuf,
        /// Only documents with this _type.
        #[arg(long = "type")]
        type_id: Option<String>,
        /// Only documents whose current revision has this tag.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Import every *.md file under <dir>. The relative path is the _id (a frontmatter
    /// _id that differs is ignored). Unchanged exported files are no-ops; edited files
    /// become the next revision.
    Import { dir: PathBuf },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Json,
    Yaml,
    /// Markdown with YAML frontmatter; the body is `content`.
    Md,
}

#[derive(Args)]
struct Input {
    /// File to read. `-` or absent means stdin.
    file: Option<PathBuf>,
    /// Input format. Defaults from the file extension, or JSON for stdin.
    #[arg(long, value_enum)]
    format: Option<Format>,
}

#[derive(Args, Default)]
struct Filter {
    /// Only documents with this _type.
    #[arg(long = "type")]
    type_id: Option<String>,
    /// Only documents whose current revision has this tag.
    #[arg(long)]
    tag: Option<String>,
    /// Page size (1..=1000, default 50).
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Subcommand)]
enum DocCmd {
    /// Create a document, or update one when the input names a revision
    /// (_parent, or the _rev of a fetched document that was edited).
    Put(Input),
    /// Replace a document's body. Parent: --parent, else the input's _rev (if edited)
    /// or _parent, else the current revision. Reviving a deleted document works.
    /// An input without _type drops the type.
    Update {
        id: String,
        #[command(flatten)]
        input: Input,
        /// Revision being replaced (explicit compare-and-swap).
        #[arg(long)]
        parent: Option<String>,
    },
    /// Print the current revision, or one revision with --rev.
    Get {
        id: String,
        #[arg(long)]
        rev: Option<String>,
    },
    /// Write a tombstone. Parent defaults to the current revision.
    Delete {
        id: String,
        #[arg(long)]
        parent: Option<String>,
    },
    /// List current documents, most recently modified first.
    List {
        #[command(flatten)]
        filter: Filter,
        /// Keyset cursor from a previous page's `next`.
        #[arg(long)]
        before: Option<i64>,
    },
    /// Full-text search over title, content and tags, best match first.
    Search {
        query: String,
        #[command(flatten)]
        filter: Filter,
    },
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
}

#[derive(Subcommand)]
enum SchemaCmd {
    /// Register a JSON Schema (needs $id, title, description). Immutable once registered.
    Register(Input),
    /// Print a registered schema as JSON.
    Get { id: String },
    /// List registered schemas.
    List,
}

// ---- entry point --------------------------------------------------------

/// Parse `args` (including argv[0]) and run. Returns the exit code. All
/// output goes to the given writers; nothing here exits the process.
pub fn run<I, T>(args: I, stdin: &mut dyn Read, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let out: &mut dyn Write = if e.use_stderr() { stderr } else { stdout };
            let _ = write!(out, "{}", e.render());
            return e.exit_code();
        }
    };
    match execute(cli, stdin, stdout) {
        Ok(()) => 0,
        Err(err) => {
            if let Some(io_err) = err.downcast_ref::<io::Error>()
                && io_err.kind() == io::ErrorKind::BrokenPipe
            {
                return 0;
            }
            match err.downcast_ref::<StoreError>() {
                Some(store_err) => {
                    let mut v = serde_json::to_value(store_err).unwrap_or_default();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("message".into(), Value::String(store_err.to_string()));
                    }
                    let _ = writeln!(stderr, "{v}");
                }
                None => {
                    let _ = writeln!(stderr, "error: {err:#}");
                }
            }
            1
        }
    }
}

fn execute(cli: Cli, stdin: &mut dyn Read, out: &mut dyn Write) -> anyhow::Result<()> {
    let json = cli.json;
    match cli.command {
        Command::Init => {
            Store::open(&cli.db)?;
            writeln!(out, "initialized {}", cli.db.display())?;
        }
        Command::Serve => {
            let store = Store::open(&cli.db)?;
            tokio::runtime::Runtime::new()?.block_on(mcp::serve(store))?;
        }
        Command::Doc(cmd) => {
            let mut store = Store::open(&cli.db)?;
            doc_cmd(&mut store, cmd, json, stdin, out)?;
        }
        Command::Schema(cmd) => {
            let mut store = Store::open(&cli.db)?;
            schema_cmd(&mut store, cmd, json, stdin, out)?;
        }
        Command::Export { dir, type_id, tag } => {
            let store = Store::open(&cli.db)?;
            export(&store, &dir, ListQuery { type_id, tag, ..Default::default() }, json, out)?;
        }
        Command::Import { dir } => {
            let mut store = Store::open(&cli.db)?;
            import(&mut store, &dir, json, out)?;
        }
    }
    Ok(())
}

fn doc_cmd(store: &mut Store, cmd: DocCmd, json: bool, stdin: &mut dyn Read, out: &mut dyn Write) -> anyhow::Result<()> {
    match cmd {
        DocCmd::Put(input) => {
            let map = read_input(&input, stdin)?;
            let prepared = prepare_write(map, WriteMode::Put)?;
            let doc = store.put(prepared.input)?;
            print_doc(out, json, &doc)?;
        }
        DocCmd::Update { id, input, parent } => {
            let map = read_input(&input, stdin)?;
            // update never creates: the document must exist (a tombstone counts)
            head_rev(store, &id)?;
            let (doc, _) = upsert(store, &id, map, parent)?;
            print_doc(out, json, &doc)?;
        }
        DocCmd::Get { id, rev } => {
            let doc = match rev {
                None => store.get(&id)?,
                Some(rev) => {
                    let doc = store.get_rev(&rev)?;
                    if doc.id != id {
                        return Err(StoreError::invalid(format!("revision {rev} belongs to {}, not {id}", doc.id)).into());
                    }
                    doc
                }
            };
            print_doc(out, json, &doc)?;
        }
        DocCmd::Delete { id, parent } => {
            let parent = match parent {
                Some(p) => p,
                None => store.get(&id)?.rev,
            };
            let doc = store.delete(&id, &parent)?;
            print_doc(out, json, &doc)?;
        }
        DocCmd::List { filter, before } => {
            let q = ListQuery { before, ..filter.into() };
            let page = store.list(&q)?;
            print_page(out, json, &page)?;
        }
        DocCmd::Search { query, filter } => {
            let page = store.search(&query, &filter.into())?;
            print_page(out, json, &page)?;
        }
        DocCmd::History { id, limit } => {
            let history = store.history(&id, limit)?;
            print_history(out, json, &history)?;
        }
        DocCmd::Changes { since, limit } => {
            let changes = store.changes(since, limit)?;
            print_changes(out, json, &changes)?;
        }
    }
    Ok(())
}

fn schema_cmd(store: &mut Store, cmd: SchemaCmd, json: bool, stdin: &mut dyn Read, out: &mut dyn Write) -> anyhow::Result<()> {
    match cmd {
        SchemaCmd::Register(input) => {
            let map = read_input(&input, stdin)?;
            let summary = store.register_schema(Value::Object(map))?;
            if json {
                print_json(out, &summary)?;
            } else {
                writeln!(out, "registered {}: {}", summary.id, summary.title)?;
            }
        }
        SchemaCmd::Get { id } => print_json(out, &store.get_schema(&id)?)?,
        SchemaCmd::List => {
            let list = store.list_schemas()?;
            if json {
                print_json(out, &list)?;
            } else {
                let rows: Vec<Vec<String>> = list
                    .schemas
                    .iter()
                    .map(|s| vec![s.id.clone(), s.title.clone(), clip(&s.description, 60)])
                    .collect();
                table(out, &["ID", "TITLE", "DESCRIPTION"], &rows)?;
            }
        }
    }
    Ok(())
}

impl From<Filter> for ListQuery {
    fn from(f: Filter) -> Self {
        ListQuery {
            type_id: f.type_id,
            tag: f.tag,
            before: None,
            limit: f.limit,
        }
    }
}

/// The current revision's id, tombstone or not.
fn head_rev(store: &Store, id: &str) -> Result<String, StoreError> {
    match store.get(id) {
        Ok(doc) => Ok(doc.rev),
        Err(StoreError::Deleted { rev, .. }) => Ok(rev),
        Err(e) => Err(e),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteStatus {
    Created,
    Updated,
    Unchanged,
}

/// Write `map` as document `id`, creating it or replacing its body.
/// Parent: `explicit`, else the map's `_rev` (if edited) or `_parent`, else
/// the current head (a tombstone revives). Shared by `doc update` and import.
fn upsert(
    store: &mut Store,
    id: &str,
    map: Map<String, Value>,
    explicit: Option<String>,
) -> Result<(Doc, WriteStatus), StoreError> {
    let mut prepared = prepare_write(map, WriteMode::Update)?;
    if let Some(input_id) = &prepared.input.id
        && input_id != id
    {
        return Err(StoreError::invalid(format!("input _id {input_id:?} does not match {id:?}")));
    }
    prepared.input.id = Some(id.to_string());
    let mut existed = true;
    if let Some(p) = explicit {
        prepared.input.parent = Some(p);
    } else if prepared.input.parent.is_none() && !prepared.unchanged {
        match head_rev(store, id) {
            Ok(rev) => prepared.input.parent = Some(rev),
            Err(StoreError::NotFound { .. }) => existed = false,
            Err(e) => return Err(e),
        }
    }
    let doc = store.put(prepared.input)?;
    let status = if prepared.unchanged {
        WriteStatus::Unchanged
    } else if existed {
        WriteStatus::Updated
    } else {
        WriteStatus::Created
    };
    Ok((doc, status))
}

// ---- import / export ----------------------------------------------------

/// The `_id` as a relative path. Rejects ids that would escape the folder.
pub fn id_to_relpath(id: &str) -> Result<PathBuf, StoreError> {
    let mut path = PathBuf::new();
    for segment in id.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('\\') {
            return Err(StoreError::invalid(format!("_id {id:?} cannot be used as a file path")));
        }
        path.push(segment);
    }
    Ok(path)
}

/// The `_id` for a file at `rel` (relative to the import root): the path
/// components joined with `/`, extension included. `None` unless it is `.md`.
pub fn relpath_to_id(rel: &Path) -> Option<String> {
    if rel.extension()?.to_str()? != "md" {
        return None;
    }
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

fn walk_md(dir: &Path, root: &Path, files: &mut Vec<PathBuf>) -> Result<(), StoreError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| StoreError::invalid(format!("reading {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::invalid(format!("reading {}: {e}", dir.display())))?;
        let path = entry.path();
        if path.is_dir() {
            walk_md(&path, root, files)?;
        } else if relpath_to_id(path.strip_prefix(root).unwrap_or(&path)).is_some() {
            files.push(path);
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct FileResult {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<WriteStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct FileReport {
    results: Vec<FileResult>,
    errors: usize,
}

fn report(out: &mut dyn Write, json: bool, verb: &str, results: Vec<FileResult>) -> anyhow::Result<()> {
    let errors = results.iter().filter(|r| r.error.is_some()).count();
    if json {
        print_json(out, &FileReport { results, errors })?;
    } else {
        let mut counts = [0usize; 3];
        for r in &results {
            match (&r.error, r.status) {
                (Some(e), _) => writeln!(out, "error      {}: {e}", r.path)?,
                (None, Some(status)) => {
                    counts[status as usize] += 1;
                    let rev = r.rev.as_deref().map(short_rev).unwrap_or_default();
                    writeln!(out, "{:<10} {} {rev}", format!("{status:?}").to_lowercase(), r.path)?;
                }
                (None, None) => writeln!(out, "{verb:<10} {}", r.path)?,
            }
        }
        if verb == "imported" {
            writeln!(
                out,
                "{} created, {} updated, {} unchanged, {errors} errors",
                counts[0], counts[1], counts[2]
            )?;
        } else {
            writeln!(out, "{} {verb}, {errors} errors", results.len() - errors)?;
        }
    }
    if errors > 0 {
        return Err(StoreError::invalid(format!("{errors} file(s) failed")).into());
    }
    Ok(())
}

fn export(store: &Store, dir: &Path, filter: ListQuery, json: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut results = Vec::new();
    let mut q = ListQuery { limit: Some(1000), ..filter };
    loop {
        let page = store.list(&q)?;
        for doc in &page.docs {
            let rel = match id_to_relpath(&doc.id) {
                Ok(rel) => rel,
                Err(e) => {
                    results.push(FileResult { path: doc.id.clone(), id: Some(doc.id.clone()), rev: None, status: None, error: Some(e.to_string()) });
                    continue;
                }
            };
            let path = dir.join(&rel);
            let written = path
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|_| std::fs::write(&path, markdown::render(doc)));
            results.push(FileResult {
                path: rel.to_string_lossy().into_owned(),
                id: Some(doc.id.clone()),
                rev: Some(doc.rev.clone()),
                status: None,
                error: written.err().map(|e| e.to_string()),
            });
        }
        match page.next {
            Some(next) => q.before = Some(next),
            None => break,
        }
    }
    results.sort_by(|a, b| a.path.cmp(&b.path));
    report(out, json, "exported", results)
}

fn import(store: &mut Store, dir: &Path, json: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut files = Vec::new();
    walk_md(dir, dir, &mut files)?;
    files.sort();
    let mut results = Vec::new();
    for path in files {
        let rel = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
        let rel_text = rel.to_string_lossy().into_owned();
        let id = relpath_to_id(&rel).expect("walk_md only collects .md files");
        let outcome = std::fs::read_to_string(&path)
            .map_err(|e| StoreError::invalid(format!("reading {}: {e}", path.display())))
            .and_then(|text| markdown::parse(&text))
            .and_then(|mut map| {
                // The path is the id. A file copied or moved from elsewhere carries
                // another document's _id and _rev: drop them and write fresh.
                if map.get("_id").is_some_and(|v| v.as_str() != Some(&id)) {
                    map.remove("_id");
                    map.remove("_rev");
                    map.remove("_parent");
                }
                upsert(store, &id, map, None)
            });
        results.push(match outcome {
            Ok((doc, status)) => FileResult { path: rel_text, id: Some(id), rev: Some(doc.rev), status: Some(status), error: None },
            Err(e) => FileResult { path: rel_text, id: Some(id), rev: None, status: None, error: Some(e.to_string()) },
        });
    }
    report(out, json, "imported", results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_and_path_map_both_ways() {
        assert_eq!(id_to_relpath("a.md").unwrap(), PathBuf::from("a.md"));
        assert_eq!(id_to_relpath("notes/2026/a.md").unwrap(), PathBuf::from("notes/2026/a.md"));
        assert_eq!(id_to_relpath("no-extension").unwrap(), PathBuf::from("no-extension"));
        for bad in ["../x", "a/../b", "/abs", "a//b", "a/", "./a", "a\\b"] {
            assert!(id_to_relpath(bad).is_err(), "{bad}");
        }
        assert_eq!(relpath_to_id(Path::new("a.md")).as_deref(), Some("a.md"));
        assert_eq!(relpath_to_id(Path::new("notes/2026/a.md")).as_deref(), Some("notes/2026/a.md"));
        assert_eq!(relpath_to_id(Path::new("a.txt")), None);
        assert_eq!(relpath_to_id(Path::new("README")), None);
    }
}

// ---- input --------------------------------------------------------------

fn detect_format(path: &Path) -> Option<Format> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "json" => Some(Format::Json),
        "yaml" | "yml" => Some(Format::Yaml),
        "md" | "markdown" => Some(Format::Md),
        _ => None,
    }
}

fn read_input(input: &Input, stdin: &mut dyn Read) -> Result<Map<String, Value>, StoreError> {
    let file = input.file.as_deref().filter(|p| *p != Path::new("-"));
    let (text, format) = match file {
        Some(path) => {
            let format = input.format.or_else(|| detect_format(path)).ok_or_else(|| {
                StoreError::invalid(format!("cannot infer the format of {}; pass --format", path.display()))
            })?;
            let text = std::fs::read_to_string(path)
                .map_err(|e| StoreError::invalid(format!("reading {}: {e}", path.display())))?;
            (text, format)
        }
        None => {
            let mut text = String::new();
            stdin
                .read_to_string(&mut text)
                .map_err(|e| StoreError::invalid(format!("reading stdin: {e}")))?;
            (text, input.format.unwrap_or(Format::Json))
        }
    };
    parse_input(&text, format)
}

pub fn parse_input(text: &str, format: Format) -> Result<Map<String, Value>, StoreError> {
    match format {
        Format::Json => match serde_json::from_str::<Value>(text)? {
            Value::Object(map) => Ok(map),
            _ => Err(StoreError::invalid("JSON input must be an object")),
        },
        Format::Yaml => markdown::yaml_to_map(text),
        Format::Md => markdown::parse(text),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    Put,
    Update,
}

pub struct Prepared {
    pub input: PutInput,
    /// The input carried a `_rev` that matches its content: nothing was edited.
    pub unchanged: bool,
}

/// Make fetched output writable again. Strips store-assigned fields and
/// resolves a fetched `_rev` into the parent of the edit. The library stays
/// strict; this leniency lives only in the CLI.
pub fn prepare_write(mut map: Map<String, Value>, mode: WriteMode) -> Result<Prepared, StoreError> {
    for key in ["_parent", "_type"] {
        if map.get(key).is_some_and(Value::is_null) {
            map.remove(key);
        }
    }
    map.remove("_created_at");
    map.remove("_seq");
    if let Some(deleted) = map.remove("_deleted")
        && deleted.as_bool() == Some(true)
        && mode == WriteMode::Put
    {
        return Err(StoreError::invalid("input is a tombstone; use `doc update` to revive the document"));
    }

    let mut unchanged = false;
    if let Some(rev_value) = map.remove("_rev") {
        let rev_id = rev_value
            .as_str()
            .ok_or_else(|| StoreError::invalid("_rev must be a string"))?
            .to_string();
        let id = map
            .get("_id")
            .and_then(Value::as_str)
            .ok_or_else(|| StoreError::invalid("input has _rev but no _id"))?;
        let parent = map.get("_parent").and_then(Value::as_str);
        let type_id = map.get("_type").and_then(Value::as_str);
        let body: Map<String, Value> = map
            .iter()
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let computed = rev::rev_of(id, parent, type_id, false, &body)?;
        if computed == rev_id {
            unchanged = true;
        } else {
            map.insert("_parent".into(), Value::String(rev_id));
        }
    }

    let input: PutInput = serde_json::from_value(Value::Object(map))?;
    Ok(Prepared { input, unchanged })
}

// ---- output -------------------------------------------------------------

fn print_json<T: Serialize>(out: &mut dyn Write, value: &T) -> io::Result<()> {
    writeln!(out, "{}", serde_json::to_string_pretty(value).expect("store types serialize"))
}

fn print_doc(out: &mut dyn Write, json: bool, doc: &Doc) -> io::Result<()> {
    if json {
        print_json(out, doc)
    } else {
        write!(out, "{}", markdown::render(doc))
    }
}

fn short_rev(rev: &str) -> String {
    match rev.split_once('-') {
        Some((g, h)) => format!("{g}-{}", &h[..h.len().min(8)]),
        None => rev.to_string(),
    }
}

fn clip(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let cut: String = one_line.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

fn title_of(doc: &Doc) -> String {
    doc.body.get("title").and_then(Value::as_str).map(|t| clip(t, 60)).unwrap_or_default()
}

fn tags_of(doc: &Doc) -> String {
    doc.body
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

fn print_page(out: &mut dyn Write, json: bool, page: &Page) -> io::Result<()> {
    if json {
        return print_json(out, page);
    }
    let rows: Vec<Vec<String>> = page
        .docs
        .iter()
        .map(|d| {
            vec![
                d.id.clone(),
                short_rev(&d.rev),
                d.type_id.clone().unwrap_or_default(),
                title_of(d),
                tags_of(d),
            ]
        })
        .collect();
    table(out, &["ID", "REV", "TYPE", "TITLE", "TAGS"], &rows)?;
    if let Some(next) = page.next {
        writeln!(out, "next: {next}")?;
    }
    Ok(())
}

fn print_history(out: &mut dyn Write, json: bool, history: &History) -> io::Result<()> {
    if json {
        return print_json(out, history);
    }
    let rows: Vec<Vec<String>> = history
        .revisions
        .iter()
        .map(|d| {
            vec![
                short_rev(&d.rev),
                d.created_at.clone(),
                if d.deleted { "yes".into() } else { String::new() },
                title_of(d),
            ]
        })
        .collect();
    table(out, &["REV", "CREATED", "DELETED", "TITLE"], &rows)
}

fn print_changes(out: &mut dyn Write, json: bool, changes: &Changes) -> io::Result<()> {
    if json {
        return print_json(out, changes);
    }
    let rows: Vec<Vec<String>> = changes
        .results
        .iter()
        .map(|d| {
            vec![
                d.seq.map(|s| s.to_string()).unwrap_or_default(),
                d.id.clone(),
                short_rev(&d.rev),
                if d.deleted { "yes".into() } else { String::new() },
                title_of(d),
            ]
        })
        .collect();
    table(out, &["SEQ", "ID", "REV", "DELETED", "TITLE"], &rows)?;
    writeln!(out, "last_seq: {}", changes.last_seq)
}

/// Plain text table: header row, two-space gutters, columns padded to the
/// widest cell. Trailing spaces are trimmed.
fn table(out: &mut dyn Write, headers: &[&str], rows: &[Vec<String>]) -> io::Result<()> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let render_row = |cells: &[&str]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            let pad = widths[i] - cell.chars().count();
            line.extend(std::iter::repeat_n(' ', pad));
        }
        line.trim_end().to_string()
    };
    writeln!(out, "{}", render_row(headers))?;
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        writeln!(out, "{}", render_row(&cells))?;
    }
    Ok(())
}
