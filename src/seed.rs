//! Seeding: the built-in documents every vault starts with. Each one is a
//! file under `src/seed/`, and its path there is its `_id`: a document with
//! `content` is Markdown with frontmatter, and any other is JSON. `seed`
//! writes each default whose current revision differs from it, as the next
//! revision, and revives deleted ones. Earlier revisions stay in history.
//! The CLI seeds on `init`, on `restore-defaults`, and when it creates a database.

use std::path::Path;

use serde_json::Value;

use crate::doc::PutInput;
use crate::error::StoreError;
use crate::format::{detect_format, parse_input};
use crate::store::Store;

/// One seed file: its `_id`, and its text.
macro_rules! seed {
    ($id:literal) => {
        ($id, include_str!(concat!("seed/", $id)))
    };
}

/// Every seed file. Schemas come first, so that typed documents pin to them.
pub const FILES: &[(&str, &str)] = &[
    seed!("schemas/task.json"),
    seed!("schemas/run.json"),
    seed!("schemas/runner.json"),
    seed!("schemas/skill.json"),
    seed!("schemas/prompt.json"),
    seed!("schemas/daily.json"),
    seed!("schemas/bookmark.json"),
    seed!("schemas/feed.json"),
    seed!("schemas/feed-item.json"),
    // Not `--bare`: bare mode skips the stored login. Bash runs in the
    // sandbox: it writes only in the cwd and has no network, and `dreams`
    // runs outside it to write the vault. `Edit(./**)` covers every
    // file-writing tool.
    seed!("runners/claude.json"),
    // `-c approval_policy=never` works on every Codex version; `-a` does not.
    // The workspace-write sandbox writes only in the cwd.
    seed!("runners/codex.json"),
    seed!("runners/pi.json"),
    // Not an agent: pulls every feed, as the task's actor. The prompt is not read.
    seed!("runners/feeds.json"),
    seed!("skills/dreams.md"),
    seed!("skills/daily-note.md"),
    seed!("skills/bookmark.md"),
    seed!("skills/brief.md"),
    seed!("prompts/daily.md"),
    seed!("prompts/intention.md"),
    seed!("prompts/bookmark.md"),
    seed!("prompts/brief.md"),
    // Seeded tasks are templates: each runs only where the user deploys it.
    seed!("tasks/brief.json"),
    seed!("tasks/pull-feeds.json"),
];

/// The seeded schema documents are under this prefix.
const SCHEMAS: &str = "schemas/";

/// One seed file as put input. The path is the `_id`.
fn parse(id: &str, text: &str) -> PutInput {
    let format = detect_format(Path::new(id)).unwrap_or_else(|| panic!("seed {id} has no known extension"));
    let mut map = parse_input(text, format).unwrap_or_else(|e| panic!("seed {id}: {e}"));
    map.insert("_id".into(), Value::String(id.to_string()));
    serde_json::from_value(Value::Object(map)).unwrap_or_else(|e| panic!("seed {id}: {e}"))
}

/// Every built-in document as put input, in the order of `FILES`.
pub fn defaults() -> Vec<PutInput> {
    FILES.iter().map(|(id, text)| parse(id, text)).collect()
}

/// Write every built-in document whose current revision differs from the
/// default, as the next revision. A deleted default revives. Returns the
/// ids written.
pub fn seed(store: &mut Store) -> Result<Vec<String>, StoreError> {
    write_defaults(store, defaults())
}

/// Seed only the schema documents.
pub fn seed_schemas(store: &mut Store) -> Result<Vec<String>, StoreError> {
    let schemas = defaults().into_iter().filter(|d| d.id.as_deref().is_some_and(|id| id.starts_with(SCHEMAS)));
    write_defaults(store, schemas.collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn files_under(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path, root, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }

    #[test]
    fn every_seed_file_is_listed() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/seed");
        let mut on_disk = Vec::new();
        files_under(&root, &root, &mut on_disk);
        on_disk.sort();
        let mut listed: Vec<String> = FILES.iter().map(|(id, _)| id.to_string()).collect();
        listed.sort();
        assert_eq!(on_disk, listed);
    }

    #[test]
    fn seeds_follow_the_format_rule() {
        for (id, text) in FILES {
            let raw = parse_input(text, detect_format(Path::new(id)).unwrap()).unwrap();
            assert!(!raw.contains_key("_id"), "{id}: the path is the _id");
            let has_content = raw.contains_key("content");
            assert_eq!(id.ends_with(".md"), has_content, "{id}: .md if and only if it has content");
            assert!(id.ends_with(".md") || id.ends_with(".json"), "{id}");
        }
    }

    #[test]
    fn schemas_come_first() {
        let first_other = FILES.iter().position(|(id, _)| !id.starts_with(SCHEMAS)).unwrap();
        assert!(FILES[first_other..].iter().all(|(id, _)| !id.starts_with(SCHEMAS)));
    }
}
