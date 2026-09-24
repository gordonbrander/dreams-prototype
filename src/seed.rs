//! Seeding: the built-in documents every vault starts with. `seed` writes
//! each default whose current revision differs from it, as the next
//! revision, and revives deleted ones. Earlier revisions stay in history.
//! The CLI seeds on `init`, on `restore-defaults`, and when it creates a database.

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
    ("schemas/bookmark", BOOKMARK_SCHEMA),
];

/// The body of `schemas/daily`: one note per day, see `seed/daily-note.md`.
pub const DAILY_SCHEMA: &str = r#"{
  "title": "Daily note",
  "description": "One document per day. The _id is the local date as YYYY-MM-DD.md. content is the log for the day; intention is the one intention for the day.",
  "type": "object",
  "properties": {
    "title": {"type": "string"},
    "content": {"type": "string"},
    "tags": {"type": "array", "items": {"type": "string"}},
    "intention": {"type": "string"}
  }
}"#;

/// The body of `schemas/bookmark`: one saved web page, see `seed/bookmark.md`.
pub const BOOKMARK_SCHEMA: &str = r#"{
  "title": "Bookmark",
  "description": "A saved web page. The _id is bookmarks/<slug>.md, with the slug made from url. content is a summary of the page and any notes from the user.",
  "type": "object",
  "required": ["url"],
  "properties": {
    "title": {"type": "string"},
    "url": {"type": "string"},
    "content": {"type": "string"},
    "tags": {"type": "array", "items": {"type": "string"}}
  }
}"#;

/// The runners the binary seeds: id, title, argv.
pub const RUNNERS: &[(&str, &str, &[&str])] = &[
    (
        "runners/claude",
        "Claude Code, headless",
        // Not `--bare`: bare mode skips the stored login. Bash runs in the
        // sandbox: it writes only in the cwd and has no network, and
        // `dreams` runs outside it to write the vault. `Edit(./**)` covers
        // every file-writing tool.
        &[
            "claude",
            "-p",
            "--permission-mode",
            "dontAsk",
            "--allowedTools",
            "Bash,Read,Glob,Grep,Edit(./**),WebSearch,WebFetch,mcp__dreams",
            "--settings",
            r#"{"sandbox":{"enabled":true,"autoAllowBashIfSandboxed":true,"excludedCommands":["dreams"]}}"#,
            "--mcp-config",
            "{mcp}",
            "--strict-mcp-config",
        ],
    ),
    (
        "runners/codex",
        "Codex CLI, non-interactive",
        // `-c approval_policy=never` works on every Codex version; `-a` does not.
        // The workspace-write sandbox writes only in the cwd.
        &[
            "codex",
            "exec",
            "-c",
            "approval_policy=never",
            "-c",
            "tools.web_search=true",
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
    let skills = [
        json!({
            "_id": "skills/daily-note",
            "_type": SKILL_TYPE,
            "name": "daily-note",
            "description": "Create, add to, or find daily notes: one document per day, with the date as its id. \
                Use when the user mentions today's note, a daily note, a journal, or a log for a day.",
            "content": include_str!("seed/daily-note.md"),
        }),
        json!({
            "_id": "skills/bookmark",
            "_type": SKILL_TYPE,
            "name": "bookmark",
            "description": "Save a web page as a bookmark: fetch it, summarize it, and tag it. \
                Use when the user wants to bookmark, clip, or save a link, or find saved links.",
            "content": include_str!("seed/bookmark.md"),
        }),
        json!({
            "_id": "skills/brief",
            "_type": SKILL_TYPE,
            "name": "brief",
            "description": "Make a daily brief: food for thought that brings back ideas from the user's notes, \
                with today's intention as its theme. Use when the user asks for a brief or a daily review.",
            "content": include_str!("seed/brief.md"),
        }),
    ];
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
        json!({
            "_id": "prompts/bookmark",
            "_type": PROMPT_TYPE,
            "name": "bookmark",
            "description": "Save a URL as a bookmark.",
            "content": "Use the bookmark skill. Save the URL the user gave with this command. Use the other \
                text the user gave as notes. If the user gave no URL, ask for one.",
        }),
        json!({
            "_id": "prompts/brief",
            "_type": PROMPT_TYPE,
            "name": "brief",
            "description": "Make today's brief and show it.",
            "content": "Use the brief skill. Make today's brief and show it to the user. \
                Do not write it to the vault.",
        }),
    ];
    // Seeded tasks are templates: each runs only where the user deploys it.
    let tasks = [json!({
        "_id": "tasks/brief",
        "_type": task::TASK_TYPE,
        "title": "Daily brief",
        "runner": "doc://runners/claude",
        "every": "1d",
        "prompt": "Use the brief skill and the daily-note skill. Get today's daily note. If its content \
            already has a \"## Brief\" heading, stop. If not, make today's brief. Then add \
            \"## Brief\", an empty line, and the brief to the end of the note's content, \
            with the steps in \"Add to a daily note\".",
    })];
    schemas
        .chain(runners)
        .chain(skills)
        .chain(prompts)
        .chain(tasks)
        .map(|v| serde_json::from_value(v).expect("seeded documents are valid put input"))
        .collect()
}

/// Write every built-in document whose current revision differs from the
/// default, as the next revision. A deleted default revives. Returns the
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
    write_defaults(store, defaults())
}

/// Seed only the schema documents.
pub fn seed_schemas(store: &mut Store) -> Result<Vec<String>, StoreError> {
    write_defaults(store, defaults().into_iter().take(SCHEMAS.len()).collect())
}

fn write_defaults(store: &mut Store, docs: Vec<PutInput>) -> Result<Vec<String>, StoreError> {
    let mut written = Vec::new();
    for mut input in docs {
        let id = input.id.clone().expect("seeded documents have ids");
        input.parent = match store.get(&id) {
            Ok(head) if head.type_path() == input.type_id.as_deref() && head.body == input.body => continue,
            Ok(head) => Some(head.rev),
            Err(StoreError::Deleted { rev, .. }) => Some(rev),
            Err(StoreError::NotFound { .. }) => None,
            Err(e) => return Err(e),
        };
        store.put(input)?;
        written.push(id);
    }
    Ok(written)
}
