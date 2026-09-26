//! Conflict resolution for agents. `Conflict` is the work that is left
//! after code merges what it can: fields that only one side changed merge
//! at once, and `content` merges line by line, with conflict markers where
//! both sides changed the same lines. An agent decides the rest, and replies
//! with `fields` and with `content_edits` that replace each marked block. The store
//! then writes the merge on the winner and tombstones the losers, as
//! `doc resolve` does by hand.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::diff::{self, Edit};
use crate::doc::{Doc, DocRef, PutInput};
use crate::error::StoreError;
use crate::rev::short_rev;
use crate::runner::{self, Runner};
use crate::store::{Store, check_conflicts};

/// The runner `--auto` uses unless told otherwise.
pub const DEFAULT_RUNNER: &str = "doc://runners/claude.json";

/// The field that merges line by line.
const CONTENT: &str = "content";

/// A merge an agent proposed, not yet written.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// The winner the agent saw. The merge is its child.
    pub winner: Doc,
    /// The conflicts the agent saw. Resolve fails if they changed since.
    pub conflicts: Vec<String>,
    pub merged: PutInput,
    /// The pinned reference of the runner revision that wrote the merge.
    /// `None` when code merged every field, so no runner started.
    pub runner: Option<String>,
}

/// A document's live leaves and the last revision they shared.
#[derive(Debug, Clone)]
pub struct Sides {
    pub winner: Doc,
    /// The other live leaves.
    pub others: Vec<Doc>,
    /// `None` when the leaves share no revision, or there are no others.
    pub ancestor: Option<Doc>,
}

impl Sides {
    pub fn load(store: &Store, id: &str) -> Result<Sides, StoreError> {
        let winner = store.get(id)?;
        let others = winner.conflicts.iter().map(|rev| store.get_rev(rev)).collect::<Result<Vec<_>, _>>()?;
        let ancestor = match common_ancestor(store, &winner.rev, &winner.conflicts)? {
            Some(rev) if !others.is_empty() => Some(store.get_rev(&rev)?),
            _ => None,
        };
        Ok(Sides { winner, others, ancestor })
    }
}

/// A document's conflicts, merged as far as code can.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Conflict {
    pub id: String,
    /// The winner's _rev. A merge is written on it.
    pub winner: String,
    /// The winner's _type, unpinned, so a merge validates against the current schema.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_id: Option<String>,
    /// The other live revisions. Pass them to resolve_doc.
    pub conflicts: Vec<String>,
    /// The last revision that every side shared. Absent when the sides share none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<String>,
    /// Fields that code merged: only one side changed them, or `content` merged by lines.
    pub settled: Map<String, Value>,
    /// Fields that two sides changed in different ways. Decide these.
    pub contested: Vec<Contested>,
    /// Present when `draft.content` has conflict markers where both sides changed the same lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marked: Option<Marked>,
    /// The working copy: `settled`, the winner's value of each contested
    /// field, and `content` with conflict markers when it is marked.
    pub draft: Map<String, Value>,
}

/// A field that two sides changed in different ways.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Contested {
    pub field: String,
    /// The ancestor's value. Absent when the ancestor does not have the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<Value>,
    /// The value on each side, winner first.
    pub sides: Vec<SideValue>,
}

/// The revisions of the lines in the marked blocks of `draft.content`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Marked {
    /// The `ours` lines: the winner's, or the first side that changed `content`.
    pub ours: String,
    /// The `theirs` lines: the other side's.
    pub theirs: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SideValue {
    pub rev: String,
    /// Absent when this side does not have the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SideDiff {
    pub from: String,
    pub to: String,
    pub diff: String,
}

/// An agent's decisions for a `Conflict`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct Reply {
    /// A value for each contested field. null removes the field. A contested
    /// field that is not here keeps the winner's value.
    #[serde(default)]
    pub fields: Map<String, Value>,
    /// Edits to the marked `content`, applied in order: each `old` is one whole
    /// marked block, copied exactly, and occurs once; `new` is the merged text.
    #[serde(default)]
    pub content_edits: Vec<Edit>,
}

impl Conflict {
    /// The conflicts of `id` as they are now.
    pub fn read(store: &Store, id: &str) -> Result<Conflict, StoreError> {
        Ok(Conflict::new(&Sides::load(store, id)?))
    }

    /// Merge what code can.
    pub fn new(s: &Sides) -> Conflict {
        let winner = &s.winner;
        let leaves: Vec<&Doc> = std::iter::once(winner).chain(&s.others).collect();
        // A change of type changes what the fields mean: compare values only.
        let same_type = leaves.iter().all(|d| d.type_path() == winner.type_path());
        let base = s.ancestor.as_ref().filter(|_| same_type);
        let keys: BTreeSet<&String> = leaves.iter().flat_map(|d| d.body.keys()).collect();
        let (mut settled, mut contested, mut marked, mut draft) = (Map::new(), Vec::new(), None, Map::new());
        for key in keys {
            let values: Vec<Option<&Value>> = leaves.iter().map(|d| d.body.get(key)).collect();
            let base_value = base.and_then(|a| a.body.get(key));
            // The distinct changes from the base, each with the first side that made it.
            let mut changes: Vec<(Option<&Value>, &str)> = Vec::new();
            for (value, doc) in values.iter().zip(&leaves) {
                let changed = match base {
                    Some(_) => *value != base_value,
                    None => true,
                };
                if changed && !changes.iter().any(|(v, _)| v == value) {
                    changes.push((*value, &doc.rev));
                }
            }
            let one = match (base, changes.as_slice()) {
                (Some(_), []) => Some(base_value),
                (_, [(value, _)]) => Some(*value),
                _ => None,
            };
            if let Some(value) = one {
                if let Some(v) = value {
                    settled.insert(key.clone(), v.clone());
                    draft.insert(key.clone(), v.clone());
                }
                continue;
            }
            if let (Some(_), CONTENT, [(ours, ours_rev), (theirs, theirs_rev)]) =
                (base, key.as_str(), changes.as_slice())
                && let (Some(a), Some(o), Some(t)) = (as_text(base_value), as_text(*ours), as_text(*theirs))
            {
                match diff::merge_content(a, o, t) {
                    Ok(merged) => {
                        settled.insert(key.clone(), Value::String(merged.clone()));
                        draft.insert(key.clone(), Value::String(merged));
                    }
                    Err(text) => {
                        draft.insert(key.clone(), Value::String(text));
                        marked = Some(Marked { ours: ours_rev.to_string(), theirs: theirs_rev.to_string() });
                    }
                }
                continue;
            }
            if let Some(v) = values[0] {
                draft.insert(key.clone(), v.clone());
            }
            contested.push(Contested {
                field: key.clone(),
                ancestor: base_value.cloned(),
                sides: leaves
                    .iter()
                    .zip(&values)
                    .map(|(d, v)| SideValue { rev: d.rev.clone(), value: v.cloned() })
                    .collect(),
            });
        }
        Conflict {
            id: winner.id.clone(),
            winner: winner.rev.clone(),
            type_id: winner.type_path().map(str::to_string),
            conflicts: s.others.iter().map(|d| d.rev.clone()).collect(),
            ancestor: s.ancestor.as_ref().map(|a| a.rev.clone()),
            settled,
            contested,
            marked,
            draft,
        }
    }

    /// Whether code merged every field, so there is nothing to decide.
    pub fn is_settled(&self) -> bool {
        self.contested.is_empty() && self.marked.is_none()
    }

    /// The merged body: the draft with the agent's decisions.
    pub fn apply(&self, reply: &Reply) -> Result<Map<String, Value>, StoreError> {
        let mut body = self.draft.clone();
        for (key, value) in &reply.fields {
            if !self.contested.iter().any(|c| c.field == *key) {
                let why = if key == CONTENT && self.marked.is_some() {
                    "`content` has conflict markers: use `content_edits`".to_string()
                } else {
                    let names: Vec<&str> = self.contested.iter().map(|c| c.field.as_str()).collect();
                    let names = if names.is_empty() { "none".to_string() } else { names.join(", ") };
                    format!("`fields` has \"{key}\", which is not a field to decide; decide: {names}")
                };
                return Err(StoreError::invalid(why));
            }
            match value {
                Value::Null => body.remove(key),
                v => body.insert(key.clone(), v.clone()),
            };
        }
        match (&self.marked, reply.content_edits.as_slice()) {
            (None, []) => {}
            (None, _) => {
                return Err(StoreError::invalid("`content_edits` need content with conflict markers, and it has none"));
            }
            (Some(_), edits) => {
                let text = body.get(CONTENT).and_then(Value::as_str).unwrap_or_default();
                body.insert(CONTENT.into(), Value::String(diff::apply_edits(text, edits)?));
            }
        }
        check_markers(&body)?;
        Ok(body)
    }

    /// The merge to write on the winner.
    pub fn merge(&self, reply: &Reply) -> Result<PutInput, StoreError> {
        Ok(PutInput {
            id: Some(self.id.clone()),
            parent: Some(self.winner.clone()),
            type_id: self.type_id.clone(),
            body: self.apply(reply)?,
        })
    }
}

/// A text field's value; an absent field is empty text. `None` for other values.
fn as_text(v: Option<&Value>) -> Option<&str> {
    match v {
        None => Some(""),
        Some(v) => v.as_str(),
    }
}

/// Each leaf as a diff from the ancestor, or from the winner without one.
pub fn side_diffs(s: &Sides) -> Vec<SideDiff> {
    if s.others.is_empty() {
        return Vec::new();
    }
    let (base, role) = match &s.ancestor {
        Some(a) => (a, "ancestor"),
        None => (&s.winner, "winner"),
    };
    let base_text = diff::body_text(base.type_path(), &base.body);
    let from = format!("{} ({role})", short_rev(&base.rev));
    std::iter::once((&s.winner, "winner"))
        .chain(s.others.iter().map(|d| (d, "conflict")))
        .filter(|(d, _)| d.rev != base.rev)
        .map(|(d, role)| {
            let to = format!("{} ({role})", short_rev(&d.rev));
            let text = diff::body_text(d.type_path(), &d.body);
            SideDiff { from: base.rev.clone(), to: d.rev.clone(), diff: diff::unified(&base_text, &text, &from, &to) }
        })
        .collect()
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

/// A merge that a person or an agent wrote in full must not keep conflict markers.
pub fn check_markers(body: &Map<String, Value>) -> Result<(), StoreError> {
    match body.get(CONTENT).and_then(Value::as_str) {
        Some(text) if diff::has_markers(text) => Err(StoreError::invalid("merged content still has conflict markers")),
        _ => Ok(()),
    }
}

fn json(v: &impl Serialize) -> String {
    serde_json::to_string_pretty(v).expect("values serialize")
}

/// The instructions and the work left, for the agent's stdin.
pub fn merge_prompt(c: &Conflict) -> String {
    let mut text = format!(
        "You merge conflicting revisions of the document \"{}\" in a document vault.\n\
         Two copies of the vault edited this document at the same time, and a sync brought the edits together.\n\
         Code merged every change that only one side made. You decide only what is left.\n\n",
        c.id
    );
    if !c.settled.is_empty() {
        text.push_str(&format!(
            "These fields are merged. Do not return them:\n<settled>\n{}\n</settled>\n\n",
            json(&c.settled)
        ));
    }
    if !c.contested.is_empty() {
        match c.ancestor {
            Some(_) => text.push_str(
                "Decide these fields. Each one shows its value in the last revision that the sides shared \
                 (the ancestor), and its value on each side. A value that is not shown means that the field is not there.\n",
            ),
            None => text.push_str(
                "Decide these fields. The sides share no revision: each side created the document separately. \
                 A value that is not shown means that the field is not there.\n",
            ),
        }
        for f in &c.contested {
            text.push_str(&format!("<field name=\"{}\">\n", f.field));
            if let Some(a) = &f.ancestor {
                text.push_str(&format!("<ancestor>\n{}\n</ancestor>\n", json(a)));
            }
            for (i, side) in f.sides.iter().enumerate() {
                let role = if i == 0 { "winner" } else { "conflict" };
                match &side.value {
                    Some(v) => text.push_str(&format!(
                        "<side rev=\"{}\" role=\"{role}\">\n{}\n</side>\n",
                        short_rev(&side.rev),
                        json(v)
                    )),
                    None => text.push_str(&format!("<side rev=\"{}\" role=\"{role}\"/>\n", short_rev(&side.rev))),
                }
            }
            text.push_str("</field>\n");
        }
        text.push('\n');
    }
    if let Some(m) = &c.marked {
        let content = c.draft.get(CONTENT).and_then(Value::as_str).unwrap_or_default();
        text.push_str(&format!(
            "The field \"{CONTENT}\" is merged line by line. Where both sides changed the same lines, it has a marked block. \
             Each marker line starts with 7 or more of one character:\n\
             - \"<<<<<<< ours\" starts the lines of revision {}.\n\
             - \"||||||| original\" starts the lines of the ancestor.\n\
             - \"=======\" starts the lines of revision {}.\n\
             - \">>>>>>> theirs\" ends the block.\n\
             <content>\n{content}</content>\n\n",
            short_rev(&m.ours),
            short_rev(&m.theirs),
        ));
    }
    text.push_str(
        "Rules:\n\
         - For each field that you decide, combine the changes of every side if you can. If you cannot, use the winner's value. \
         Put the value in \"fields\". null removes the field.\n\
         - For each marked block, write one edit in \"content_edits\". \"old\" is the whole block, from its \"<<<<<<< ours\" line through its \
         \">>>>>>> theirs\" line, copied exactly. \"new\" is the merged text: keep the additions from every side, \
         and remove the markers. Edit only the marked blocks.\n\
         - \"title\" and \"content\" must be strings. \"tags\" must be a list of strings.\n\n\
         Do not use any tools, and do not change the vault.\n\
         Reply with exactly one JSON object, and write no other text:\n\
         {\"fields\": {\"<field>\": <value>}, \"content_edits\": [{\"old\": \"<block>\", \"new\": \"<merged text>\"}]}\n",
    );
    text
}

/// The agent's reply: the outermost JSON object in the text, so a code
/// fence or a sentence around it does no harm.
pub fn parse_reply(text: &str) -> Result<Reply, StoreError> {
    let snippet = || text.chars().take(200).collect::<String>();
    let bad = |why: String| StoreError::Runner { message: format!("{why}; the reply began: {:?}", snippet()) };
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err(bad("the reply has no JSON object".into()));
    };
    if end < start {
        return Err(bad("the reply has no JSON object".into()));
    }
    match serde_json::from_str::<Value>(&text[start..=end]) {
        Ok(v @ Value::Object(_)) => {
            serde_json::from_value(v).map_err(|e| bad(format!("the reply is not a merge: {e}")))
        }
        Ok(_) => Err(bad("the reply is not a JSON object".into())),
        Err(e) => Err(bad(format!("the reply is not valid JSON: {e}"))),
    }
}

/// A merge of `id`: code's merge when it settles every field, else the
/// runner's decisions on the rest. `None` when the document has no
/// conflicts. Nothing is written.
pub async fn propose(store: &Store, db: &Path, id: &str, runner_ref: &str) -> Result<Option<Proposal>, StoreError> {
    let sides = Sides::load(store, id)?;
    if sides.others.is_empty() {
        return Ok(None);
    }
    let conflict = Conflict::new(&sides);
    let (merged, runner) = if conflict.is_settled() {
        (conflict.merge(&Reply::default())?, None)
    } else {
        let runner = Runner::get(store, &DocRef::from_cli(runner_ref)?.to_string())?;
        let reply = runner::invoke(&runner, db, &format!("resolve/{id}"), &merge_prompt(&conflict)).await?;
        let merged = conflict
            .merge(&parse_reply(&reply)?)
            .map_err(|e| StoreError::Runner { message: format!("the merge does not apply: {e}") })?;
        (merged, Some(runner.pinned()))
    };
    Ok(Some(Proposal { winner: sides.winner, conflicts: conflict.conflicts, merged, runner }))
}

/// Resolve `id` with an agent's decisions on the conflicts it read. Fails
/// when the conflicts changed since, so the draft is the one it read.
pub fn resolve_with(store: &mut Store, id: &str, conflicts: &[String], reply: &Reply) -> Result<Doc, StoreError> {
    let conflict = Conflict::read(store, id)?;
    check_conflicts(id, &conflict.winner, &conflict.conflicts, conflicts)?;
    store.resolve(id, Some(conflict.merge(reply)?), Some(conflicts))
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

    fn side(rev: &str, body: Value) -> Doc {
        Doc { conflicts: Vec::new(), ..doc(rev, body) }
    }

    fn conflict(winner: &Doc, others: &[Doc], ancestor: Option<&Doc>) -> Conflict {
        Conflict::new(&Sides { winner: winner.clone(), others: others.to_vec(), ancestor: ancestor.cloned() })
    }

    fn reply(v: Value) -> Reply {
        serde_json::from_value(v).unwrap()
    }

    fn fields(c: &Conflict) -> Vec<&str> {
        c.contested.iter().map(|f| f.field.as_str()).collect()
    }

    #[test]
    fn parse_reply_finds_the_object() {
        let bare = parse_reply(r#"{"fields": {"title": "t"}}"#).unwrap();
        assert_eq!(bare.fields["title"], "t");
        let fenced = parse_reply("```json\n{\"content_edits\": [{\"old\": \"a\", \"new\": \"b\"}]}\n```").unwrap();
        assert_eq!(fenced.content_edits[0].new, "b");
        let chatty = parse_reply("Here is the merge:\n{\"fields\": {\"content\": \"a {b} c\"}}\nDone.").unwrap();
        assert_eq!(chatty.fields["content"], "a {b} c");
        for bad in ["no json here", "} {", "{not json}", "[1, 2]", r#"{"content_edits": "no"}"#] {
            let err = parse_reply(bad).unwrap_err();
            assert!(matches!(err, StoreError::Runner { .. }), "{bad}: {err}");
        }
    }

    #[test]
    fn fields_that_one_side_changed_settle() {
        let base = side("1-a", json!({"title": "t", "content": "c", "tags": ["x"]}));
        let winner = doc("2-b", json!({"title": "left", "content": "c", "tags": ["x"]}));
        let other = side("2-c", json!({"title": "t", "content": "c"}));
        let c = conflict(&winner, &[other], Some(&base));
        assert!(c.contested.is_empty());
        assert_eq!(Value::Object(c.settled), json!({"title": "left", "content": "c"}), "a removed field stays removed");
        assert_eq!(c.conflicts, ["2-c"]);
        assert_eq!(c.ancestor.as_deref(), Some("1-a"));
        let sides =
            Sides { winner: winner.clone(), others: vec![side("2-c", json!({}))], ancestor: Some(base.clone()) };
        assert_eq!(side_diffs(&sides).len(), 2);

        let same = side("2-d", json!({"title": "left", "content": "c", "tags": ["x"]}));
        let c = conflict(&winner, &[same], Some(&base));
        assert!(c.contested.is_empty(), "the same change on two sides is one change");
    }

    #[test]
    fn content_merges_by_lines() {
        let base = side("1-a", json!({"content": "one\ntwo\nthree\n"}));
        let winner = doc("2-b", json!({"content": "ONE\ntwo\nthree\n"}));
        let apart = side("2-c", json!({"content": "one\ntwo\nTHREE\n"}));
        let c = conflict(&winner, &[apart], Some(&base));
        assert!(c.contested.is_empty());
        assert_eq!(c.settled["content"], "ONE\ntwo\nTHREE\n");

        let winner = doc("2-b", json!({"title": "w", "content": "one\ntwo\nthree\nfour a\n"}));
        let other = side("2-c", json!({"title": "o", "content": "one\ntwo\nthree\nfour b\n"}));
        let c = conflict(&winner, &[other], Some(&base));
        assert_eq!(fields(&c), ["title"]);
        let marked = c.marked.as_ref().unwrap();
        assert_eq!((marked.ours.as_str(), marked.theirs.as_str()), ("2-b", "2-c"));
        let draft = c.draft["content"].as_str().unwrap();
        assert!(draft.starts_with("one\ntwo\nthree\n<<<<<<< ours\nfour a\n"), "{draft}");
        assert_eq!(c.draft["title"], "w", "the draft has the winner's value");
        assert_eq!(c.contested[0].sides.len(), 2);

        let block = &draft["one\ntwo\nthree\n".len()..];
        let edits = json!([{"old": block, "new": "four a\nfour b\n"}]);
        let body = c.apply(&reply(json!({"fields": {"title": "both"}, "content_edits": edits}))).unwrap();
        assert_eq!(Value::Object(body), json!({"title": "both", "content": "one\ntwo\nthree\nfour a\nfour b\n"}));
        let merged = c.merge(&reply(json!({"content_edits": edits}))).unwrap();
        assert_eq!((merged.id.as_deref(), merged.parent.as_deref()), (Some("n"), Some("2-b")));

        let err = |r: Value| c.apply(&reply(r)).unwrap_err().to_string();
        assert!(err(json!({})).contains("still has conflict markers"));
        assert!(err(json!({"content_edits": [{"old": "one\n", "new": "1\n"}]})).contains("still has conflict markers"));
        let unknown = err(json!({"fields": {"tags": ["no"]}, "content_edits": edits}));
        assert!(unknown.contains("\"tags\", which is not a field to decide; decide: title"), "{unknown}");
        assert!(err(json!({"fields": {"content": "x"}, "content_edits": edits})).contains("use `content_edits`"));
    }

    #[test]
    fn some_conflicts_stay_whole() {
        // three sides that change content: no line merge
        let base = side("1-a", json!({"content": "c\n"}));
        let winner = doc("2-b", json!({"content": "b\n"}));
        let c = conflict(
            &winner,
            &[side("2-c", json!({"content": "x\n"})), side("2-d", json!({"content": "y\n"}))],
            Some(&base),
        );
        assert!(c.marked.is_none() && c.contested[0].sides.len() == 3);
        let err =
            c.apply(&Reply { content_edits: vec![Edit { old: "b".into(), new: "z".into() }], ..Default::default() });
        assert!(err.unwrap_err().to_string().contains("`content_edits` need content"));
        let kept = c.apply(&Reply::default()).unwrap();
        assert_eq!(kept["content"], "b\n", "a field without a decision keeps the winner's value");
        let removed =
            c.apply(&Reply { fields: serde_json::from_value(json!({"content": null})).unwrap(), ..Default::default() });
        assert!(!removed.unwrap().contains_key("content"));

        // no ancestor: only values that every side has settle
        let winner = doc("1-b", json!({"title": "same", "content": "b\n"}));
        let c = conflict(&winner, &[side("1-c", json!({"title": "same", "content": "c\n"}))], None);
        assert_eq!(fields(&c), ["content"]);
        assert!(c.marked.is_none());
        assert_eq!(c.settled["title"], "same");

        // a changed type: compare values only
        let mut typed = side("2-c", json!({"title": "t", "tags": ["x"]}));
        typed.type_id = Some("doc://schemas/note?rev=1-x".into());
        let base = side("1-a", json!({"title": "t"}));
        let c = conflict(&doc("2-b", json!({"title": "left"})), &[typed], Some(&base));
        assert_eq!(fields(&c), ["tags", "title"]);
    }

    #[test]
    fn prompt_shows_only_what_is_left() {
        let base = side("1-aaa", json!({"title": "base", "tags": ["t"], "content": "one\n"}));
        let winner = doc("2-bbb", json!({"title": "left", "tags": ["t", "w"], "content": "one\nleft\n"}));
        let other = side("2-ccc", json!({"title": "right", "tags": ["t"], "content": "one\nright\n"}));
        let c = conflict(&winner, &[other], Some(&base));
        let p = merge_prompt(&c);
        for needle in
            ["<settled>", "\"w\"", "<field name=\"title\">", "\"base\"", "role=\"winner\"", "role=\"conflict\""]
        {
            assert!(p.contains(needle), "missing {needle}: {p}");
        }
        assert!(p.contains("revision 2-bbb") && p.contains("revision 2-ccc"), "{p}");
        assert!(p.contains("<<<<<<< ours\nleft\n"), "{p}");
        assert!(!p.contains("_conflicts"), "metadata the agent does not need is left out");
    }
}
