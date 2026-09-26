//! Skills: documents typed `doc://schemas/skill`, served to MCP hosts
//! through the Skills Extension (`io.modelcontextprotocol/skills`). Each
//! skill is one file, `skill://<name>/SKILL.md`: a `name` and
//! `description` frontmatter, then `content`. When two documents share a
//! name, the most recently modified one wins.

use std::collections::HashSet;

use serde_json::{Value, json};
use serde_yaml_ng::{Mapping, Value as Yaml};

use crate::doc::Doc;
use crate::error::StoreError;
use crate::hash::sha256_hex;
use crate::store::Store;

/// The seeded schema document for skills, as a type path.
pub const SKILL_TYPE: &str = "doc://schemas/skill";

/// The MCP extension identifier.
pub const EXTENSION_ID: &str = "io.modelcontextprotocol/skills";

/// The body of `schemas/skill`. `name` follows the Agent Skills naming rule.
pub const SKILL_SCHEMA: &str = r#"{
  "title": "Skill",
  "description": "Instructions an agent loads on demand. Served over MCP as skill://<name>/SKILL.md, with name and description as frontmatter and content as the body.",
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
pub struct Skill {
    /// `skill://<name>/SKILL.md`
    pub uri: String,
    pub name: String,
    pub description: String,
    /// The rendered SKILL.md.
    pub text: String,
}

impl Skill {
    /// A skill from a document, or `None` when a field is missing. The
    /// schema requires the fields, so `None` means the document is not a skill.
    pub fn from_doc(doc: &Doc) -> Option<Skill> {
        let field = |key: &str| doc.body.get(key).and_then(Value::as_str);
        let (name, description, content) = (field("name")?, field("description")?, field("content")?);
        let mut fm = Mapping::new();
        fm.insert(Yaml::String("name".into()), Yaml::String(name.into()));
        fm.insert(Yaml::String("description".into()), Yaml::String(description.into()));
        let yaml = serde_yaml_ng::to_string(&fm).expect("a mapping serializes");
        Some(Skill {
            uri: format!("skill://{name}/SKILL.md"),
            name: name.to_string(),
            description: description.to_string(),
            text: format!("---\n{yaml}---\n{content}"),
        })
    }

    /// The extension's Skill object: its SKILL.md as the one resource.
    pub fn entry(&self) -> Value {
        let digest = sha256_hex(self.text.as_bytes());
        json!({
            "uri": self.uri,
            "frontmatter": {"name": self.name, "description": self.description},
            "resources": [{"uri": self.uri, "digest": format!("sha256:{digest}"), "size": self.text.len()}],
        })
    }
}

/// Every skill in the vault, most recently modified first, one per name.
pub fn list(store: &Store) -> Result<Vec<Skill>, StoreError> {
    let mut seen = HashSet::new();
    Ok(store
        .list_all(Some(SKILL_TYPE))?
        .iter()
        .filter_map(Skill::from_doc)
        .filter(|s| seen.insert(s.name.clone()))
        .collect())
}

/// The skill whose SKILL.md is at `uri`.
pub fn find(store: &Store, uri: &str) -> Result<Option<Skill>, StoreError> {
    Ok(list(store)?.into_iter().find(|s| s.uri == uri))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown;
    use crate::seed;

    fn store() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        seed::seed(&mut store).unwrap();
        store
    }

    fn put(store: &mut Store, id: &str, name: &str, content: &str) {
        let input = serde_json::from_value(json!({
            "_id": id, "_type": SKILL_TYPE, "name": name, "description": "Do the thing.", "content": content,
        }))
        .unwrap();
        store.put(input).unwrap();
    }

    #[test]
    fn renders_skill_md_with_frontmatter() {
        let mut store = store();
        put(&mut store, "skills/a", "git-workflow", "# Steps\n");
        let skill = find(&store, "skill://git-workflow/SKILL.md").unwrap().unwrap();
        assert_eq!(skill.uri, "skill://git-workflow/SKILL.md");
        let parsed = markdown::parse(&skill.text).unwrap();
        assert_eq!(parsed["name"], "git-workflow");
        assert_eq!(parsed["description"], "Do the thing.");
        assert_eq!(parsed["content"], "# Steps\n");
    }

    #[test]
    fn entry_digest_and_size_match_the_text() {
        use sha2::{Digest, Sha256};
        let mut store = store();
        put(&mut store, "skills/a", "git-workflow", "# Steps\n");
        let skill = find(&store, "skill://git-workflow/SKILL.md").unwrap().unwrap();
        let entry = skill.entry();
        let digest: String = Sha256::digest(skill.text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(entry["resources"][0]["digest"], format!("sha256:{digest}"));
        assert_eq!(entry["resources"][0]["size"], skill.text.len());
        assert_eq!(entry["frontmatter"]["name"], "git-workflow");
    }

    #[test]
    fn newest_document_wins_a_shared_name() {
        let mut store = store();
        put(&mut store, "skills/old", "same", "old");
        put(&mut store, "skills/new", "same", "new");
        let skills: Vec<Skill> = list(&store).unwrap().into_iter().filter(|s| s.name == "same").collect();
        assert_eq!(skills.len(), 1);
        assert!(skills[0].text.ends_with("new"), "{}", skills[0].text);
    }

    #[test]
    fn a_seeded_vault_has_the_daily_note_skill() {
        let skill = find(&store(), "skill://daily-note/SKILL.md").unwrap().unwrap();
        assert!(skill.text.contains("tag `daily`"), "{}", skill.text);
    }

    #[test]
    fn a_seeded_vault_has_the_bookmark_skill() {
        let skill = find(&store(), "skill://bookmark/SKILL.md").unwrap().unwrap();
        assert!(skill.text.contains("tag `bookmark`"), "{}", skill.text);
        assert!(skill.text.contains("bookmarks/<origin-slug>/<path-slug>.md"), "{}", skill.text);
    }

    #[test]
    fn a_seeded_vault_has_the_brief_skill() {
        let skill = find(&store(), "skill://brief/SKILL.md").unwrap().unwrap();
        assert!(skill.text.contains("## Brief sections"), "{}", skill.text);
    }

    #[test]
    fn schema_rejects_a_bad_name() {
        let mut store = store();
        let input = serde_json::from_value(json!({
            "_type": SKILL_TYPE, "name": "Git Workflow", "description": "x", "content": "y",
        }))
        .unwrap();
        assert!(store.put(input).is_err());
    }
}
