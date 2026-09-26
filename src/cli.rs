//! Command line interface. Every MCP tool has a subcommand here with the
//! same semantics. `run` is in-process so tests can drive it.

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::doc::{Doc, DocRef, PutInput};
use crate::error::StoreError;
use crate::rev::short_rev;
use crate::runner::{self, Runner};
use crate::store::{Changes, History, ListQuery, Page, SearchPage, Store};
use crate::sync::{self, PullReport};
use crate::task::{self, Deploy, Evaluation, TaskState, TickReport, When};
use crate::{daemon, feed, markdown, mcp, resolve, rev, seed};

/// Dreams: a versioned document vault in SQLite, with a CLI and an MCP server.
#[derive(Parser)]
#[command(name = "dreams", version, about)]
pub struct Cli {
    /// Path to the SQLite database file.
    #[arg(long, global = true, default_value = "vault.db", env = "DREAMS_DB")]
    db: PathBuf,
    /// Print JSON (the same structures the MCP tools return) instead of human output.
    #[arg(long, global = true)]
    json: bool,
    /// Record this name as the writer of every revision. A scheduled task's
    /// agent runs with its task id here, so the task does not wake itself.
    #[arg(long, global = true, env = "DREAMS_ACTOR")]
    actor: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the database if needed, apply migrations, and seed the built-in schemas, runners, skills, and prompts.
    Init,
    /// Seed the built-in schemas, runners, skills, and prompts. Writes each one whose current
    /// revision differs from the default, and revives deleted ones. Earlier revisions stay in history.
    RestoreDefaults,
    /// Serve MCP (2026-07-28, stateless) over stdio. Runner, run, and seeded schema documents are read-only.
    Serve,
    /// Print how a host starts `serve` on this vault, as the JSON of one `mcpServers` entry.
    /// Paths are absolute. For example: claude mcp add-json dreams "$(dreams mcp-json)"
    McpJson,
    /// Documents. A schema is a document too: put one, then reference it as `_type: doc://<id>`.
    #[command(subcommand)]
    Doc(DocCmd),
    /// Scheduled agent tasks (documents typed doc://schemas/task).
    #[command(subcommand)]
    Task(TaskCmd),
    /// Agent commands that tasks run (documents typed doc://schemas/runner). Never writable over MCP.
    #[command(subcommand)]
    Runner(RunnerCmd),
    /// Feeds to pull (documents typed doc://schemas/feed). A pull writes new items and prints them.
    #[command(subcommand)]
    Feed(FeedCmd),
    /// One scheduler pass: fire every due task, then exit.
    Tick {
        /// Evaluate as if it were this time (store format, for example 2026-09-23T10:00:00.000Z).
        #[arg(long, hide = true)]
        now: Option<String>,
    },
    /// Run the scheduler until stopped. Ticks every --interval, and sooner when the database changes.
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonCmd>,
        /// Longest wait between ticks.
        #[arg(long, default_value = "60s")]
        interval: String,
        /// How often to look for changes between ticks.
        #[arg(long, default_value = "2s")]
        poll: String,
    },
    /// Write current documents as files at <dir>/<_id>. The _id extension picks the
    /// format: .json is JSON, .yaml/.yml is YAML, and anything else is Markdown.
    Export {
        dir: PathBuf,
        /// Only documents of this type: a doc:// reference, matching every pinned revision unless it has ?rev=.
        #[arg(long = "type")]
        type_id: Option<String>,
        /// Only documents whose current revision has this tag.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Import every .md, .markdown, .json, .yaml, and .yml file under <dir>. The relative
    /// path is the _id (an _id in the file that differs is ignored). Unchanged exported
    /// files are no-ops; edited files become the next revision.
    Import { dir: PathBuf },
    /// Copy every revision of the vault at PEER that this vault does not have.
    /// A task that arrives is dormant here until `task enable`. Concurrent edits
    /// become conflicts: see `_conflicts` in `doc get` and `doc resolve`.
    Pull { peer: PathBuf },
    /// Pull from the vault at PEER, then push to it. Afterwards both hold the same revisions.
    Sync { peer: PathBuf },
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
    /// Only documents of this type: a doc:// reference, matching every pinned revision unless it has ?rev=.
    #[arg(long = "type")]
    type_id: Option<String>,
    /// Only documents whose current revision has this tag.
    #[arg(long)]
    tag: Option<String>,
    /// Only documents whose _id starts with this text.
    #[arg(long)]
    prefix: Option<String>,
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
    /// Print a document by id or doc:// reference. Pin a revision with doc://<id>?rev=<rev>.
    Get {
        href: String,
        /// Also list tombstoned leaves other than the winner, as _deleted_conflicts.
        #[arg(long)]
        deleted_conflicts: bool,
    },
    /// List documents with conflicts, in id order.
    Conflicts {
        /// Page size (1..=1000, default 50).
        #[arg(long)]
        limit: Option<usize>,
        /// Cursor: the previous page's `next`.
        #[arg(long)]
        after: Option<String>,
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
    /// Resolve conflicts: write FILE (if given) on the winning revision, then
    /// tombstone every other live leaf listed in `_conflicts`. Without FILE the
    /// winner's content stays. FILE may be `doc get` output, edited.
    /// With --auto, an agent reads every conflicting revision and writes the merge.
    Resolve {
        id: String,
        /// The merged document. `-` means stdin. Absent keeps the winner.
        file: Option<PathBuf>,
        #[arg(long, value_enum)]
        format: Option<Format>,
        /// Ask an agent to merge the winner and every conflict, then write the merge.
        #[arg(long, conflicts_with = "file")]
        auto: bool,
        /// The runner that merges: runners/claude or doc://runners/claude.
        #[arg(long, requires = "auto", default_value = resolve::DEFAULT_RUNNER)]
        runner: String,
        /// Print the agent's merge as input for `doc resolve <id> FILE`, and write nothing.
        #[arg(long, requires = "auto")]
        dry_run: bool,
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
enum TaskCmd {
    /// Create or replace a task. The prompt comes from PROMPT_FILE, `-`, or stdin.
    Add {
        /// Task id, for example tasks/triage-inbox.
        task_id: String,
        /// The runner document: runners/claude or doc://runners/claude. Pin a revision with ?rev=.
        #[arg(long)]
        runner: String,
        /// Interval between runs: 30s, 15m, 2h, 1d, 1w.
        #[arg(long)]
        every: String,
        /// Only fire when a document whose _id matches this GLOB changed.
        #[arg(long)]
        glob: Option<String>,
        /// Only fire when a document with this tag changed.
        #[arg(long)]
        tag: Option<String>,
        /// Only fire when a document of this type changed (a doc:// reference).
        #[arg(long = "type")]
        type_id: Option<String>,
        /// Only fire when this document changed. Repeatable.
        #[arg(long = "id", value_name = "DOC_ID")]
        ids: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        /// The folder the agent runs in. A relative DIR is relative to the vault's folder.
        /// Default: workspace.
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,
        /// Write the task but do not deploy it on this vault.
        #[arg(long)]
        no_deploy: bool,
        /// Deploy without asking for confirmation.
        #[arg(long)]
        yes: bool,
        /// Prompt text file. `-` or absent means stdin.
        prompt_file: Option<PathBuf>,
    },
    /// Every task with its state on this vault, its last run, and whether it is due.
    List,
    /// Run the current revision of a task, and of its runner, on this vault. Without an id,
    /// every task that is not deployed at its current revisions. Asks for confirmation.
    /// Edits and tasks that arrive by sync run only after a deploy.
    Deploy {
        task_id: Option<String>,
        /// Deploy without asking for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Stop running a task on this vault. Other vaults are not affected. `task deploy` starts it again.
    Disable { task_id: String },
    /// Delete a task (a tombstone; its runs stay).
    Rm { task_id: String },
    /// Evaluate one task: due or not, matched changes, the command it would run. Nothing runs.
    Check {
        task_id: String,
        #[arg(long, hide = true)]
        now: Option<String>,
    },
    /// Fire one task now, whatever its schedule says. A dormant task stays dormant.
    Run {
        task_id: String,
        /// Fire even if another run holds the task.
        #[arg(long)]
        force: bool,
    },
    /// Past runs of a task, newest first.
    Runs {
        task_id: String,
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
enum RunnerCmd {
    /// Create or replace a runner. The command follows `--` and is spawned without a shell.
    /// Tokens {db} {task} {run} {mcp} {out} {exe} are replaced inside each argument.
    Add {
        /// Runner id, for example runners/claude.
        runner_id: String,
        #[arg(long)]
        title: Option<String>,
        /// Kill the command after this long. Default 10m.
        #[arg(long)]
        timeout: Option<String>,
        /// The command and its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true, num_args = 1..)]
        argv: Vec<String>,
    },
    /// Every runner.
    List,
    /// Delete a runner.
    Rm { runner_id: String },
}

#[derive(Subcommand)]
enum FeedCmd {
    /// Create or update a feed. A pull writes its items under the feed's id without `.md`.
    /// On an existing feed, fields that are not given stay as they are.
    Add {
        url: String,
        /// Feed id. Default: feeds/<origin-slug>.md, for example feeds/example-com.md.
        #[arg(long)]
        id: Option<String>,
        /// rss reads RSS or Atom, one item per entry. html reads one page as text. Default for a new feed: rss.
        #[arg(long, value_parser = ["rss", "html"])]
        kind: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Instructions for the agent that processes the items, for example to correct
        /// for a known bias of the source. An empty TEXT removes them.
        #[arg(long, value_name = "TEXT")]
        instructions: Option<String>,
    },
    /// Every feed.
    List,
    /// Fetch one feed, or every feed. Writes the items not seen before and prints them.
    /// Exits 1 when a feed failed; the other feeds are still pulled.
    Pull { feed_id: Option<String> },
    /// Delete a feed (a tombstone; its items stay).
    Rm { feed_id: String },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start the daemon at login and keep it running (launchd on macOS, systemd on Linux).
    Install,
    /// Stop the daemon and remove its service.
    Uninstall,
}

// ---- entry point --------------------------------------------------------

/// Asks a person to confirm `text`: true for yes.
pub type Confirm<'a> = &'a mut dyn FnMut(&str) -> io::Result<bool>;

/// Ask on the controlling terminal, not on stdin, which can hold a task prompt.
pub fn tty_confirm(text: &str) -> io::Result<bool> {
    let mut tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    write!(tty, "{text}\nDeploy? [y/N] ")?;
    tty.flush()?;
    let mut line = String::new();
    io::BufReader::new(&tty).read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// Parse `args` (including argv[0]) and run. Returns the exit code. All
/// output goes to the given writers; nothing here exits the process.
/// `confirm` asks before a deploy.
pub fn run<I, T>(args: I, stdin: &mut dyn Read, stdout: &mut dyn Write, stderr: &mut dyn Write, confirm: Confirm) -> i32
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
    match execute(cli, stdin, stdout, confirm) {
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

fn execute(cli: Cli, stdin: &mut dyn Read, out: &mut dyn Write, confirm: Confirm) -> anyhow::Result<()> {
    let json = cli.json;
    // Every entry point seeds a database it creates, before the actor is set
    // so the built-in documents never carry a task's id. The library never
    // writes on open.
    let open = || -> Result<Store, StoreError> {
        let created = !cli.db.exists();
        let mut store = Store::open(&cli.db)?;
        if created {
            seed::seed(&mut store)?;
        }
        store.set_actor(cli.actor.clone());
        Ok(store)
    };
    match cli.command {
        Command::Init => {
            let mut store = Store::open(&cli.db)?;
            let seeded = seed::seed(&mut store)?;
            writeln!(out, "initialized {}", cli.db.display())?;
            for id in seeded {
                writeln!(out, "seeded {id}")?;
            }
        }
        Command::RestoreDefaults => {
            let mut store = Store::open(&cli.db)?;
            let seeded = seed::seed(&mut store)?;
            if json {
                writeln!(out, "{}", serde_json::json!({ "seeded": seeded }))?;
            } else if seeded.is_empty() {
                writeln!(out, "nothing to seed")?;
            } else {
                for id in seeded {
                    writeln!(out, "seeded {id}")?;
                }
            }
        }
        Command::Serve => {
            let mut store = open()?;
            store.set_protected(runner::PROTECTED_TYPES, runner::PROTECTED_IDS);
            tokio::runtime::Runtime::new()?.block_on(mcp::serve(store))?;
        }
        Command::McpJson => {
            let exe = std::env::current_exe()?;
            let db = absolute(&cli.db)?;
            writeln!(out, "{}", mcp::server_entry(&exe, &db, cli.actor.as_deref()))?;
        }
        Command::Doc(cmd) => {
            let mut store = open()?;
            let db = absolute(&cli.db)?;
            doc_cmd(&mut store, &db, cmd, json, stdin, out)?;
        }
        Command::Export { dir, type_id, tag } => {
            let store = open()?;
            export(&store, &dir, ListQuery { type_id, tag, ..Default::default() }, json, out)?;
        }
        Command::Import { dir } => {
            let mut store = open()?;
            import(&mut store, &dir, json, out)?;
        }
        Command::Pull { peer } => {
            let mut store = open()?;
            let (_, there) = peer_paths(&cli.db, &peer)?;
            let source = Store::open(&peer)?;
            let report = sync::pull(&mut store, &source, &there)?;
            print_pulls(out, json, &[(format!("from {there}"), report)])?;
        }
        Command::Sync { peer } => {
            let mut store = open()?;
            let (here, there) = peer_paths(&cli.db, &peer)?;
            let mut other = Store::open(&peer)?;
            let [into_here, into_there] = sync::sync(&mut store, &here, &mut other, &there)?;
            print_pulls(out, json, &[(format!("from {there}"), into_here), (format!("to {there}"), into_there)])?;
        }
        Command::Task(cmd) => {
            let mut store = open()?;
            let db = absolute(&cli.db)?;
            task_cmd(&mut store, &db, cmd, json, stdin, out, confirm)?;
        }
        Command::Runner(cmd) => {
            let mut store = open()?;
            runner_cmd(&mut store, cmd, json, out)?;
        }
        Command::Feed(cmd) => {
            let mut store = open()?;
            feed_cmd(&mut store, cmd, json, out)?;
        }
        Command::Tick { now } => {
            let mut store = open()?;
            let db = absolute(&cli.db)?;
            let now = match now {
                Some(n) => n,
                None => store.now()?,
            };
            let report = tokio::runtime::Runtime::new()?.block_on(task::tick(&mut store, &db, &now))?;
            print_tick(out, json, &report)?;
        }
        Command::Daemon { action, interval, poll } => {
            let db = absolute(&cli.db)?;
            match action {
                Some(DaemonCmd::Install) => {
                    // Create and migrate first, so the daemon finds a database.
                    let id = open()?.vault_id()?;
                    let exe = std::env::current_exe()?;
                    daemon::install(&db, &id, &exe, out)?;
                }
                Some(DaemonCmd::Uninstall) => {
                    // A deleted vault has no id; its service is found by path. Opening would create the file.
                    let id = if db.exists() { Some(Store::open(&db)?.vault_id()?) } else { None };
                    daemon::uninstall(&db, id.as_deref(), out)?
                }
                None => {
                    let mut store = open()?;
                    let interval = std::time::Duration::from_secs(task::parse_duration(&interval)?);
                    let poll = std::time::Duration::from_secs(task::parse_duration(&poll)?);
                    install_tracing("info");
                    tokio::runtime::Runtime::new()?.block_on(daemon::run(&mut store, &db, interval, poll))?;
                }
            }
        }
    }
    Ok(())
}

/// Canonical paths of this vault and the peer, which must already exist and
/// differ. The peer's path is the key of this vault's checkpoint for it.
fn peer_paths(db: &Path, peer: &Path) -> Result<(String, String), StoreError> {
    let canonical = |p: &Path| {
        std::fs::canonicalize(p)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| StoreError::invalid(format!("no vault at {}: {e}", p.display())))
    };
    let here = canonical(db)?;
    let there = canonical(peer)?;
    if here == there {
        return Err(StoreError::invalid(format!("{there} is this vault")));
    }
    Ok((here, there))
}

/// Each report with its direction: `from <peer>` for a pull, `to <peer>` for a push.
fn print_pulls(out: &mut dyn Write, json: bool, reports: &[(String, PullReport)]) -> io::Result<()> {
    if json {
        return match reports {
            [(_, r)] => print_json(out, r),
            _ => print_json(out, &reports.iter().map(|(_, r)| r).collect::<Vec<_>>()),
        };
    }
    for (direction, r) in reports {
        let verb = if direction.starts_with("to ") { "pushed" } else { "pulled" };
        let noun = if r.written == 1 { "revision" } else { "revisions" };
        let mut notes = vec![format!("{} present", r.present)];
        if r.missing_parent > 0 {
            notes.push(format!("{} missing parent", r.missing_parent));
        }
        if r.restarted {
            notes.push("checkpoint reset".into());
        }
        writeln!(out, "{verb} {} {noun} {direction} ({})", r.written, notes.join(", "))?;
    }
    Ok(())
}

/// Logs to stderr, filtered by `RUST_LOG`, or by `default` when it is unset.
/// Shared by `serve` and `daemon`.
pub fn install_tracing(default: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(false).try_init();
}

fn absolute(path: &Path) -> Result<PathBuf, StoreError> {
    std::path::absolute(path).map_err(|e| StoreError::invalid(format!("{}: {e}", path.display())))
}

// ---- tasks and runners ----------------------------------------------------

fn read_text(file: Option<&Path>, stdin: &mut dyn Read) -> Result<String, StoreError> {
    match file.filter(|p| *p != Path::new("-")) {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| StoreError::invalid(format!("reading {}: {e}", path.display())))
        }
        None => {
            let mut text = String::new();
            stdin.read_to_string(&mut text).map_err(|e| StoreError::invalid(format!("reading stdin: {e}")))?;
            Ok(text)
        }
    }
}

fn task_cmd(
    store: &mut Store,
    db: &Path,
    cmd: TaskCmd,
    json: bool,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    confirm: Confirm,
) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::Add { task_id, runner, every, glob, tag, type_id, ids, title, cwd, no_deploy, yes, prompt_file } => {
            id_to_relpath(&task_id)?;
            task::parse_duration(&every)?;
            let runner = DocRef::from_cli(&runner)?.to_string();
            Runner::get(store, &runner)?;
            let prompt = read_text(prompt_file.as_deref(), stdin)?;
            let when = When { glob, tag, type_id, ids: if ids.is_empty() { None } else { Some(ids) } };
            let mut map = Map::new();
            map.insert("_type".into(), Value::String(task::TASK_TYPE.into()));
            map.insert("runner".into(), Value::String(runner));
            map.insert("every".into(), Value::String(every));
            map.insert("prompt".into(), Value::String(prompt));
            if when != When::default() {
                map.insert("when".into(), serde_json::to_value(&when)?);
            }
            if let Some(t) = title {
                map.insert("title".into(), Value::String(t));
            }
            if let Some(c) = cwd {
                map.insert("cwd".into(), Value::String(c));
            }
            let (doc, status) = upsert_unless_same(store, &task_id, map)?;
            if !json {
                writeln!(out, "{} {} {}", format!("{status:?}").to_lowercase(), doc.id, short_rev(&doc.rev))?;
            }
            let deployed = if no_deploy {
                Some(Vec::new())
            } else {
                deploy(store, task::plan_deploy(store, Some(&task_id))?, yes, confirm)?
            };
            if json {
                print_json(out, &doc)?;
            } else {
                match (deployed, task::state(store, &task_id)?) {
                    (Some(states), _) => print_deployed(out, &states)?,
                    (None, Some(s)) if s.enabled => {
                        writeln!(out, "not deployed; this vault still runs {task_id} {}", short_rev(&s.task_rev))?
                    }
                    (None, _) => writeln!(out, "not deployed; {task_id} does not run on this vault")?,
                }
            }
        }
        TaskCmd::List => {
            let now = store.now()?;
            let evals = task::evaluate(store, &now, None)?;
            if json {
                print_json(out, &evals)?;
            } else {
                let rows: Vec<Vec<String>> = evals
                    .iter()
                    .map(|e| {
                        vec![
                            e.task.id.clone(),
                            state_word(e),
                            e.parsed.runner.clone(),
                            e.task.str_field("every").unwrap_or_default().to_string(),
                            e.parsed.when.as_ref().map(When::summary).unwrap_or_default(),
                            e.last_run_at.clone().unwrap_or_default(),
                            due_word(e).to_string(),
                        ]
                    })
                    .collect();
                table(out, &["ID", "STATE", "RUNNER", "EVERY", "WHEN", "LAST RUN", "DUE"], &rows)?;
            }
        }
        TaskCmd::Deploy { task_id, yes } => {
            let plan = task::plan_deploy(store, task_id.as_deref())?;
            if plan.is_empty() && !json {
                writeln!(out, "nothing to deploy")?;
                return Ok(());
            }
            let states = deploy(store, plan, yes, confirm)?.ok_or_else(|| StoreError::invalid("not deployed"))?;
            if json {
                print_json(out, &states)?;
            } else {
                print_deployed(out, &states)?;
            }
        }
        TaskCmd::Disable { task_id } => {
            let state = task::disable(store, &task_id)?;
            if json {
                print_json(out, &state)?;
            } else {
                writeln!(out, "disabled {task_id}")?;
            }
        }
        TaskCmd::Rm { task_id } => {
            let parent = head_rev(store, &task_id)?;
            let doc = store.get_rev(&parent)?;
            if doc.type_path() != Some(task::TASK_TYPE) {
                return Err(StoreError::invalid(format!("{task_id} is not a {} document", task::TASK_TYPE)).into());
            }
            let tomb = store.delete(&task_id, &parent)?;
            if json {
                print_json(out, &tomb)?;
            } else {
                writeln!(out, "removed {task_id}")?;
            }
        }
        TaskCmd::Check { task_id, now } => {
            let now = match now {
                Some(n) => n,
                None => store.now()?,
            };
            let eval = task::evaluate(store, &now, Some(&task_id))?.remove(0);
            let argv = match &eval.runner {
                Ok(r) => r.argv.clone(),
                Err(e) => vec![format!("(runner error: {e})")],
            };
            let cwd = task::work_dir(db, eval.parsed.cwd.as_deref());
            if json {
                let mut v = serde_json::to_value(&eval)?;
                v["argv"] = json_array(&argv);
                v["cwd"] = Value::String(cwd.to_string_lossy().into_owned());
                v["prompt"] = Value::String(task::prompt_text(&eval));
                print_json(out, &v)?;
            } else {
                writeln!(out, "task:      {}", eval.task.id)?;
                writeln!(out, "runner:    {}", eval.parsed.runner)?;
                writeln!(out, "every:     {}", eval.task.str_field("every").unwrap_or_default())?;
                if let Some(w) = &eval.parsed.when {
                    writeln!(out, "when:      {}", w.summary())?;
                }
                writeln!(out, "state:     {}", state_word(&eval))?;
                writeln!(out, "runs:      {}", short_rev(&eval.task.rev))?;
                if eval.head_rev != eval.task.rev {
                    writeln!(out, "head:      {}", short_rev(&eval.head_rev))?;
                }
                writeln!(out, "last run:  {}", eval.last_run_at.as_deref().unwrap_or("never"))?;
                writeln!(out, "cursor:    {} (head {})", eval.cursor, eval.head)?;
                writeln!(out, "status:    {}", due_word(&eval))?;
                if !eval.changes.is_empty() {
                    writeln!(out, "\nchanges since cursor:")?;
                    let rows: Vec<Vec<String>> = eval
                        .changes
                        .iter()
                        .map(|c| {
                            vec![
                                c.seq.to_string(),
                                c.id.clone(),
                                short_rev(&c.rev),
                                if c.deleted { "yes".into() } else { String::new() },
                            ]
                        })
                        .collect();
                    table(out, &["SEQ", "ID", "REV", "DELETED"], &rows)?;
                }
                writeln!(out, "\ncwd:       {}", cwd.display())?;
                writeln!(out, "command:   {}", argv.join(" "))?;
            }
        }
        TaskCmd::Run { task_id, force } => {
            let now = store.now()?;
            let eval = task::evaluate(store, &now, Some(&task_id))?.remove(0);
            if eval.running && !force {
                let until = eval.state.as_ref().and_then(|s| s.lease_until.as_deref()).unwrap_or("?");
                return Err(StoreError::invalid(format!(
                    "{task_id} is running (lease until {until}); pass --force to fire anyway"
                ))
                .into());
            }
            let fired = tokio::runtime::Runtime::new()?
                .block_on(task::fire(store, db, &eval, &now, force))?
                .ok_or_else(|| StoreError::invalid(format!("{task_id} was fired by another process; try again")))?;
            let run = store.get(&fired.run)?;
            if json {
                print_json(out, &run)?;
            } else {
                write!(out, "{}", markdown::render(&run))?;
            }
            if fired.error.is_some() {
                return Err(StoreError::invalid(format!(
                    "run {} failed: {}",
                    fired.run,
                    fired.error.unwrap_or_default()
                ))
                .into());
            }
        }
        TaskCmd::Runs { task_id, limit } => {
            let q = ListQuery { type_id: Some(task::RUN_TYPE.into()), tag: Some(task_id), limit, ..Default::default() };
            let page = store.list(&q)?;
            if json {
                print_json(out, &page)?;
            } else {
                let field = |d: &Doc, k: &str| d.str_field(k).unwrap_or_default().to_string();
                let rows: Vec<Vec<String>> = page
                    .docs
                    .iter()
                    .map(|d| {
                        vec![
                            field(d, "started_at"),
                            field(d, "finished_at"),
                            d.body.get("exit_code").and_then(Value::as_i64).map(|c| c.to_string()).unwrap_or_default(),
                            field(d, "vault"),
                            clip(&field(d, "error"), 60),
                        ]
                    })
                    .collect();
                table(out, &["STARTED", "FINISHED", "EXIT", "VAULT", "ERROR"], &rows)?;
                if let Some(next) = page.next {
                    writeln!(out, "next: {next}")?;
                }
            }
        }
    }
    Ok(())
}

/// `upsert`, but a map identical to the current revision writes nothing.
/// Built for `task add` and `runner add`, whose inputs never carry `_rev`.
fn upsert_unless_same(store: &mut Store, id: &str, map: Map<String, Value>) -> Result<(Doc, WriteStatus), StoreError> {
    if let Ok(head) = store.get(id) {
        let same_type = head.type_path() == map.get("_type").and_then(Value::as_str).map(DocRef::path_of);
        let body: Map<String, Value> =
            map.iter().filter(|(k, _)| !k.starts_with('_')).map(|(k, v)| (k.clone(), v.clone())).collect();
        if same_type && body == head.body {
            return Ok((head, WriteStatus::Unchanged));
        }
    }
    upsert(store, id, map, None)
}

/// Confirm a plan (unless `yes`) and apply it. `None` when the person said no.
fn deploy(store: &mut Store, plan: Vec<Deploy>, yes: bool, confirm: Confirm) -> anyhow::Result<Option<Vec<TaskState>>> {
    if plan.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if !yes {
        let text = plan.iter().map(Deploy::describe).collect::<Vec<_>>().join("\n");
        let answer = confirm(&text).map_err(|e| {
            StoreError::invalid(format!("cannot ask for confirmation ({e}); pass --yes to deploy without asking"))
        })?;
        if !answer {
            return Ok(None);
        }
    }
    Ok(Some(task::apply_deploy(store, &plan)?))
}

fn print_deployed(out: &mut dyn Write, states: &[TaskState]) -> io::Result<()> {
    for s in states {
        writeln!(out, "deployed {} {}", s.task_id, short_rev(&s.task_rev))?;
    }
    Ok(())
}

/// A task's state on this vault, and whether a deploy would change it.
fn state_word(e: &Evaluation) -> String {
    let word = match &e.state {
        None => "dormant",
        Some(s) if s.enabled => "deployed",
        Some(_) => "disabled",
    };
    if e.drift { format!("{word}, changed") } else { word.to_string() }
}

fn due_word(e: &Evaluation) -> &'static str {
    if !e.state.as_ref().is_some_and(|s| s.enabled) {
        ""
    } else if e.running {
        "running"
    } else if e.due {
        "due"
    } else if e.time_due {
        "waiting for changes"
    } else {
        ""
    }
}

fn json_array(items: &[String]) -> Value {
    Value::Array(items.iter().map(|s| Value::String(s.clone())).collect())
}

fn runner_cmd(store: &mut Store, cmd: RunnerCmd, json: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    match cmd {
        RunnerCmd::Add { runner_id, title, timeout, argv } => {
            let mut map = Map::new();
            map.insert("_type".into(), Value::String(runner::RUNNER_TYPE.into()));
            map.insert("argv".into(), json_array(&argv));
            if let Some(t) = title {
                map.insert("title".into(), Value::String(t));
            }
            if let Some(t) = timeout {
                task::parse_duration(&t)?;
                map.insert("timeout".into(), Value::String(t));
            }
            let (doc, status) = upsert_unless_same(store, &runner_id, map)?;
            if json {
                print_json(out, &doc)?;
            } else {
                writeln!(out, "{} {} {}", format!("{status:?}").to_lowercase(), doc.id, short_rev(&doc.rev))?;
            }
        }
        RunnerCmd::List => {
            let page = store.list(&ListQuery {
                type_id: Some(runner::RUNNER_TYPE.into()),
                limit: Some(1000),
                ..Default::default()
            })?;
            if json {
                print_json(out, &page)?;
            } else {
                let mut docs = page.docs;
                docs.sort_by(|a, b| a.id.cmp(&b.id));
                let rows: Vec<Vec<String>> = docs
                    .iter()
                    .map(|d| {
                        let argv = d
                            .body
                            .get("argv")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
                            .unwrap_or_default();
                        vec![
                            d.id.clone(),
                            title_of(d),
                            d.str_field("timeout").unwrap_or(runner::DEFAULT_TIMEOUT).to_string(),
                            clip(&argv, 80),
                        ]
                    })
                    .collect();
                table(out, &["ID", "TITLE", "TIMEOUT", "ARGV"], &rows)?;
            }
        }
        RunnerCmd::Rm { runner_id } => {
            let parent = head_rev(store, &runner_id)?;
            let doc = store.get_rev(&parent)?;
            if doc.type_path() != Some(runner::RUNNER_TYPE) {
                return Err(
                    StoreError::invalid(format!("{runner_id} is not a {} document", runner::RUNNER_TYPE)).into()
                );
            }
            let tomb = store.delete(&runner_id, &parent)?;
            if json {
                print_json(out, &tomb)?;
            } else {
                writeln!(out, "removed {runner_id}")?;
            }
        }
    }
    Ok(())
}

fn feed_cmd(store: &mut Store, cmd: FeedCmd, json: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    match cmd {
        FeedCmd::Add { url, id, kind, title, instructions } => {
            let id = match id {
                Some(id) => id,
                None => feed::default_id(&url)?,
            };
            // An existing feed keeps the fields that are not given.
            let mut map = Map::new();
            if let Ok(head) = store.get(&id) {
                let old = feed::Feed::from_doc(&head)
                    .map_err(|_| StoreError::invalid(format!("{id} exists and is not a feed; pass another --id")))?;
                if old.url != url {
                    return Err(StoreError::invalid(format!(
                        "{id} is the feed for {}; pass --id to add another feed",
                        old.url
                    ))
                    .into());
                }
                map = head.body;
            }
            map.insert("_type".into(), Value::String(feed::FEED_TYPE.into()));
            map.insert("url".into(), Value::String(url));
            match kind {
                Some(k) => {
                    map.insert("kind".into(), Value::String(k));
                }
                None => {
                    map.entry("kind").or_insert_with(|| Value::String("rss".into()));
                }
            }
            if let Some(t) = title {
                map.insert("title".into(), Value::String(t));
            }
            match instructions {
                Some(i) if i.is_empty() => {
                    map.remove("instructions");
                }
                Some(i) => {
                    map.insert("instructions".into(), Value::String(i));
                }
                None => {}
            }
            let (doc, status) = upsert_unless_same(store, &id, map)?;
            if json {
                print_json(out, &doc)?;
            } else {
                writeln!(out, "{} {} {}", format!("{status:?}").to_lowercase(), doc.id, short_rev(&doc.rev))?;
            }
        }
        FeedCmd::List => {
            let mut docs = store.list_all(Some(feed::FEED_TYPE))?;
            docs.sort_by(|a, b| a.id.cmp(&b.id));
            if json {
                print_json(out, &docs)?;
            } else {
                let text = |d: &Doc, key: &str| d.str_field(key).unwrap_or_default().to_string();
                let rows: Vec<Vec<String>> =
                    docs.iter().map(|d| vec![d.id.clone(), text(d, "kind"), text(d, "url"), title_of(d)]).collect();
                table(out, &["ID", "KIND", "URL", "TITLE"], &rows)?;
            }
        }
        FeedCmd::Pull { feed_id } => {
            let report = feed::pull(store, feed_id.as_deref())?;
            if json {
                print_json(out, &report)?;
            } else {
                if report.feeds.is_empty() {
                    writeln!(out, "no new items")?;
                } else {
                    let rows: Vec<Vec<String>> = report
                        .feeds
                        .iter()
                        .flat_map(|f| {
                            let feed = f.feed.trim_start_matches(DocRef::SCHEME).to_string();
                            f.items.iter().map(move |i| vec![feed.clone(), i.href.clone(), clip(&i.title, 60)])
                        })
                        .collect();
                    table(out, &["FEED", "HREF", "TITLE"], &rows)?;
                }
                for e in &report.errors {
                    writeln!(out, "failed {}: {}", e.feed, e.error)?;
                }
            }
            if !report.errors.is_empty() {
                anyhow::bail!("{} feed(s) failed", report.errors.len());
            }
        }
        FeedCmd::Rm { feed_id } => {
            let parent = head_rev(store, &feed_id)?;
            feed::Feed::from_doc(&store.get_rev(&parent)?)?;
            let tomb = store.delete(&feed_id, &parent)?;
            if json {
                print_json(out, &tomb)?;
            } else {
                writeln!(out, "removed {feed_id}")?;
            }
        }
    }
    Ok(())
}

fn print_tick(out: &mut dyn Write, json: bool, report: &TickReport) -> io::Result<()> {
    if json {
        return print_json(out, report);
    }
    for f in &report.fired {
        match &f.error {
            Some(e) => writeln!(out, "fired  {} -> {} error: {e}", f.task, f.run)?,
            None => writeln!(out, "fired  {} -> {} exit {}", f.task, f.run, f.exit_code.unwrap_or(-1))?,
        }
    }
    for id in &report.skipped {
        writeln!(out, "skipped {id} (claimed elsewhere)")?;
    }
    for (id, e) in &report.errors {
        writeln!(out, "error  {id}: {e}")?;
    }
    writeln!(out, "{} fired, {} skipped, {} errors", report.fired.len(), report.skipped.len(), report.errors.len())
}

fn doc_cmd(
    store: &mut Store,
    db: &Path,
    cmd: DocCmd,
    json: bool,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
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
        DocCmd::Get { href, deleted_conflicts } => {
            print_doc(out, json, &store.get_href(&href, deleted_conflicts)?)?;
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
            print_search(out, json, &page)?;
        }
        DocCmd::Resolve { id, auto: true, runner, dry_run, .. } => {
            let proposal = tokio::runtime::Runtime::new()?.block_on(resolve::propose(store, db, &id, &runner))?;
            let Some(p) = proposal else {
                print_doc(out, json, &store.get(&id)?)?;
                return Ok(());
            };
            if dry_run {
                if json {
                    print_json(out, &p.merged)?;
                } else {
                    write!(out, "{}", markdown::render_input(&p.merged))?;
                }
                return Ok(());
            }
            // The merge records which agent wrote it, unless --actor named someone.
            let previous = store.actor().map(str::to_string);
            if previous.is_none() {
                store.set_actor(Some(p.runner.clone()));
            }
            let result = store.resolve(&id, Some(p.merged), Some(&p.conflicts));
            store.set_actor(previous);
            print_doc(out, json, &result?)?;
        }
        DocCmd::Resolve { id, file, format, .. } => {
            let merged = match file {
                None => None,
                Some(file) => {
                    let map = read_input(&Input { file: Some(file), format }, stdin)?;
                    let prepared = prepare_write(map, WriteMode::Update)?;
                    if prepared.unchanged { None } else { Some(prepared.input) }
                }
            };
            let doc = store.resolve(&id, merged, None)?;
            print_doc(out, json, &doc)?;
        }
        DocCmd::Conflicts { limit, after } => {
            let page = store.conflicted(after.as_deref(), limit)?;
            if json {
                print_json(out, &page)?;
            } else {
                let rows: Vec<Vec<String>> = page
                    .docs
                    .iter()
                    .map(|d| vec![d.id.clone(), short_rev(&d.rev), d.conflicts.len().to_string()])
                    .collect();
                table(out, &["ID", "WINNER", "CONFLICTS"], &rows)?;
                if let Some(next) = &page.next {
                    writeln!(out, "next: {next}")?;
                }
            }
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

impl From<Filter> for ListQuery {
    fn from(f: Filter) -> Self {
        ListQuery { type_id: f.type_id, tag: f.tag, prefix: f.prefix, before: None, limit: f.limit }
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
/// components joined with `/`, extension included. `None` unless the
/// extension names a format.
pub fn relpath_to_id(rel: &Path) -> Option<String> {
    detect_format(rel)?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    Some(parts.join("/"))
}

fn walk_docs(dir: &Path, root: &Path, files: &mut Vec<PathBuf>) -> Result<(), StoreError> {
    let entries = std::fs::read_dir(dir).map_err(|e| StoreError::invalid(format!("reading {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::invalid(format!("reading {}: {e}", dir.display())))?;
        let path = entry.path();
        if path.is_dir() {
            walk_docs(&path, root, files)?;
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
            writeln!(out, "{} created, {} updated, {} unchanged, {errors} errors", counts[0], counts[1], counts[2])?;
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
                    results.push(FileResult {
                        path: doc.id.clone(),
                        id: Some(doc.id.clone()),
                        rev: None,
                        status: None,
                        error: Some(e.to_string()),
                    });
                    continue;
                }
            };
            let path = dir.join(&rel);
            let written = path
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|_| std::fs::write(&path, render_doc(doc)));
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
    walk_docs(dir, dir, &mut files)?;
    files.sort();
    let mut results = Vec::new();
    for path in files {
        let rel = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
        let rel_text = rel.to_string_lossy().into_owned();
        let id = relpath_to_id(&rel).expect("walk_docs only collects files with a format");
        let format = detect_format(&rel).expect("walk_docs only collects files with a format");
        let outcome = std::fs::read_to_string(&path)
            .map_err(|e| StoreError::invalid(format!("reading {}: {e}", path.display())))
            .and_then(|text| parse_input(&text, format))
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
            Ok((doc, status)) => {
                FileResult { path: rel_text, id: Some(id), rev: Some(doc.rev), status: Some(status), error: None }
            }
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
        for ext in ["md", "markdown", "json", "yaml", "yml"] {
            let rel = format!("notes/a.{ext}");
            assert_eq!(relpath_to_id(Path::new(&rel)), Some(rel.clone()));
        }
        assert_eq!(relpath_to_id(Path::new("A.MD")), None);
        assert_eq!(relpath_to_id(Path::new("a.JSON")), None);
        assert_eq!(relpath_to_id(Path::new("a.txt")), None);
        assert_eq!(relpath_to_id(Path::new("README")), None);
    }
}

// ---- input --------------------------------------------------------------

fn detect_format(path: &Path) -> Option<Format> {
    match path.extension()?.to_str()? {
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
            stdin.read_to_string(&mut text).map_err(|e| StoreError::invalid(format!("reading stdin: {e}")))?;
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
    map.remove("_actor");
    map.remove("_seq");
    map.remove("_conflicts");
    map.remove("_deleted_conflicts");
    if let Some(deleted) = map.remove("_deleted")
        && deleted.as_bool() == Some(true)
        && mode == WriteMode::Put
    {
        return Err(StoreError::invalid("input is a tombstone; use `doc update` to revive the document"));
    }

    let mut unchanged = false;
    if let Some(rev_value) = map.remove("_rev") {
        let rev_id = rev_value.as_str().ok_or_else(|| StoreError::invalid("_rev must be a string"))?.to_string();
        let id =
            map.get("_id").and_then(Value::as_str).ok_or_else(|| StoreError::invalid("input has _rev but no _id"))?;
        let parent = map.get("_parent").and_then(Value::as_str);
        let type_id = map.get("_type").and_then(Value::as_str);
        let body: Map<String, Value> =
            map.iter().filter(|(k, _)| !k.starts_with('_')).map(|(k, v)| (k.clone(), v.clone())).collect();
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
    if json { print_json(out, doc) } else { write!(out, "{}", markdown::render(doc)) }
}

/// A document in the format its `_id` extension names; Markdown by default.
fn render_doc(doc: &Doc) -> String {
    match detect_format(Path::new(&doc.id)) {
        Some(Format::Json) => format!("{}\n", serde_json::to_string_pretty(doc).expect("store types serialize")),
        Some(Format::Yaml) => serde_yaml_ng::to_string(doc).expect("store types serialize"),
        Some(Format::Md) | None => markdown::render(doc),
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
    doc.str_field("title").map(|t| clip(t, 60)).unwrap_or_default()
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
                d.type_path().unwrap_or_default().to_string(),
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

fn print_search(out: &mut dyn Write, json: bool, page: &SearchPage) -> io::Result<()> {
    if json {
        return print_json(out, page);
    }
    let rows: Vec<Vec<String>> = page
        .results
        .iter()
        .map(|r| {
            vec![
                r.id.clone(),
                short_rev(&r.rev),
                r.type_id.as_deref().map(DocRef::path_of).unwrap_or_default().to_string(),
                r.title.as_deref().map(|t| clip(t, 60)).unwrap_or_default(),
                r.content_matches.as_deref().map(|m| clip(m, 80)).unwrap_or_default(),
            ]
        })
        .collect();
    table(out, &["ID", "REV", "TYPE", "TITLE", "MATCH"], &rows)?;
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
