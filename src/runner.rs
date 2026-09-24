//! Runners: documents typed `doc://schemas/runner` that hold an argv
//! template. A task names one by `doc://` reference. The template is
//! spawned directly, never through a shell, with tokens replaced inside
//! each element. Runner commands are code, so they enter only through the
//! CLI: `serve` marks the type protected. The default runners are seeded
//! by `seed::seed`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::doc::{Doc, DocRef};
use crate::error::StoreError;
use crate::store::Store;
use crate::task::{self, parse_duration};

/// The seeded schema document for runners, as a type path.
pub const RUNNER_TYPE: &str = "doc://schemas/runner";

/// Type paths that MCP clients may read but not write or delete.
pub const PROTECTED_TYPES: &[&str] = &[RUNNER_TYPE, task::RUN_TYPE];

/// Seeded schema documents that MCP clients may not change.
pub const PROTECTED_IDS: &[&str] = &[
    "schemas/task",
    "schemas/run",
    "schemas/runner",
    "schemas/skill",
    "schemas/prompt",
    "schemas/daily",
    "schemas/bookmark",
];

pub const DEFAULT_TIMEOUT: &str = "10m";

/// The body of `schemas/runner`.
pub const RUNNER_SCHEMA: &str = r#"{
  "title": "Runner",
  "description": "A command that runs an agent: argv with {db} {task} {run} {mcp} {out} {exe} tokens. The prompt arrives on stdin; the last message is read from stdout, or from {out} when the command wrote it.",
  "type": "object",
  "required": ["argv"],
  "properties": {
    "argv": {"type": "array", "minItems": 1, "items": {"type": "string"}},
    "timeout": {"type": "string", "pattern": "^[0-9]+[smhdw]$"},
    "title": {"type": "string"}
  }
}"#;

#[derive(Debug, Clone)]
pub struct Runner {
    pub id: String,
    /// The revision that was read, so a run can record exactly what ran.
    pub rev: String,
    pub argv: Vec<String>,
    pub timeout_secs: u64,
}

impl Runner {
    /// `doc://<id>?rev=<rev>` of this runner revision.
    pub fn pinned(&self) -> String {
        DocRef::pinned(&self.id, &self.rev).to_string()
    }

    pub fn from_doc(doc: &Doc) -> Result<Runner, StoreError> {
        if doc.type_path() != Some(RUNNER_TYPE) {
            return Err(StoreError::invalid(format!("{} is not a {RUNNER_TYPE} document", doc.id)));
        }
        let argv: Vec<String> = doc
            .body
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if argv.is_empty() {
            return Err(StoreError::invalid(format!("runner {} has an empty argv", doc.id)));
        }
        let timeout = doc.body.get("timeout").and_then(Value::as_str).unwrap_or(DEFAULT_TIMEOUT);
        Ok(Runner { id: doc.id.clone(), rev: doc.rev.clone(), argv, timeout_secs: parse_duration(timeout)? })
    }

    /// Load by `doc://` reference: the head, or one pinned revision.
    pub fn get(store: &Store, reference: &str) -> Result<Runner, StoreError> {
        let r = DocRef::parse(reference)?;
        let doc = match &r.rev {
            None => store.get(&r.id)?,
            Some(rev) => {
                let d = store.get_rev(rev)?;
                if d.id != r.id {
                    return Err(StoreError::invalid(format!("{rev} is a revision of {}, not {}", d.id, r.id)));
                }
                if d.deleted {
                    return Err(StoreError::Deleted { id: d.id, rev: d.rev });
                }
                d
            }
        };
        Runner::from_doc(&doc)
    }
}

/// Run `runner` once, outside the scheduler, with `prompt` on stdin, and
/// return its last message. `name` fills the `{task}` token and the actor
/// of anything the agent writes. A command that does not start, times
/// out, or exits non-zero is an error.
pub async fn invoke(runner: &Runner, db: &Path, name: &str, prompt: &str) -> Result<String, StoreError> {
    let scratch = std::env::temp_dir().join(format!("dreams-{}", crate::doc::new_id()));
    std::fs::create_dir_all(&scratch)
        .map_err(|e| StoreError::invalid(format!("creating {}: {e}", scratch.display())))?;
    let ctx = Context {
        db: db.to_path_buf(),
        task: name.to_string(),
        run: String::new(),
        mcp: scratch.join("mcp.json"),
        out: scratch.join("last-message"),
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dreams")),
    };
    let cwd =
        db.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::write(&ctx.mcp, ctx.mcp_config());
    let timeout = std::time::Duration::from_secs(runner.timeout_secs);
    let outcome = task::spawn(&ctx.resolve(&runner.argv), &ctx.env(), &cwd, prompt, timeout).await;
    let message = task::last_message(&ctx.out, &outcome.stdout);
    let _ = std::fs::remove_dir_all(&scratch);
    let errors = task::failures(&outcome, timeout);
    if !errors.is_empty() {
        return Err(StoreError::Runner { message: format!("{}: {}", runner.id, errors.join("; ")) });
    }
    Ok(String::from_utf8_lossy(&message).into_owned())
}

/// What one run makes available to the command, as tokens and as
/// environment variables.
#[derive(Debug, Clone)]
pub struct Context {
    /// Absolute path of the database.
    pub db: PathBuf,
    pub task: String,
    pub run: String,
    /// A generated MCP config that points back at this vault as `task`.
    pub mcp: PathBuf,
    /// Where a command may write its last message instead of stdout.
    pub out: PathBuf,
    /// This binary.
    pub exe: PathBuf,
}

impl Context {
    fn pairs(&self) -> [(&'static str, String); 6] {
        [
            ("db", self.db.to_string_lossy().into_owned()),
            ("task", self.task.clone()),
            ("run", self.run.clone()),
            ("mcp", self.mcp.to_string_lossy().into_owned()),
            ("out", self.out.to_string_lossy().into_owned()),
            ("exe", self.exe.to_string_lossy().into_owned()),
        ]
    }

    /// Replace `{db}`, `{task}`, `{run}`, `{mcp}`, `{out}`, `{exe}` inside
    /// each element. Elements stay separate: no shell, no re-splitting.
    pub fn resolve(&self, argv: &[String]) -> Vec<String> {
        let pairs = self.pairs();
        argv.iter()
            .map(|arg| pairs.iter().fold(arg.clone(), |acc, (name, value)| acc.replace(&format!("{{{name}}}"), value)))
            .collect()
    }

    /// The same values as `DREAMS_*` variables, plus `PATH` with this
    /// binary's directory first so an agent's shell finds `dreams`.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .pairs()
            .into_iter()
            .map(|(name, value)| (format!("DREAMS_{}", name.to_ascii_uppercase()), value))
            .collect();
        env.push(("DREAMS_ACTOR".into(), self.task.clone()));
        let mut path = self.exe.parent().map(Path::to_path_buf).unwrap_or_default().to_string_lossy().into_owned();
        if let Ok(existing) = std::env::var("PATH")
            && !existing.is_empty()
        {
            path = format!("{path}:{existing}");
        }
        env.push(("PATH".into(), path));
        env
    }

    /// An MCP config file body for hosts that take one.
    pub fn mcp_config(&self) -> String {
        json!({ "mcpServers": { "dreams": crate::mcp::server_entry(&self.exe, &self.db, Some(&self.task)) } })
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            db: "/v/vault.db".into(),
            task: "tasks/t".into(),
            run: "runs/tasks/t/1".into(),
            mcp: "/tmp/r/mcp.json".into(),
            out: "/tmp/r/out".into(),
            exe: "/bin/dreams".into(),
        }
    }

    #[test]
    fn tokens_are_replaced_per_element() {
        let argv: Vec<String> =
            ["x", "--db={db}", "{task}:{run}", "{missing}", "a {out} b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            ctx().resolve(&argv),
            ["x", "--db=/v/vault.db", "tasks/t:runs/tasks/t/1", "{missing}", "a /tmp/r/out b"]
        );
    }

    #[test]
    fn env_names_everything_and_prepends_exe_dir() {
        let env = ctx().env();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap();
        assert_eq!(get("DREAMS_DB"), "/v/vault.db");
        assert_eq!(get("DREAMS_TASK"), "tasks/t");
        assert_eq!(get("DREAMS_ACTOR"), "tasks/t");
        assert_eq!(get("DREAMS_MCP"), "/tmp/r/mcp.json");
        assert!(get("PATH").starts_with("/bin"));
        let cfg: Value = serde_json::from_str(&ctx().mcp_config()).unwrap();
        assert_eq!(cfg["mcpServers"]["dreams"]["args"][3], "tasks/t");
    }

    #[test]
    fn runner_from_doc_checks_type_and_argv() {
        let mut doc = Doc {
            id: "runners/x".into(),
            rev: "1-a".into(),
            parent: None,
            type_id: Some(format!("{RUNNER_TYPE}?rev=1-a")),
            deleted: false,
            created_at: "t".into(),
            actor: None,
            seq: None,
            conflicts: Vec::new(),
            deleted_conflicts: Vec::new(),
            body: serde_json::from_value(json!({"argv": ["cat"], "timeout": "2m"})).unwrap(),
        };
        let r = Runner::from_doc(&doc).unwrap();
        assert_eq!(r.argv, ["cat"]);
        assert_eq!(r.timeout_secs, 120);
        assert_eq!(r.pinned(), "doc://runners/x?rev=1-a");
        doc.type_id = Some("doc://schemas/note?rev=1-a".into());
        assert!(Runner::from_doc(&doc).is_err());
    }
}
