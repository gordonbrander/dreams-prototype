//! Scheduled tasks: documents typed `doc://schemas/task` that wake an agent
//! every interval, optionally only when watched documents changed. Runs are
//! documents typed `doc://schemas/run` and the only state: the newest run
//! holds the cursor.

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

use crate::doc::{Doc, PutInput};
use crate::error::StoreError;
use crate::runner::{Context, Runner};
use crate::store::{DOC_COLS, Store, row_to_doc, type_filter};

/// Seeded schema documents, as type paths.
pub const TASK_TYPE: &str = "doc://schemas/task";
pub const RUN_TYPE: &str = "doc://schemas/run";

/// Agent output above this is cut, and the run records the cut.
pub const MAX_CONTENT_BYTES: usize = 512 * 1024;
const MAX_STDERR_BYTES: usize = 4 * 1024;

/// The body of `schemas/task`.
pub const TASK_SCHEMA: &str = r#"{
  "title": "Scheduled task",
  "description": "Wake an agent every interval with a prompt. With `when`, only when a matching document changed since the last run. `runner` is a doc:// reference to a runner document.",
  "type": "object",
  "required": ["runner", "every", "prompt"],
  "properties": {
    "runner": {"type": "string", "pattern": "^doc://"},
    "every": {"type": "string", "pattern": "^[0-9]+[smhdw]$"},
    "prompt": {"type": "string"},
    "enabled": {"type": "boolean"},
    "when": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "glob": {"type": "string", "minLength": 1},
        "tag": {"type": "string", "minLength": 1},
        "type": {"type": "string", "minLength": 1},
        "ids": {"type": "array", "minItems": 1, "items": {"type": "string"}}
      }
    },
    "title": {"type": "string"},
    "tags": {"type": "array", "items": {"type": "string"}}
  }
}"#;

/// The body of `schemas/run`.
pub const RUN_SCHEMA: &str = r#"{
  "title": "Task run",
  "description": "One firing of a task. Revision 1 is the claim, written before the agent starts; revision 2 adds the result. `seq` is the change-feed position the run consumed. `runner` is the pinned doc:// reference of the runner revision that ran.",
  "type": "object",
  "required": ["task", "runner", "started_at", "seq", "tags"],
  "properties": {
    "task": {"type": "string"},
    "runner": {"type": "string"},
    "started_at": {"type": "string"},
    "finished_at": {"type": "string"},
    "seq": {"type": "integer"},
    "exit_code": {"type": ["integer", "null"]},
    "error": {"type": "string"},
    "content": {"type": "string"},
    "tags": {"type": "array", "items": {"type": "string"}}
  }
}"#;

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
    pub enabled: bool,
}

impl Task {
    pub fn from_doc(doc: &Doc) -> Result<Task, StoreError> {
        if doc.type_path() != Some(TASK_TYPE) {
            return Err(StoreError::invalid(format!("{} is not a {TASK_TYPE} document", doc.id)));
        }
        let text = |key: &str| doc.body.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
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
            enabled: doc.body.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        })
    }
}

/// One matching revision since the cursor.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Change {
    pub seq: i64,
    pub id: String,
    pub rev: String,
    pub deleted: bool,
}

/// Everything `task check` shows and `tick` decides on.
#[derive(Debug, Clone, Serialize)]
pub struct Evaluation {
    pub task: Doc,
    /// The change-feed head when evaluated. Becomes the run's `seq`.
    pub head: i64,
    /// The position the last run consumed.
    pub cursor: i64,
    pub last_run_at: Option<String>,
    pub last_run: Option<String>,
    /// The newest run has no result yet and is younger than its timeout.
    pub running: bool,
    /// The interval has elapsed since the last run.
    pub time_due: bool,
    /// Ready to fire: enabled, not running, time due, and changes when `when` is set.
    pub due: bool,
    pub changes: Vec<Change>,
    #[serde(skip)]
    pub parsed: Task,
    /// The runner revision that would run, resolved once per evaluation,
    /// or why it could not be loaded.
    #[serde(skip)]
    pub runner: Result<Runner, String>,
}

fn head_seq(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT COALESCE(MAX(_local_seq), 0) FROM docs", [], |r| r.get(0))?)
}

/// Seconds from `from` to `to`, both in the store's time format.
fn elapsed_secs(conn: &Connection, from: &str, to: &str) -> Result<i64, StoreError> {
    Ok(conn.query_row("SELECT unixepoch(?2) - unixepoch(?1)", [from, to], |r| r.get(0))?)
}

/// The run with the greatest `started_at` for a task, tombstones excluded.
fn newest_run(conn: &Connection, task_id: &str) -> Result<Option<Doc>, StoreError> {
    let sql = format!(
        "SELECT {DOC_COLS} FROM doc_tags t
           JOIN doc_heads h ON h._id = t._id
           JOIN docs d ON d._local_seq = h.seq
          WHERE t.tag = ?1 AND d._type_path = ?2
          ORDER BY json_extract(d.body, '$.started_at') DESC LIMIT 1"
    );
    Ok(conn.query_row(&sql, params![task_id, RUN_TYPE], |r| row_to_doc(r, false)).optional()?)
}

fn changes_since(conn: &Connection, task_id: &str, when: &When, cursor: i64, head: i64) -> Result<Vec<Change>, StoreError> {
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
    let rows = stmt.query_map(
        params![cursor, head, task_id, when.glob, exact, when.tag, ids_json, path],
        |r| {
            Ok(Change {
                seq: r.get(0)?,
                id: r.get(1)?,
                rev: r.get(2)?,
                deleted: r.get::<_, i64>(3)? != 0,
            })
        },
    )?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Evaluate every enabled task, or one task by id (enabled or not). All
/// reads happen in one snapshot, so `head` bounds `changes` exactly.
pub fn evaluate(store: &Store, now: &str, only: Option<&str>) -> Result<Vec<Evaluation>, StoreError> {
    let conn = store.connection();
    let tx = conn.unchecked_transaction()?;
    let sql = format!(
        "SELECT {DOC_COLS} FROM doc_heads h JOIN docs d ON d._local_seq = h.seq
          WHERE d._type_path = ?1 AND d._deleted = 0 AND (?2 IS NULL OR d._id = ?2)
          ORDER BY d._id"
    );
    let mut stmt = tx.prepare(&sql)?;
    let docs = stmt
        .query_map(params![TASK_TYPE, only], |r| row_to_doc(r, true))?
        .collect::<Result<Vec<_>, _>>()?;
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
    for doc in docs {
        let parsed = Task::from_doc(&doc)?;
        if only.is_none() && !parsed.enabled {
            continue;
        }
        let runner = Runner::get(store, &parsed.runner).map_err(|e| e.to_string());
        let newest = newest_run(&tx, &doc.id)?;
        let (last_run_at, cursor, last_run, running) = match &newest {
            Some(run) => {
                let started = run.body.get("started_at").and_then(Value::as_str).unwrap_or_default().to_string();
                let seq = run.body.get("seq").and_then(Value::as_i64).unwrap_or(0);
                let unfinished = run.body.get("finished_at").is_none();
                let timeout = runner
                    .as_ref()
                    .map(|r| r.timeout_secs)
                    .unwrap_or_else(|_| parse_duration(crate::runner::DEFAULT_TIMEOUT).unwrap_or(600));
                let young = elapsed_secs(&tx, &started, now)? < timeout as i64 + 60;
                (Some(started), seq, Some(run.id.clone()), unfinished && young)
            }
            None => (None, doc.seq.unwrap_or(0), None, false),
        };
        let since = last_run_at.clone().unwrap_or_else(|| doc.created_at.clone());
        let time_due = elapsed_secs(&tx, &since, now)? >= parsed.every_secs as i64;
        let changes = match &parsed.when {
            Some(when) => changes_since(&tx, &doc.id, when, cursor, head)?,
            None => Vec::new(),
        };
        let due = parsed.enabled && !running && time_due && (parsed.when.is_none() || !changes.is_empty());
        out.push(Evaluation {
            task: doc,
            head,
            cursor,
            last_run_at,
            last_run,
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
        let rev = c.rev.split_once('-').map(|(g, h)| format!("{g}-{}", &h[..h.len().min(8)])).unwrap_or_default();
        let _ = writeln!(text, "- {}  rev {rev}{}", c.id, if c.deleted { "  (deleted)" } else { "" });
    }
    text
}

/// `runs/<task>/<compact time>-<head>`.
fn run_id(task_id: &str, now: &str, head: i64) -> String {
    let compact: String = now.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("runs/{task_id}/{compact}-{head}")
}

/// Write run revision 1 before the agent starts. Fails the check, and
/// returns `None`, when another process fired the task first.
pub fn claim(store: &mut Store, eval: &Evaluation, now: &str) -> Result<Option<Doc>, StoreError> {
    let task_id = eval.task.id.clone();
    let expected = eval.last_run.clone();
    // The pinned runner revision when it loaded; the task's own reference when it did not.
    let runner = match &eval.runner {
        Ok(r) => r.pinned(),
        Err(_) => eval.parsed.runner.clone(),
    };
    let input: PutInput = serde_json::from_value(json!({
        "_id": run_id(&task_id, now, eval.head),
        "_type": RUN_TYPE,
        "task": task_id,
        "runner": runner,
        "started_at": now,
        "seq": eval.head,
        "tags": [task_id],
    }))?;
    let check_task = eval.task.id.clone();
    store.put_if(input, move |conn| {
        let newest = newest_run(conn, &check_task)?;
        Ok(newest.map(|d| d.id) == expected)
    })
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

/// Lossy UTF-8, cut at a character boundary. The flag says whether it was cut.
fn truncate_utf8(bytes: &[u8], max: usize) -> (String, bool) {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
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
        let (tail, _) = truncate_utf8(&outcome.stderr[outcome.stderr.len().saturating_sub(MAX_STDERR_BYTES)..], MAX_STDERR_BYTES);
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

/// Write run revision 2 with the result. `content` is the command's last message.
pub fn finish(store: &mut Store, run: &Doc, outcome: &Outcome, out_file: &Path, timeout: Duration, now: &str) -> Result<Doc, StoreError> {
    let mut body = run.body.clone();
    body.insert("finished_at".into(), Value::String(now.to_string()));
    let (content, cut) = truncate_utf8(&last_message(out_file, &outcome.stdout), MAX_CONTENT_BYTES);
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
    store.put(PutInput {
        id: Some(run.id.clone()),
        parent: Some(run.rev.clone()),
        // The claim's pinned type, so both revisions name the same schema revision.
        type_id: run.type_id.clone(),
        body,
    })
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
/// task first. Every failure after the claim lands in the run's `error`.
pub async fn fire(store: &mut Store, db: &Path, eval: &Evaluation, now: &str) -> Result<Option<Fired>, StoreError> {
    let previous_actor = store.actor().map(str::to_string);
    store.set_actor(Some(eval.task.id.clone()));
    let result = fire_as_task(store, db, eval, now).await;
    store.set_actor(previous_actor);
    result
}

async fn fire_as_task(store: &mut Store, db: &Path, eval: &Evaluation, now: &str) -> Result<Option<Fired>, StoreError> {
    let Some(run) = claim(store, eval, now)? else {
        return Ok(None);
    };
    let scratch = std::env::temp_dir().join(format!("dreams-{}", crate::doc::new_id()));
    std::fs::create_dir_all(&scratch).map_err(|e| StoreError::invalid(format!("creating {}: {e}", scratch.display())))?;
    let ctx = Context {
        db: db.to_path_buf(),
        task: eval.task.id.clone(),
        run: run.id.clone(),
        mcp: scratch.join("mcp.json"),
        out: scratch.join("last-message"),
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dreams")),
    };
    let cwd = db.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));

    let outcome = match &eval.runner {
        Ok(runner) => {
            let timeout = Duration::from_secs(runner.timeout_secs);
            let _ = std::fs::write(&ctx.mcp, ctx.mcp_config());
            let argv = ctx.resolve(&runner.argv);
            (spawn(&argv, &ctx.env(), &cwd, &prompt_text(eval), timeout).await, timeout)
        }
        Err(e) => (
            Outcome { spawn_error: Some(e.clone()), ..Default::default() },
            Duration::from_secs(0),
        ),
    };
    let finished_at = store.now()?;
    let done = finish(store, &run, &outcome.0, &ctx.out, outcome.1, &finished_at);
    let _ = std::fs::remove_dir_all(&scratch);
    let done = done?;
    Ok(Some(Fired {
        task: eval.task.id.clone(),
        run: done.id.clone(),
        exit_code: outcome.0.exit_code,
        error: done.body.get("error").and_then(Value::as_str).map(str::to_string),
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

/// One pass over every enabled task. Sequential; one failure never stops
/// the others.
pub async fn tick(store: &mut Store, db: &Path, now: &str) -> Result<TickReport, StoreError> {
    let mut report = TickReport::default();
    for eval in evaluate(store, now, None)? {
        if !eval.due {
            continue;
        }
        match fire(store, db, &eval, now).await {
            Ok(Some(f)) => report.fired.push(f),
            Ok(None) => report.skipped.push(eval.task.id.clone()),
            Err(e) => report.errors.push((eval.task.id.clone(), e.to_string())),
        }
    }
    Ok(report)
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
            head: 9,
            cursor: 4,
            last_run_at: None,
            last_run: None,
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
                enabled: true,
            },
            runner: Err("not loaded".into()),
        }
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

    #[test]
    fn run_ids_are_compact() {
        assert_eq!(run_id("tasks/t", "2026-09-23T10:11:12.345Z", 9), "runs/tasks/t/20260923T101112345Z-9");
    }

    #[test]
    fn truncation_keeps_char_boundaries() {
        let s = "héllo wörld".repeat(10);
        let (cut, was_cut) = truncate_utf8(s.as_bytes(), 8);
        assert!(was_cut);
        assert!(cut.len() <= 8);
        assert!(s.starts_with(&cut));
        let (whole, was_cut) = truncate_utf8(b"abc", 8);
        assert_eq!((whole.as_str(), was_cut), ("abc", false));
    }
}
