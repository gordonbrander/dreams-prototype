//! Scheduled tasks: documents typed `doc://schemas/task.json` that wake an agent
//! every interval, optionally only when watched documents changed. A task
//! document is a template and replicates. Each vault keeps its own schedule
//! in the local `task_state` table: a task with no row is dormant there.
//! A deploy pins the task revision and the runner revision that run, so an
//! edit, local or synced, runs only after the next deploy. Each run writes
//! a receipt, a document typed `doc://schemas/run.json`, which also replicates.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::doc::{Doc, DocRef, PutInput, new_id, stem};
use crate::error::StoreError;
use crate::rev::short_rev;
use crate::runner::{Context, Runner};
use crate::store::{DOC_COLS, Store, row_to_doc, type_filter};
use crate::text::truncate_bytes;

/// Seeded schema documents, as type paths.
pub const TASK_TYPE: &str = "doc://schemas/task.json";
pub const RUN_TYPE: &str = "doc://schemas/run.json";

/// Agent output above this is cut, and the run records the cut.
pub const MAX_CONTENT_BYTES: usize = 512 * 1024;
const MAX_STDERR_BYTES: usize = 4 * 1024;

/// `30s`, `15m`, `2h`, `1d`, `1w` to seconds. Zero is an error.
pub fn parse_duration(text: &str) -> Result<u64, StoreError> {
    let bad = || StoreError::invalid(format!("bad duration {text:?}: use a number and one of s, m, h, d, w"));
    let (digits, unit) = text.split_at(text.len().saturating_sub(1));
    let n: u64 = digits.parse().map_err(|_| bad())?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(bad()),
    };
    if n == 0 {
        return Err(StoreError::invalid("duration must be greater than zero"));
    }
    Ok(n * mult)
}

/// Seconds as the largest unit that divides them: 600 is `10m`.
pub fn format_duration(secs: u64) -> String {
    let unit = [(604_800, 'w'), (86_400, 'd'), (3600, 'h'), (60, 'm')]
        .into_iter()
        .find(|(n, _)| secs > 0 && secs.is_multiple_of(*n));
    match unit {
        Some((n, u)) => format!("{}{u}", secs / n),
        None => format!("{secs}s"),
    }
}

/// The change filter. Fields are AND-ed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct When {
    /// SQLite GLOB pattern on `_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
}

impl When {
    /// One line for tables: `tag=inbox glob=inbox/*`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(g) = &self.glob {
            parts.push(format!("glob={g}"));
        }
        if let Some(t) = &self.tag {
            parts.push(format!("tag={t}"));
        }
        if let Some(t) = &self.type_id {
            parts.push(format!("type={t}"));
        }
        if let Some(ids) = &self.ids {
            parts.push(format!("ids={}", ids.join(",")));
        }
        parts.join(" ")
    }
}

/// A typed view of a task document.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub runner: String,
    pub every_secs: u64,
    pub when: Option<When>,
    pub prompt: String,
    pub cwd: Option<String>,
}

impl Task {
    pub fn from_doc(doc: &Doc) -> Result<Task, StoreError> {
        if doc.type_path() != Some(TASK_TYPE) {
            return Err(StoreError::invalid(format!("{} is not a {TASK_TYPE} document", doc.id)));
        }
        let text = |key: &str| doc.field::<String>(key).unwrap_or_default();
        let when = match doc.body.get("when") {
            Some(v) => Some(serde_json::from_value::<When>(v.clone())?),
            None => None,
        };
        Ok(Task {
            id: doc.id.clone(),
            runner: text("runner"),
            every_secs: parse_duration(&text("every"))?,
            when,
            prompt: text("prompt"),
            cwd: doc.field::<String>("cwd"),
        })
    }
}

/// The folder an agent runs in when its task names none.
pub const DEFAULT_CWD: &str = "workspace";

/// The folder an agent runs in: `cwd`, or `workspace`. A leading `~/` is the
/// home folder, so one task works on machines with different home paths. A
/// relative path is relative to the folder of `db`.
pub fn work_dir(db: &Path, cwd: Option<&str>) -> PathBuf {
    let cwd = cwd.unwrap_or(DEFAULT_CWD);
    if let (Some(rest), Some(home)) = (cwd.strip_prefix("~/"), std::env::var_os("HOME")) {
        return PathBuf::from(home).join(rest);
    }
    db.parent().unwrap_or(Path::new(".")).join(cwd)
}

/// One matching revision since the cursor.
#[derive(Debug, Clone, Serialize, PartialEq, JsonSchema)]
pub struct Change {
    pub seq: i64,
    pub id: String,
    pub rev: String,
    pub deleted: bool,
}

/// This vault's schedule for one task: its `task_state` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TaskState {
    pub task_id: String,
    /// The deployed task revision.
    pub task_rev: String,
    /// The deployed runner, a pinned `doc://` reference.
    pub runner: String,
    pub enabled: bool,
    /// The schedule counts from here until the first run.
    pub enabled_at: String,
    /// The change-feed position the last run consumed.
    pub cursor: i64,
    pub last_run_at: Option<String>,
    /// Set while a run holds the task.
    pub lease_until: Option<String>,
    /// The next tick fires the task, whatever its schedule says.
    pub run_requested: bool,
}

/// Everything `task check` shows and `tick` decides on.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Evaluation {
    /// The revision that runs: the deployed one, or the head when dormant.
    pub task: Doc,
    /// The current revision of the template.
    pub head_rev: String,
    /// A deploy now would change what runs: the template or its runner changed.
    pub drift: bool,
    /// This vault's schedule. `None`: the task is dormant here.
    pub state: Option<TaskState>,
    /// The change-feed head when evaluated. Becomes the cursor when a run finishes.
    pub head: i64,
    /// The position the last run consumed.
    pub cursor: i64,
    pub last_run_at: Option<String>,
    /// A run holds the lease.
    pub running: bool,
    /// The interval has elapsed since the last run.
    pub time_due: bool,
    /// Ready to fire: enabled, not running, and either a run was requested or
    /// time due, with changes when `when` is set.
    pub due: bool,
    pub changes: Vec<Change>,
    #[serde(skip)]
    #[schemars(skip)]
    pub parsed: Task,
    /// The runner revision that would run, resolved once per evaluation,
    /// or why it could not be loaded. Serialized as `runner_error`: why
    /// the runner cannot run, or null.
    #[serde(rename = "runner_error", serialize_with = "runner_error")]
    #[schemars(with = "Option<String>")]
    pub runner: Result<Runner, String>,
}

fn runner_error<S: serde::Serializer>(runner: &Result<Runner, String>, s: S) -> Result<S::Ok, S::Error> {
    runner.as_ref().err().serialize(s)
}

fn head_seq(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COALESCE(MAX(_local_seq), 0) FROM docs", [], |r| r.get(0))?)
}

/// Seconds from `from` to `to`, both in the store's time format.
fn elapsed_secs(conn: &Connection, from: &str, to: &str) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT unixepoch(?2) - unixepoch(?1)", [from, to], |r| r.get(0))?)
}

fn state_in(conn: &Connection, task_id: &str) -> Result<Option<TaskState>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT task_id, task_rev, runner, enabled, enabled_at, cursor, last_run_at, lease_until, run_requested
               FROM task_state WHERE task_id = ?1",
            [task_id],
            |r| {
                Ok(TaskState {
                    task_id: r.get(0)?,
                    task_rev: r.get(1)?,
                    runner: r.get(2)?,
                    enabled: r.get(3)?,
                    enabled_at: r.get(4)?,
                    cursor: r.get(5)?,
                    last_run_at: r.get(6)?,
                    lease_until: r.get(7)?,
                    run_requested: r.get(8)?,
                })
            },
        )
        .optional()?)
}

/// This vault's schedule for a task, or `None` when it is dormant here.
pub fn state(store: &Store, task_id: &str) -> Result<Option<TaskState>, StoreError> {
    state_in(store.connection(), task_id)
}

/// One task to deploy: the revisions that would run, and what the person
/// confirms.
#[derive(Debug, Clone, Serialize)]
pub struct Deploy {
    pub task_id: String,
    pub task_rev: String,
    /// The pinned runner reference.
    pub runner: String,
    /// The runner's command.
    pub command: Vec<String>,
    pub every: String,
    pub when: Option<When>,
    pub prompt: String,
    /// The task's `cwd`. `None`: the default folder.
    pub cwd: Option<String>,
    /// The runner's timeout, for example `10m`.
    pub timeout: String,
    /// What is deployed now. `None`: dormant.
    pub current: Option<TaskState>,
}

impl Deploy {
    /// The text a person confirms: exactly what will run.
    pub fn describe(&self) -> String {
        let now = match &self.current {
            None => "dormant".to_string(),
            Some(s) if s.enabled => format!("deployed at {}", short_rev(&s.task_rev)),
            Some(s) => format!("disabled at {}", short_rev(&s.task_rev)),
        };
        let mut text = format!("{}  rev {}  (now {now})\n", self.task_id, short_rev(&self.task_rev));
        let _ = writeln!(text, "  every:   {}", self.every);
        if let Some(w) = &self.when {
            let _ = writeln!(text, "  when:    {}", w.summary());
        }
        let _ = writeln!(text, "  runner:  {}", self.runner);
        let _ = writeln!(text, "  command: {}", self.command.join(" "));
        let _ = match &self.cwd {
            Some(cwd) => writeln!(text, "  cwd:     {cwd}"),
            None => writeln!(text, "  cwd:     {DEFAULT_CWD} (default)"),
        };
        let _ = writeln!(text, "  timeout: {}", self.timeout);
        text.push_str("  prompt:\n");
        for line in self.prompt.lines() {
            let _ = writeln!(text, "    {line}");
        }
        text
    }
}

/// The head of a live task, parsed, with its runner resolved and pinned.
fn resolve(store: &Store, id: &str) -> Result<(Doc, Task, Runner), StoreError> {
    let head = store.get(id)?;
    let task = Task::from_doc(&head)?;
    let runner = Runner::get(store, &task.runner)?;
    Ok((head, task, runner))
}

/// What a deploy would change. With an id: that task, or nothing when it is
/// deployed and enabled at its current heads. Without: every live task
/// that is not. A task with conflicts cannot deploy.
pub fn plan_deploy(store: &Store, id: Option<&str>) -> Result<Vec<Deploy>, StoreError> {
    let ids: Vec<String> = match id {
        Some(id) => vec![id.to_string()],
        None => store.list_all(Some(TASK_TYPE))?.into_iter().map(|d| d.id).collect(),
    };
    let mut plan = Vec::new();
    for id in ids {
        let (head, task, runner) = resolve(store, &id)?;
        if !head.conflicts.is_empty() {
            return Err(StoreError::invalid(format!("{id} has conflicts; resolve them before a deploy")));
        }
        let current = state(store, &id)?;
        let runner_ref = runner.pinned();
        if current.as_ref().is_some_and(|s| s.enabled && s.task_rev == head.rev && s.runner == runner_ref) {
            continue;
        }
        plan.push(Deploy {
            task_id: id,
            task_rev: head.rev.clone(),
            runner: runner_ref,
            command: runner.command,
            every: head.field::<String>("every").unwrap_or_default(),
            when: task.when,
            prompt: task.prompt,
            cwd: task.cwd,
            timeout: format_duration(runner.timeout_secs),
            current,
        });
    }
    Ok(plan)
}

/// Whether each planned task and runner is still at the revision in `plan`.
pub fn plan_is_current(store: &Store, plan: &[Deploy]) -> Result<bool, StoreError> {
    for d in plan {
        match resolve(store, &d.task_id) {
            Ok((head, _, runner)) if head.rev == d.task_rev && runner.pinned() == d.runner => {}
            Ok(_) | Err(StoreError::NotFound { .. } | StoreError::Deleted { .. }) => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Deploy a confirmed plan, in one transaction. Fails when a task or runner
/// changed since the plan was made. A task deployed for the first time
/// starts its schedule now and its cursor at the change-feed head, so it
/// does not replay history; a redeploy keeps its cursor.
pub fn apply_deploy(store: &mut Store, plan: &[Deploy]) -> Result<Vec<TaskState>, StoreError> {
    let now = store.now()?;
    let tx = store.connection().unchecked_transaction()?;
    if !plan_is_current(store, plan)? {
        return Err(StoreError::invalid("a task or runner changed since you confirmed; deploy again"));
    }
    for d in plan {
        tx.execute(
            "INSERT INTO task_state(task_id, task_rev, runner, enabled, enabled_at, cursor)
             VALUES (?1, ?2, ?3, 1, ?4, (SELECT COALESCE(MAX(_local_seq), 0) FROM docs))
             ON CONFLICT(task_id) DO UPDATE SET task_rev = excluded.task_rev, runner = excluded.runner, enabled = 1",
            params![d.task_id, d.task_rev, d.runner, now],
        )?;
    }
    let states = plan
        .iter()
        .map(|d| Ok(state_in(&tx, &d.task_id)?.expect("the row was just written")))
        .collect::<Result<Vec<_>, StoreError>>()?;
    tx.commit()?;
    Ok(states)
}

/// Ask the scheduler to fire a task on its next tick, at its deployed
/// revisions. Only a task enabled on this vault: the person confirmed
/// exactly what runs when they deployed it.
pub fn request_run(store: &mut Store, task_id: &str) -> Result<TaskState, StoreError> {
    let conn = store.connection();
    if conn.execute("UPDATE task_state SET run_requested = 1 WHERE task_id = ?1 AND enabled = 1", [task_id])? == 0 {
        Task::from_doc(&store.get(task_id)?)?;
        return Err(StoreError::invalid(format!("{task_id} is not deployed on this vault; deploy it first")));
    }
    Ok(state_in(conn, task_id)?.expect("the row exists"))
}

/// Stop running a task on this vault. It keeps its pins and cursor.
pub fn disable(store: &mut Store, task_id: &str) -> Result<TaskState, StoreError> {
    let conn = store.connection();
    if conn.execute("UPDATE task_state SET enabled = 0 WHERE task_id = ?1", [task_id])? == 0 {
        Task::from_doc(&store.get(task_id)?)?;
        return Err(StoreError::invalid(format!("{task_id} is not deployed on this vault")));
    }
    Ok(state_in(conn, task_id)?.expect("the row exists"))
}

fn changes_since(
    conn: &Connection,
    task_id: &str,
    when: &When,
    cursor: i64,
    head: i64,
) -> Result<Vec<Change>, StoreError> {
    let ids_json = match &when.ids {
        Some(ids) => Some(serde_json::to_string(ids)?),
        None => None,
    };
    let (exact, path) = type_filter(when.type_id.as_deref());
    let mut stmt = conn.prepare_cached(
        "SELECT d._local_seq, d._id, d._rev, d._deleted FROM docs d
          WHERE d._local_seq > ?1 AND d._local_seq <= ?2
            AND (d.actor IS NULL OR d.actor <> ?3)
            AND (?4 IS NULL OR d._id GLOB ?4)
            AND (?5 IS NULL OR d._type = ?5) AND (?8 IS NULL OR d._type_path = ?8)
            AND (?6 IS NULL
                 OR (json_type(d.body, '$.tags') = 'array'
                     AND EXISTS (SELECT 1 FROM json_each(d.body, '$.tags') j WHERE j.type = 'text' AND j.value = ?6))
                 OR (d._deleted = 1
                     AND EXISTS (SELECT 1 FROM docs p, json_each(p.body, '$.tags') j
                                  WHERE p._rev = d._parent AND json_type(p.body, '$.tags') = 'array'
                                    AND j.type = 'text' AND j.value = ?6)))
            AND (?7 IS NULL OR d._id IN (SELECT value FROM json_each(?7)))
          ORDER BY d._local_seq",
    )?;
    let rows = stmt.query_map(params![cursor, head, task_id, when.glob, exact, when.tag, ids_json, path], |r| {
        Ok(Change { seq: r.get(0)?, id: r.get(1)?, rev: r.get(2)?, deleted: r.get::<_, i64>(3)? != 0 })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Evaluate every current task, or one task by id. A deployed task is
/// evaluated at its pinned revisions, a dormant one at its heads. Only a
/// task enabled on this vault can be due. All reads happen in one snapshot, so `head`
/// bounds `changes` exactly.
pub fn evaluate(store: &Store, now: &str, only: Option<&str>) -> Result<Vec<Evaluation>, StoreError> {
    let conn = store.connection();
    let tx = conn.unchecked_transaction()?;
    let sql = format!(
        "SELECT {DOC_COLS} FROM doc_heads h JOIN docs d ON d._local_seq = h.seq
          WHERE d._type_path = ?1 AND d._deleted = 0 AND (?2 IS NULL OR d._id = ?2)
          ORDER BY d._id"
    );
    let mut stmt = tx.prepare(&sql)?;
    let docs = stmt.query_map(params![TASK_TYPE, only], |r| row_to_doc(r, true))?.collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    if let Some(id) = only
        && docs.is_empty()
    {
        return Err(match store.get(id) {
            Ok(_) => StoreError::invalid(format!("{id} is not a {TASK_TYPE} document")),
            Err(e) => e,
        });
    }
    let head = head_seq(&tx)?;
    let mut out = Vec::new();
    for head_doc in docs {
        let state = state_in(&tx, &head_doc.id)?;
        let head_rev = head_doc.rev.clone();
        let (doc, runner, drift) = match &state {
            None => {
                let runner =
                    Task::from_doc(&head_doc).and_then(|t| Runner::get(store, &t.runner)).map_err(|e| e.to_string());
                (head_doc, runner, false)
            }
            Some(s) => {
                let current = resolve(store, &head_doc.id).map(|(_, _, r)| r.pinned());
                let drift = s.task_rev != head_rev || current.ok().as_deref() != Some(s.runner.as_str());
                // The pinned runner, unless its document was deleted since: stopping needs no deploy.
                let runner = Runner::get(store, &s.runner).and_then(|r| store.get(&r.id).map(|_| r));
                (store.get_rev(&s.task_rev)?, runner.map_err(|e| e.to_string()), drift)
            }
        };
        let parsed = Task::from_doc(&doc)?;
        let (cursor, last_run_at, running, time_due, changes) = match &state {
            None => (head, None, false, false, Vec::new()),
            Some(s) => {
                let running = match &s.lease_until {
                    Some(until) => elapsed_secs(&tx, now, until)? > 0,
                    None => false,
                };
                let since = s.last_run_at.as_deref().unwrap_or(&s.enabled_at);
                let time_due = elapsed_secs(&tx, since, now)? >= parsed.every_secs as i64;
                let changes = match &parsed.when {
                    Some(when) => changes_since(&tx, &doc.id, when, s.cursor, head)?,
                    None => Vec::new(),
                };
                (s.cursor, s.last_run_at.clone(), running, time_due, changes)
            }
        };
        let enabled = state.as_ref().is_some_and(|s| s.enabled);
        let requested = state.as_ref().is_some_and(|s| s.run_requested);
        let scheduled = time_due && (parsed.when.is_none() || !changes.is_empty());
        let due = enabled && !running && (requested || scheduled);
        out.push(Evaluation {
            task: doc,
            head_rev,
            drift,
            state,
            head,
            cursor,
            last_run_at,
            running,
            time_due,
            due,
            changes,
            parsed,
            runner,
        });
    }
    tx.commit()?;
    Ok(out)
}

/// The prompt as the agent receives it: the task's text, then, for a task
/// with `when`, the newest matching revision of each changed document.
pub fn prompt_text(eval: &Evaluation) -> String {
    let mut text = eval.parsed.prompt.clone();
    if eval.parsed.when.is_none() {
        return text;
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let mut newest: BTreeMap<&str, &Change> = BTreeMap::new();
    for c in &eval.changes {
        newest.insert(&c.id, c);
    }
    let mut rows: Vec<&Change> = newest.into_values().collect();
    rows.sort_by_key(|c| c.seq);
    let _ = write!(text, "\n---\nChanged since last run (seq {} to {}):\n", eval.cursor, eval.head);
    for c in rows {
        let _ = writeln!(text, "- {}  rev {}{}", c.id, short_rev(&c.rev), if c.deleted { "  (deleted)" } else { "" });
    }
    text
}

/// A lease on a task, taken before the agent starts.
#[derive(Debug, Clone)]
pub struct Claim {
    /// `runs/<task id without extension>/<UUID v7>.md`, the id of the receipt.
    pub run_id: String,
    pub task_id: String,
    /// The task revision that runs.
    pub task_rev: String,
    /// The pinned runner revision when it loaded; the task's own reference when it did not.
    pub runner: String,
    pub started_at: String,
    /// The change-feed head the run consumes.
    pub head: i64,
}

/// Take the lease on a task for the runner's timeout plus a minute.
/// Returns `None` when another process holds the lease, or finished a run
/// since `eval`. `force` ignores both. A task with no state gets a disabled
/// row pinned to its heads, so a manual run of a dormant task does not
/// start its schedule.
pub fn claim(store: &mut Store, eval: &Evaluation, now: &str, force: bool) -> Result<Option<Claim>, StoreError> {
    let task_id = eval.task.id.clone();
    let lease_secs = eval.runner.as_ref().map(|r| r.timeout_secs).unwrap_or(0) as i64 + 60;
    let runner = match &eval.runner {
        Ok(r) => r.pinned(),
        Err(_) => eval.parsed.runner.clone(),
    };
    let tx = store.connection().unchecked_transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO task_state(task_id, task_rev, runner, enabled, enabled_at, cursor)
         VALUES (?1, ?2, ?3, 0, ?4, ?5)",
        params![task_id, eval.task.rev, runner, now, eval.head],
    )?;
    let taken = tx.execute(
        "UPDATE task_state SET lease_until = strftime('%Y-%m-%dT%H:%M:%fZ', unixepoch(?2) + ?3, 'unixepoch'),
                run_requested = 0
          WHERE task_id = ?1
            AND (?5 OR (last_run_at IS ?4 AND (lease_until IS NULL OR unixepoch(lease_until) <= unixepoch(?2))))",
        params![task_id, now, lease_secs, eval.last_run_at, force],
    )?;
    tx.commit()?;
    if taken == 0 {
        return Ok(None);
    }
    Ok(Some(Claim {
        run_id: format!("runs/{}/{}.md", stem(&task_id), new_id()),
        task_rev: eval.task.rev.clone(),
        task_id,
        runner,
        started_at: now.to_string(),
        head: eval.head,
    }))
}

/// What came back from the command.
#[derive(Debug, Default)]
pub struct Outcome {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
}

/// Spawn the resolved argv with the prompt on stdin. Stdin and stdout are
/// driven together so neither pipe fills. On timeout the child is killed.
pub async fn spawn(argv: &[String], env: &[(String, String)], cwd: &Path, prompt: &str, timeout: Duration) -> Outcome {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome { spawn_error: Some(format!("cannot start {}: {e}", argv[0])), ..Default::default() };
        }
    };
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let prompt = prompt.as_bytes().to_vec();
    let feed = async move {
        // A command that never reads stdin closes the pipe; that is not an error.
        let _ = stdin.write_all(&prompt).await;
        let _ = stdin.shutdown().await;
        drop(stdin);
    };
    match tokio::time::timeout(timeout, async { tokio::join!(feed, child.wait_with_output()) }).await {
        Ok((_, Ok(output))) => Outcome {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
            ..Default::default()
        },
        Ok((_, Err(e))) => Outcome { spawn_error: Some(format!("waiting for {}: {e}", argv[0])), ..Default::default() },
        Err(_) => Outcome { timed_out: true, ..Default::default() },
    }
}

/// The command's last message: the `{out}` file when the command wrote
/// one, else stdout.
pub fn last_message(out_file: &Path, stdout: &[u8]) -> Vec<u8> {
    match std::fs::read(out_file) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        _ => stdout.to_vec(),
    }
}

/// Why a command failed: it did not start, it timed out, or it exited
/// non-zero (with the tail of its stderr). Empty on success.
pub fn failures(outcome: &Outcome, timeout: Duration) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    if let Some(e) = &outcome.spawn_error {
        errors.push(e.clone());
    }
    if outcome.timed_out {
        errors.push(format!("timeout after {}s", timeout.as_secs()));
    }
    if !outcome.timed_out && outcome.spawn_error.is_none() && outcome.exit_code != Some(0) {
        let (tail, _) =
            truncate_bytes(&outcome.stderr[outcome.stderr.len().saturating_sub(MAX_STDERR_BYTES)..], MAX_STDERR_BYTES);
        let tail = tail.trim();
        errors.push(match outcome.exit_code {
            Some(code) if tail.is_empty() => format!("exit code {code}"),
            Some(code) => format!("exit code {code}: {tail}"),
            None if tail.is_empty() => "killed by signal".to_string(),
            None => format!("killed by signal: {tail}"),
        });
    }
    errors
}

/// Write the receipt, move the cursor, and release the lease, in one
/// transaction. `content` is the command's last message.
pub fn finish(
    store: &mut Store,
    claim: &Claim,
    outcome: &Outcome,
    out_file: &Path,
    timeout: Duration,
    now: &str,
) -> Result<Doc, StoreError> {
    let Value::Object(mut body) = json!({
        "task": DocRef::pinned(&claim.task_id, &claim.task_rev).to_string(),
        "runner": claim.runner,
        "vault": store.vault_id()?,
        "started_at": claim.started_at,
        "finished_at": now,
        "tags": [claim.task_id],
    }) else {
        unreachable!("a JSON object")
    };
    let (content, cut) = truncate_bytes(&last_message(out_file, &outcome.stdout), MAX_CONTENT_BYTES);
    let mut errors = failures(outcome, timeout);
    if cut {
        errors.push(format!("output truncated to {MAX_CONTENT_BYTES} bytes"));
    }
    body.insert("exit_code".into(), outcome.exit_code.map(Value::from).unwrap_or(Value::Null));
    if !errors.is_empty() {
        body.insert("error".into(), Value::String(errors.join("; ")));
    }
    if !content.is_empty() {
        body.insert("content".into(), Value::String(content));
    }
    let input = PutInput { id: Some(claim.run_id.clone()), parent: None, type_id: Some(RUN_TYPE.into()), body };
    let receipt = store.put_if(input, |conn| {
        // Not a check: the state update joins the receipt's transaction.
        conn.execute(
            "UPDATE task_state SET cursor = ?2, last_run_at = ?3, lease_until = NULL WHERE task_id = ?1",
            params![claim.task_id, claim.head, claim.started_at],
        )?;
        Ok(true)
    })?;
    Ok(receipt.expect("the check always passes"))
}

/// The result of one firing, as `tick` reports it.
#[derive(Debug, Clone, Serialize)]
pub struct Fired {
    pub task: String,
    pub run: String,
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Claim, spawn, record. Returns `None` when another process claimed the
/// task first; `force` claims anyway. Every failure after the claim lands
/// in the receipt's `error`.
pub async fn fire(
    store: &mut Store,
    db: &Path,
    eval: &Evaluation,
    now: &str,
    force: bool,
) -> Result<Option<Fired>, StoreError> {
    let Some(claim) = claim(store, eval, now, force)? else {
        return Ok(None);
    };
    let scratch = std::env::temp_dir().join(format!("dreams-{}", crate::doc::new_id()));
    std::fs::create_dir_all(&scratch)
        .map_err(|e| StoreError::invalid(format!("creating {}: {e}", scratch.display())))?;
    let ctx = Context {
        db: db.to_path_buf(),
        task: eval.task.id.clone(),
        run: claim.run_id.clone(),
        mcp: scratch.join("mcp.json"),
        out: scratch.join("last-message"),
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dreams")),
    };
    let cwd = work_dir(db, eval.parsed.cwd.as_deref());

    let failed = |e: String| (Outcome { spawn_error: Some(e), ..Default::default() }, Duration::from_secs(0));
    let outcome = match (&eval.runner, std::fs::create_dir_all(&cwd)) {
        (Err(e), _) => failed(e.clone()),
        (Ok(_), Err(e)) => failed(format!("creating {}: {e}", cwd.display())),
        (Ok(runner), Ok(())) => {
            let timeout = Duration::from_secs(runner.timeout_secs);
            let _ = std::fs::write(&ctx.mcp, ctx.mcp_config());
            let argv = ctx.resolve(&runner.command);
            (spawn(&argv, &ctx.env(), &cwd, &prompt_text(eval), timeout).await, timeout)
        }
    };
    let finished_at = store.now()?;
    // The receipt records the task as its actor.
    let done = store
        .as_actor(Some(eval.task.id.clone()), |s| finish(s, &claim, &outcome.0, &ctx.out, outcome.1, &finished_at));
    let _ = std::fs::remove_dir_all(&scratch);
    let done = done?;
    Ok(Some(Fired {
        task: eval.task.id.clone(),
        run: done.id.clone(),
        exit_code: outcome.0.exit_code,
        error: done.field::<String>("error"),
    }))
}

#[derive(Debug, Default, Serialize)]
pub struct TickReport {
    pub fired: Vec<Fired>,
    /// Due tasks another process claimed first.
    pub skipped: Vec<String>,
    /// Tasks whose firing failed before a run could record it.
    pub errors: Vec<(String, String)>,
}

/// One pass over every task enabled on this vault. Sequential; one failure never stops
/// the others. Records `now` as the last tick when the pass ends.
pub async fn tick(store: &mut Store, db: &Path, now: &str) -> Result<TickReport, StoreError> {
    let mut report = TickReport::default();
    for eval in evaluate(store, now, None)? {
        if !eval.due {
            continue;
        }
        match fire(store, db, &eval, now, false).await {
            Ok(Some(f)) => report.fired.push(f),
            Ok(None) => report.skipped.push(eval.task.id.clone()),
            Err(e) => report.errors.push((eval.task.id.clone(), e.to_string())),
        }
    }
    store.vault_id()?;
    store.connection().execute("UPDATE vault SET last_tick = ?1", [now])?;
    Ok(report)
}

/// A scheduler that has not ticked for this long is taken as stopped. The
/// daemon ticks at least every minute by default.
pub const STALE_AFTER_SECS: i64 = 300;

/// Whether a scheduler runs on this vault, as far as its ticks show.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct SchedulerStatus {
    /// When the last scheduler pass ended. `None`: never on this vault.
    pub last_tick: Option<String>,
    /// No tick in the last five minutes: deployed tasks do not fire.
    pub stale: bool,
}

/// Every task on this vault, and whether a scheduler fires them.
#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskList {
    pub tasks: Vec<Evaluation>,
    pub scheduler: SchedulerStatus,
}

/// Every task evaluated now, with the scheduler's status.
pub fn list(store: &Store) -> Result<TaskList, StoreError> {
    let now = store.now()?;
    Ok(TaskList { tasks: evaluate(store, &now, None)?, scheduler: scheduler_status(store, &now)? })
}

pub fn scheduler_status(store: &Store, now: &str) -> Result<SchedulerStatus, StoreError> {
    let conn = store.connection();
    let last_tick: Option<String> =
        conn.query_row("SELECT last_tick FROM vault", [], |r| r.get(0)).optional()?.flatten();
    let stale = match &last_tick {
        Some(t) => elapsed_secs(conn, t, now)? > STALE_AFTER_SECS,
        None => true,
    };
    Ok(SchedulerStatus { last_tick, stale })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("15m").unwrap(), 900);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
        assert_eq!(parse_duration("1w").unwrap(), 604_800);
        for text in ["30s", "90s", "10m", "2h", "1d", "1w", "8d"] {
            assert_eq!(format_duration(parse_duration(text).unwrap()), text);
        }
        for bad in ["0m", "15", "m", "", "1.5h", "-1m"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    fn eval(prompt: &str, when: Option<When>, changes: Vec<Change>) -> Evaluation {
        let doc = Doc {
            id: "tasks/t".into(),
            rev: "1-a".into(),
            parent: None,
            type_id: Some(format!("{TASK_TYPE}?rev=1-a")),
            deleted: false,
            created_at: "t".into(),
            actor: None,
            seq: Some(4),
            conflicts: Vec::new(),
            deleted_conflicts: Vec::new(),
            body: serde_json::Map::new(),
        };
        Evaluation {
            task: doc,
            head_rev: "1-a".into(),
            drift: false,
            state: None,
            head: 9,
            cursor: 4,
            last_run_at: None,
            running: false,
            time_due: true,
            due: true,
            changes,
            parsed: Task {
                id: "tasks/t".into(),
                runner: "doc://runners/cat".into(),
                every_secs: 60,
                when,
                prompt: prompt.into(),
                cwd: None,
            },
            runner: Err("not loaded".into()),
        }
    }

    #[test]
    fn work_dir_is_cwd_or_workspace_next_to_the_vault() {
        let db = Path::new("/v/vault.db");
        assert_eq!(work_dir(db, None), Path::new("/v/workspace"));
        assert_eq!(work_dir(db, Some("repos/a")), Path::new("/v/repos/a"));
        assert_eq!(work_dir(db, Some("/abs")), Path::new("/abs"));
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(work_dir(db, Some("~/src")), home.join("src"));
    }

    #[test]
    fn prompt_lists_newest_revision_per_id() {
        assert_eq!(prompt_text(&eval("just this", None, vec![])), "just this");
        let changes = vec![
            Change { seq: 5, id: "a".into(), rev: "1-aaaaaaaaaaaa".into(), deleted: false },
            Change { seq: 6, id: "b".into(), rev: "1-bbbbbbbbbbbb".into(), deleted: false },
            Change { seq: 7, id: "a".into(), rev: "2-cccccccccccc".into(), deleted: true },
        ];
        let text = prompt_text(&eval("do it", Some(When::default()), changes));
        assert_eq!(
            text,
            "do it\n\n---\nChanged since last run (seq 4 to 9):\n- b  rev 1-bbbbbbbb\n- a  rev 2-cccccccc  (deleted)\n"
        );
        let empty = prompt_text(&eval("do it\n", Some(When::default()), vec![]));
        assert!(empty.ends_with("(seq 4 to 9):\n"), "{empty}");
    }
}
