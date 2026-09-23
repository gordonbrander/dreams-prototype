//! Runners: `runner/v1` documents that hold an argv template. A task names
//! one by `_id`. The template is spawned directly, never through a shell,
//! with tokens replaced inside each element. Runner commands are code, so
//! they enter only through the CLI: `serve` marks the type protected.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::doc::{Doc, PutInput};
use crate::error::StoreError;
use crate::store::Store;
use crate::task::parse_duration;

pub const RUNNER_TYPE: &str = "runner/v1";

/// Types that MCP clients may read but not write or delete.
pub const PROTECTED_TYPES: &[&str] = &[RUNNER_TYPE, crate::task::RUN_TYPE];

pub const DEFAULT_TIMEOUT: &str = "10m";

/// Frozen. A new shape is `runner/v2`.
pub const RUNNER_SCHEMA: &str = r#"{
  "$id": "runner/v1",
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

/// The runners the binary seeds: id, title, argv.
pub const DEFAULTS: &[(&str, &str, &[&str])] = &[
    (
        "runners/claude",
        "Claude Code, headless",
        // Not `--bare`: bare mode skips the stored login.
        &[
            "claude",
            "-p",
            "--permission-mode",
            "dontAsk",
            "--allowedTools",
            "Bash(subconscious:*)",
            "--mcp-config",
            "{mcp}",
            "--strict-mcp-config",
        ],
    ),
    (
        "runners/codex",
        "Codex CLI, non-interactive",
        // `-c approval_policy=never` works on every Codex version; `-a` does not.
        &[
            "codex",
            "exec",
            "-c",
            "approval_policy=never",
            "--sandbox",
            "workspace-write",
            "--skip-git-repo-check",
            "--output-last-message",
            "{out}",
            "-",
        ],
    ),
    ("runners/pi", "Pi, print mode", &["pi", "-p", "--no-extensions", "-"]),
];

/// Write each default runner whose id has never existed. An edited or
/// deleted default is left as the user left it. Returns the ids written.
pub fn seed_defaults(store: &mut Store) -> Result<Vec<String>, StoreError> {
    let mut written = Vec::new();
    for (id, title, argv) in DEFAULTS {
        if store.exists(id)? {
            continue;
        }
        let input: PutInput = serde_json::from_value(json!({
            "_id": id,
            "_type": RUNNER_TYPE,
            "title": title,
            "argv": argv,
            "timeout": DEFAULT_TIMEOUT,
        }))?;
        store.put(input)?;
        written.push(id.to_string());
    }
    Ok(written)
}

#[derive(Debug, Clone)]
pub struct Runner {
    pub id: String,
    pub argv: Vec<String>,
    pub timeout_secs: u64,
}

impl Runner {
    pub fn from_doc(doc: &Doc) -> Result<Runner, StoreError> {
        if doc.type_id.as_deref() != Some(RUNNER_TYPE) {
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
        Ok(Runner {
            id: doc.id.clone(),
            argv,
            timeout_secs: parse_duration(timeout)?,
        })
    }

    pub fn get(store: &Store, id: &str) -> Result<Runner, StoreError> {
        Runner::from_doc(&store.get(id)?)
    }
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
            .map(|arg| {
                pairs.iter().fold(arg.clone(), |acc, (name, value)| acc.replace(&format!("{{{name}}}"), value))
            })
            .collect()
    }

    /// The same values as `SUBCONSCIOUS_*` variables, plus `PATH` with this
    /// binary's directory first so an agent's shell finds `subconscious`.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .pairs()
            .into_iter()
            .map(|(name, value)| (format!("SUBCONSCIOUS_{}", name.to_ascii_uppercase()), value))
            .collect();
        env.push(("SUBCONSCIOUS_ACTOR".into(), self.task.clone()));
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
        json!({
            "mcpServers": {
                "subconscious": {
                    "command": self.exe,
                    "args": ["--db", self.db, "--actor", self.task, "serve"],
                }
            }
        })
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
            exe: "/bin/subconscious".into(),
        }
    }

    #[test]
    fn tokens_are_replaced_per_element() {
        let argv: Vec<String> = ["x", "--db={db}", "{task}:{run}", "{missing}", "a {out} b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            ctx().resolve(&argv),
            ["x", "--db=/v/vault.db", "tasks/t:runs/tasks/t/1", "{missing}", "a /tmp/r/out b"]
        );
    }

    #[test]
    fn env_names_everything_and_prepends_exe_dir() {
        let env = ctx().env();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap();
        assert_eq!(get("SUBCONSCIOUS_DB"), "/v/vault.db");
        assert_eq!(get("SUBCONSCIOUS_TASK"), "tasks/t");
        assert_eq!(get("SUBCONSCIOUS_ACTOR"), "tasks/t");
        assert_eq!(get("SUBCONSCIOUS_MCP"), "/tmp/r/mcp.json");
        assert!(get("PATH").starts_with("/bin"));
        let cfg: Value = serde_json::from_str(&ctx().mcp_config()).unwrap();
        assert_eq!(cfg["mcpServers"]["subconscious"]["args"][3], "tasks/t");
    }

    #[test]
    fn runner_from_doc_checks_type_and_argv() {
        let mut doc = Doc {
            id: "runners/x".into(),
            rev: "1-a".into(),
            parent: None,
            type_id: Some(RUNNER_TYPE.into()),
            deleted: false,
            created_at: "t".into(),
            actor: None,
            seq: None,
            body: serde_json::from_value(json!({"argv": ["cat"], "timeout": "2m"})).unwrap(),
        };
        let r = Runner::from_doc(&doc).unwrap();
        assert_eq!(r.argv, ["cat"]);
        assert_eq!(r.timeout_secs, 120);
        doc.type_id = Some("note/v1".into());
        assert!(Runner::from_doc(&doc).is_err());
    }
}
