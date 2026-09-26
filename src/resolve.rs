//! Automatic conflict resolution: an agent reads the winner, every
//! conflicting revision, and their common ancestor, and writes one merged
//! body. The store then writes the merge on the winner and tombstones the
//! losers, as `doc resolve` does by hand.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{Map, Value};

use crate::doc::{Doc, DocRef, PutInput};
use crate::error::StoreError;
use crate::runner::{self, Runner};
use crate::store::Store;

/// The runner `--auto` uses unless told otherwise.
pub const DEFAULT_RUNNER: &str = "doc://runners/claude.json";

/// A merge an agent proposed, not yet written.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// The winner the agent saw. The merge is its child.
    pub winner: Doc,
    /// The conflicts the agent saw. Resolve fails if they changed since.
    pub conflicts: Vec<String>,
    pub merged: PutInput,
    /// The pinned reference of the runner revision that wrote the merge.
    /// `None` when the field merge settled it, so no runner started.
    pub runner: Option<String>,
}

/// A merge without an agent: each field takes the one change that the sides
/// made to it. `None` when two sides changed one field in different ways,
/// or when the sides have different types.
pub fn field_merge(ancestor: &Doc, leaves: &[Doc]) -> Option<Map<String, Value>> {
    let first = leaves.first()?;
    if leaves.iter().any(|d| d.type_path() != first.type_path()) {
        return None;
    }
    let keys: std::collections::BTreeSet<&String> = leaves.iter().flat_map(|d| d.body.keys()).collect();
    let mut merged = Map::new();
    for key in keys {
        let base = ancestor.body.get(key);
        let mut changes: Vec<Option<&Value>> = Vec::new();
        for leaf in leaves {
            let value = leaf.body.get(key);
            if value != base && !changes.contains(&value) {
                changes.push(value);
            }
        }
        let value = match changes.as_slice() {
            [] => base,
            [one] => *one,
            _ => return None,
        };
        if let Some(v) = value {
            merged.insert(key.clone(), v.clone());
        }
    }
    Some(merged)
}

/// The newest revision that `winner` and every one of `others` descend
/// from. `None` when the branches share no revision, which happens when two
/// vaults each created the same id.
pub fn common_ancestor(store: &Store, winner: &str, others: &[String]) -> Result<Option<String>, StoreError> {
    let chain = store.ancestors(winner)?;
    let mut oldest = 0;
    for other in others {
        let theirs: HashSet<String> = store.ancestors(other)?.into_iter().collect();
        match chain.iter().position(|r| theirs.contains(r)) {
            Some(i) => oldest = oldest.max(i),
            None => return Ok(None),
        }
    }
    Ok(chain.get(oldest).cloned())
}

fn revision_json(doc: &Doc) -> String {
    let mut doc = doc.clone();
    doc.conflicts.clear();
    doc.deleted_conflicts.clear();
    doc.seq = None;
    serde_json::to_string_pretty(&doc).expect("documents serialize")
}

/// The instructions and the revisions, for the agent's stdin. `leaves[0]`
/// is the winner.
pub fn merge_prompt(id: &str, ancestor: Option<&Doc>, leaves: &[Doc]) -> String {
    let mut text = format!(
        "You merge conflicting revisions of the document \"{id}\" in a document vault.\n\
         Two copies of the vault edited this document at the same time, and a sync brought the edits together.\n\
         Each revision below is a JSON object. Fields whose names start with \"_\" are metadata. \
         The other fields are the document body.\n\n"
    );
    match ancestor {
        Some(a) => text.push_str(&format!(
            "The common ancestor is the last revision that all sides shared. Compare each side with it to see what that side changed.\n\
             <ancestor>\n{}\n</ancestor>\n\n",
            revision_json(a)
        )),
        None => text.push_str("The revisions share no common ancestor. Each side created the document separately.\n\n"),
    }
    if let Some((winner, others)) = leaves.split_first() {
        text.push_str(&format!(
            "The winner is the revision the vault shows now:\n<revision role=\"winner\">\n{}\n</revision>\n\n",
            revision_json(winner)
        ));
        text.push_str("The conflicting revisions:\n");
        for doc in others {
            text.push_str(&format!("<revision role=\"conflict\">\n{}\n</revision>\n", revision_json(doc)));
        }
    }
    text.push_str(
        "\nWrite one merged body:\n\
         - Keep every change that one side made and the other sides did not.\n\
         - When sides changed the same field in different ways, combine the changes if you can. If you cannot, keep the winner's value.\n\
         - Merge text fields such as \"content\" line by line, and keep the additions from every side.\n\
         - \"title\" and \"content\" must be strings. \"tags\" must be a list of strings.\n\
         - Do not add fields that no revision has.\n\n\
         Do not use any tools, and do not change the vault.\n\
         Reply with exactly one JSON object: the merged body, with no field whose name starts with \"_\". Write no other text.\n",
    );
    text
}

/// The merged body from an agent's reply: the outermost JSON object in the
/// text, so a code fence or a sentence around it does no harm. Fields that
/// start with `_` are dropped; the store sets them.
pub fn parse_merge(text: &str) -> Result<Map<String, Value>, StoreError> {
    let snippet = || text.chars().take(200).collect::<String>();
    let bad = |why: String| StoreError::Runner { message: format!("{why}; the reply began: {:?}", snippet()) };
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err(bad("the reply has no JSON object".into()));
    };
    if end < start {
        return Err(bad("the reply has no JSON object".into()));
    }
    match serde_json::from_str::<Value>(&text[start..=end]) {
        Ok(Value::Object(mut map)) => {
            map.retain(|k, _| !k.starts_with('_'));
            Ok(map)
        }
        Ok(_) => Err(bad("the reply is not a JSON object".into())),
        Err(e) => Err(bad(format!("the reply is not valid JSON: {e}"))),
    }
}

/// A merge of `id`: the field merge when it settles every field, else the
/// runner's. `None` when the document has no conflicts. Nothing is written.
pub async fn propose(store: &Store, db: &Path, id: &str, runner_ref: &str) -> Result<Option<Proposal>, StoreError> {
    let winner = store.get(id)?;
    if winner.conflicts.is_empty() {
        return Ok(None);
    }
    let mut leaves = vec![winner.clone()];
    for rev in &winner.conflicts {
        leaves.push(store.get_rev(rev)?);
    }
    let ancestor = match common_ancestor(store, &winner.rev, &winner.conflicts)? {
        Some(rev) => Some(store.get_rev(&rev)?),
        None => None,
    };
    let (body, runner) = match ancestor.as_ref().and_then(|a| field_merge(a, &leaves)) {
        Some(body) => (body, None),
        None => {
            let runner = Runner::get(store, &DocRef::from_cli(runner_ref)?.to_string())?;
            let prompt = merge_prompt(id, ancestor.as_ref(), &leaves);
            let reply = runner::invoke(&runner, db, &format!("resolve/{id}"), &prompt).await?;
            (parse_merge(&reply)?, Some(runner.pinned()))
        }
    };
    let merged = PutInput {
        id: Some(id.to_string()),
        parent: Some(winner.rev.clone()),
        // Unpinned, so the merge pins and validates against the current schema.
        type_id: winner.type_path().map(str::to_string),
        body,
    };
    Ok(Some(Proposal { conflicts: winner.conflicts.clone(), winner, merged, runner }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(rev: &str, body: Value) -> Doc {
        Doc {
            id: "n".into(),
            rev: rev.into(),
            parent: None,
            type_id: None,
            deleted: false,
            created_at: "t".into(),
            actor: None,
            seq: None,
            conflicts: vec!["x".into()],
            deleted_conflicts: Vec::new(),
            body: serde_json::from_value(body).unwrap(),
        }
    }

    #[test]
    fn parse_merge_finds_the_object() {
        let bare = parse_merge(r#"{"title": "t"}"#).unwrap();
        assert_eq!(bare["title"], "t");
        let fenced = parse_merge("```json\n{\"title\": \"t\", \"tags\": [\"a\"]}\n```").unwrap();
        assert_eq!(fenced["tags"], json!(["a"]));
        let chatty = parse_merge("Here is the merge:\n{\"content\": \"a {b} c\"}\nDone.").unwrap();
        assert_eq!(chatty["content"], "a {b} c");
        let meta = parse_merge(r#"{"_id": "x", "_rev": "1-a", "title": "t"}"#).unwrap();
        assert_eq!(meta.keys().collect::<Vec<_>>(), ["title"]);
        for bad in ["no json here", "} {", "{not json}", "[1, 2]"] {
            let err = parse_merge(bad).unwrap_err();
            assert!(matches!(err, StoreError::Runner { .. }), "{bad}: {err}");
        }
    }

    #[test]
    fn field_merge_takes_the_one_change_to_each_field() {
        let base = doc("1-a", json!({"title": "t", "content": "c", "tags": ["x"]}));
        let left = doc("2-b", json!({"title": "left", "content": "c", "tags": ["x"]}));
        let right = doc("2-c", json!({"title": "t", "content": "right"}));
        let merged = field_merge(&base, &[left.clone(), right.clone()]).unwrap();
        assert_eq!(
            Value::Object(merged),
            json!({"title": "left", "content": "right"}),
            "a removed field stays removed"
        );

        let same = doc("2-d", json!({"title": "left", "content": "c", "tags": ["x"]}));
        let merged = field_merge(&base, &[left.clone(), same]).unwrap();
        assert_eq!(merged["title"], "left", "the same change on two sides is one change");

        let keeps_tags = doc("2-e", json!({"title": "t", "content": "right", "tags": ["x"]}));
        let third = doc("3-f", json!({"title": "t", "content": "c", "tags": ["x", "y"]}));
        let merged = field_merge(&base, &[left.clone(), keeps_tags, third]).unwrap();
        assert_eq!(Value::Object(merged), json!({"title": "left", "content": "right", "tags": ["x", "y"]}));
    }

    #[test]
    fn field_merge_refuses_an_overlap_or_a_type_change() {
        let base = doc("1-a", json!({"title": "t"}));
        let left = doc("2-b", json!({"title": "left"}));
        assert!(field_merge(&base, &[left.clone(), doc("2-c", json!({"title": "right"}))]).is_none());
        let removed = doc("2-c", json!({}));
        assert!(field_merge(&base, &[left.clone(), removed]).is_none(), "a removal and an edit overlap");
        let mut typed = doc("2-d", json!({"title": "t"}));
        typed.type_id = Some("doc://schemas/note?rev=1-x".into());
        assert!(field_merge(&base, &[left, typed]).is_none());
    }

    #[test]
    fn prompt_names_the_ancestor_and_every_leaf() {
        let ancestor = doc("1-aaa", json!({"title": "base"}));
        let leaves = [doc("2-bbb", json!({"title": "left"})), doc("2-ccc", json!({"title": "right"}))];
        let p = merge_prompt("n", Some(&ancestor), &leaves);
        for needle in ["1-aaa", "base", "2-bbb", "left", "2-ccc", "right", "role=\"winner\"", "role=\"conflict\""] {
            assert!(p.contains(needle), "missing {needle}");
        }
        assert!(!p.contains("_conflicts"), "metadata the agent does not need is left out");
        assert!(merge_prompt("n", None, &leaves).contains("no common ancestor"));
    }
}
