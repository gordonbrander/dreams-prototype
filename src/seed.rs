//! Seeding: the built-in documents every vault starts with. `seed` writes
//! each one only when its id has never existed, so an edited or deleted
//! default stays as the user left it. `restore` writes the defaults again
//! over whatever is there; earlier revisions stay in history.

use serde_json::{Value, json};

use crate::doc::PutInput;
use crate::error::StoreError;
use crate::prompt::{self, PROMPT_TYPE};
use crate::runner::{self, RUNNER_TYPE};
use crate::skill::{self, SKILL_TYPE};
use crate::store::Store;
use crate::task;

/// The seeded schema documents: id and body.
pub const SCHEMAS: &[(&str, &str)] = &[
    ("schemas/task", task::TASK_SCHEMA),
    ("schemas/run", task::RUN_SCHEMA),
    ("schemas/runner", runner::RUNNER_SCHEMA),
    ("schemas/skill", skill::SKILL_SCHEMA),
    ("schemas/prompt", prompt::PROMPT_SCHEMA),
    ("schemas/daily", DAILY_SCHEMA),
];

/// The body of `schemas/daily`: one note per day, see `seed/daily-note.md`.
pub const DAILY_SCHEMA: &str = r#"{
  "title": "Daily note",
  "description": "One document per day. The _id is the local date as YYYY-MM-DD. content is the log for the day; intention is the one intention for the day.",
  "type": "object",
  "properties": {
    "title": {"type": "string"},
    "content": {"type": "string"},
    "tags": {"type": "array", "items": {"type": "string"}},
    "intention": {"type": "string"}
  }
}"#;

/// The runners the binary seeds: id, title, argv.
pub const RUNNERS: &[(&str, &str, &[&str])] = &[
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
            "Bash(dreams:*),mcp__dreams",
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

/// Every built-in document as put input, schemas first so that typed
/// documents pin to them.
pub fn defaults() -> Vec<PutInput> {
    let schemas = SCHEMAS.iter().map(|(id, body)| {
        let mut value: Value = serde_json::from_str(body).expect("seeded schemas are valid JSON");
        value["_id"] = json!(id);
        value
    });
    let runners = RUNNERS.iter().map(|(id, title, argv)| {
        json!({"_id": id, "_type": RUNNER_TYPE, "title": title, "argv": argv, "timeout": runner::DEFAULT_TIMEOUT})
    });
    let skills = [json!({
        "_id": "skills/daily-note",
        "_type": SKILL_TYPE,
        "name": "daily-note",
        "description": "Create, add to, or find daily notes: one document per day, with the date as its id. \
            Use when the user mentions today's note, a daily note, a journal, or a log for a day.",
        "content": include_str!("seed/daily-note.md"),
    })];
    let prompts = [
        json!({
            "_id": "prompts/daily",
            "_type": PROMPT_TYPE,
            "name": "daily",
            "description": "Add text to today's daily note.",
            "content": "Use the daily-note skill. Add the text the user gave with this command to today's \
                daily note. If the user gave no text, ask what to add.",
        }),
        json!({
            "_id": "prompts/intention",
            "_type": PROMPT_TYPE,
            "name": "intention",
            "description": "Set today's intention in the daily note.",
            "content": "Use the daily-note skill. Set today's intention to the text the user gave with this \
                command. If the user gave no text, ask for the intention.",
        }),
    ];
    schemas
        .chain(runners)
        .chain(skills)
        .chain(prompts)
        .map(|v| serde_json::from_value(v).expect("seeded documents are valid put input"))
        .collect()
}

/// Write each built-in document whose id has never existed. Every product
/// entry point calls this; the library never writes on open. Returns the
/// ids written.
pub fn seed(store: &mut Store) -> Result<Vec<String>, StoreError> {
    let legacy: bool = store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM docs WHERE _type IS NOT NULL AND _type NOT LIKE 'doc://%')",
        [],
        |r| r.get(0),
    )?;
    if legacy {
        return Err(StoreError::invalid("this vault predates doc:// types; delete it and start again"));
    }
    write_missing(store, defaults())
}

/// Seed only the schema documents.
pub fn seed_schemas(store: &mut Store) -> Result<Vec<String>, StoreError> {
    write_missing(store, defaults().into_iter().take(SCHEMAS.len()).collect())
}

fn write_missing(store: &mut Store, docs: Vec<PutInput>) -> Result<Vec<String>, StoreError> {
    let mut written = Vec::new();
    for input in docs {
        let id = input.id.clone().expect("seeded documents have ids");
        if store.exists(&id)? {
            continue;
        }
        store.put(input)?;
        written.push(id);
    }
    Ok(written)
}

/// Write every built-in document whose current revision differs from the
/// default, as the next revision. A deleted default revives. Returns the
/// ids written.
pub fn restore(store: &mut Store) -> Result<Vec<String>, StoreError> {
    let mut written = seed(store)?;
    for mut input in defaults() {
        let id = input.id.clone().expect("seeded documents have ids");
        let parent = match store.get(&id) {
            Ok(head) if head.type_path() == input.type_id.as_deref() && head.body == input.body => continue,
            Ok(head) => head.rev,
            Err(StoreError::Deleted { rev, .. }) => rev,
            Err(e) => return Err(e),
        };
        input.parent = Some(parent);
        store.put(input)?;
        written.push(id);
    }
    Ok(written)
}
