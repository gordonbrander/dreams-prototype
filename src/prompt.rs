//! Prompts: documents typed `doc://schemas/prompt`, served as MCP prompts.
//! A prompt is instructions only: it declares no arguments. The user's own
//! text reaches the model beside it, as the user's input. When two
//! documents share a name, the most recently modified one wins.

use std::collections::HashSet;

use serde_json::Value;

use crate::doc::Doc;
use crate::error::StoreError;
use crate::store::Store;

/// The seeded schema document for prompts, as a type path.
pub const PROMPT_TYPE: &str = "doc://schemas/prompt";

/// The body of `schemas/prompt`. `name` follows the skill naming rule.
pub const PROMPT_SCHEMA: &str = r#"{
  "title": "Prompt",
  "description": "Instructions a user runs as a command. Served over MCP as a prompt named <name>, with no arguments; content is the message.",
  "type": "object",
  "required": ["name", "description", "content"],
  "properties": {
    "name": {"type": "string", "maxLength": 64, "pattern": "^[a-z0-9]+(-[a-z0-9]+)*$"},
    "description": {"type": "string", "minLength": 1, "maxLength": 1024},
    "content": {"type": "string"},
    "title": {"type": "string"},
    "tags": {"type": "array", "items": {"type": "string"}}
  }
}"#;

#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub name: String,
    pub description: String,
    pub content: String,
}

impl Prompt {
    /// A prompt from a document, or `None` when a field is missing.
    pub fn from_doc(doc: &Doc) -> Option<Prompt> {
        let field = |key: &str| doc.body.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Prompt { name: field("name")?, description: field("description")?, content: field("content")? })
    }
}

/// Every prompt in the vault, most recently modified first, one per name.
pub fn list(store: &Store) -> Result<Vec<Prompt>, StoreError> {
    let mut seen = HashSet::new();
    Ok(store
        .list_all(Some(PROMPT_TYPE))?
        .iter()
        .filter_map(Prompt::from_doc)
        .filter(|p| seen.insert(p.name.clone()))
        .collect())
}

/// The prompt named `name`.
pub fn find(store: &Store, name: &str) -> Result<Option<Prompt>, StoreError> {
    Ok(list(store)?.into_iter().find(|p| p.name == name))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::seed;

    fn store() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        seed::seed(&mut store).unwrap();
        store
    }

    fn put(store: &mut Store, id: &str, name: &str, content: &str) {
        let input = serde_json::from_value(json!({
            "_id": id, "_type": PROMPT_TYPE, "name": name, "description": "Do the thing.", "content": content,
        }))
        .unwrap();
        store.put(input).unwrap();
    }

    #[test]
    fn finds_a_prompt_by_name() {
        let mut store = store();
        put(&mut store, "prompts/a", "standup", "Summarize yesterday.");
        let prompt = find(&store, "standup").unwrap().unwrap();
        assert_eq!(prompt.description, "Do the thing.");
        assert_eq!(prompt.content, "Summarize yesterday.");
    }

    #[test]
    fn newest_document_wins_a_shared_name() {
        let mut store = store();
        put(&mut store, "prompts/old", "same", "old");
        put(&mut store, "prompts/new", "same", "new");
        let prompts: Vec<Prompt> = list(&store).unwrap().into_iter().filter(|p| p.name == "same").collect();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].content, "new");
    }

    #[test]
    fn a_seeded_vault_has_the_daily_prompt() {
        assert!(find(&store(), "daily").unwrap().unwrap().content.contains("daily-note skill"));
    }

    #[test]
    fn a_seeded_vault_has_the_intention_prompt() {
        assert!(find(&store(), "intention").unwrap().unwrap().content.contains("intention"));
    }

    #[test]
    fn a_seeded_vault_has_the_bookmark_prompt() {
        assert!(find(&store(), "bookmark").unwrap().unwrap().content.contains("bookmark skill"));
    }

    #[test]
    fn a_seeded_vault_has_the_brief_prompt() {
        assert!(find(&store(), "brief").unwrap().unwrap().content.contains("brief skill"));
    }

    #[test]
    fn schema_rejects_a_bad_name() {
        let mut store = store();
        let input = serde_json::from_value(json!({
            "_type": PROMPT_TYPE, "name": "Stand Up", "description": "x", "content": "y",
        }))
        .unwrap();
        assert!(store.put(input).is_err());
    }
}
