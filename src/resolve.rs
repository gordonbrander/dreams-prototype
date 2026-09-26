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
use crate::store::Store;

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

/// A document's conflicts, merged as far as code can.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Conflict {
    pub id: String,
    /// The winner's _rev. A merge is written on it.
    pub winner: String,
    /// The other live revisions. Pass them to resolve_doc.
    pub conflicts: Vec<String>,
    /// The last revision that every side shared. Absent when the sides share none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<String>,
    /// Fields that code merged: only one side changed them, or `content` merged by lines.
    pub settled: Map<String, Value>,
    /// Fields that two sides changed in different ways. Decide these.
    pub contested: Vec<Contested>,
    /// The merged body so far: `settled`, plus the winner's value of each
    /// contested field, and `content` with conflict markers when it is marked.
    pub draft: Map<String, Value>,
    /// Each side's body as a unified diff from the ancestor (from the winner without one).
    pub diffs: Vec<SideDiff>,
}

/// A field that two sides changed in different ways.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Contested {
    pub field: String,
    /// The ancestor's value. Absent when the ancestor does not have the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<Value>,
    /// The value on each side, winner first. Empty when the field is marked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sides: Vec<SideValue>,
    /// `content` only: the draft has conflict markers in it. `ours` is the
    /// winner's lines (or the first side that changed), `theirs` the other side's.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub marked: bool,
    /// With `marked`: the revisions of the `ours` and the `theirs` lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
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
    /// A value for each contested field that is not marked. null removes the field.
    /// A contested field that is not here keeps the winner's value.
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
        let (winner, sides, ancestor) = load(store, id)?;
        Ok(Conflict::from_docs(&winner, &sides, ancestor.as_ref()))
    }

    /// Merge what code can. `sides` excludes the winner.
    pub fn from_docs(winner: &Doc, sides: &[Doc], ancestor: Option<&Doc>) -> Conflict {
        let leaves: Vec<&Doc> = std::iter::once(winner).chain(sides).collect();
        // A change of type changes what the fields mean: compare values only.
        let same_type = leaves.iter().all(|d| d.type_path() == winner.type_path());
        let base = ancestor.filter(|_| same_type);
        let keys: BTreeSet<&String> = leaves.iter().flat_map(|d| d.body.keys()).collect();
        let (mut settled, mut contested, mut draft) = (Map::new(), Vec::new(), Map::new());
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
                    Err(marked) => {
                        draft.insert(key.clone(), Value::String(marked));
                        contested.push(Contested {
                            field: key.clone(),
                            ancestor: base_value.cloned(),
                            sides: Vec::new(),
                            marked: true,
                            ours: Some(ours_rev.to_string()),
                            theirs: Some(theirs_rev.to_string()),
                        });
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
                marked: false,
                ours: None,
                theirs: None,
            });
        }
        Conflict {
            id: winner.id.clone(),
            winner: winner.rev.clone(),
            conflicts: sides.iter().map(|d| d.rev.clone()).collect(),
            ancestor: ancestor.map(|a| a.rev.clone()),
            settled,
            contested,
            draft,
            diffs: side_diffs(winner, sides, ancestor),
        }
    }

    /// The merged body: the draft with the agent's decisions.
    pub fn apply(&self, reply: &Reply) -> Result<Map<String, Value>, StoreError> {
        let mut body = self.draft.clone();
        let mut marked = false;
        for c in &self.contested {
            if c.marked {
                marked = true;
                let text = body.get(&c.field).and_then(Value::as_str).unwrap_or_default();
                let merged = diff::apply_edits(text, &reply.content_edits)?;
                body.insert(c.field.clone(), Value::String(merged));
            } else {
                match reply.fields.get(&c.field) {
                    None => {}
                    Some(Value::Null) => {
                        body.remove(&c.field);
                    }
                    Some(v) => {
                        body.insert(c.field.clone(), v.clone());
                    }
                }
            }
        }
        if !marked && !reply.content_edits.is_empty() {
            return Err(StoreError::invalid("`content_edits` need content with conflict markers, and it has none"));
        }
        Ok(body)
    }
}

/// A text field's value; an absent field is empty text. `None` for other values.
fn as_text(v: Option<&Value>) -> Option<&str> {
    match v {
        None => Some(""),
        Some(v) => v.as_str(),
    }
}

/// The winner, the other live leaves, and the last revision they shared.
fn load(store: &Store, id: &str) -> Result<(Doc, Vec<Doc>, Option<Doc>), StoreError> {
    let winner = store.get(id)?;
    let sides = winner.conflicts.iter().map(|rev| store.get_rev(rev)).collect::<Result<Vec<_>, _>>()?;
    let ancestor = match common_ancestor(store, &winner.rev, &winner.conflicts)? {
        Some(rev) if !sides.is_empty() => Some(store.get_rev(&rev)?),
        _ => None,
    };
    Ok((winner, sides, ancestor))
}

/// Each leaf as a diff from the ancestor, or from the winner without one.
fn side_diffs(winner: &Doc, sides: &[Doc], ancestor: Option<&Doc>) -> Vec<SideDiff> {
    if sides.is_empty() {
        return Vec::new();
    }
    let (base, role) = match ancestor {
        Some(a) => (a, "ancestor"),
        None => (winner, "winner"),
    };
    let base_text = diff::body_text(base.type_path(), &base.body);
    let from = format!("{} ({role})", short_rev(&base.rev));
    std::iter::once((winner, "winner"))
        .chain(sides.iter().map(|d| (d, "conflict")))
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
    let fields: Vec<&Contested> = c.contested.iter().filter(|f| !f.marked).collect();
    if !fields.is_empty() {
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
        for f in fields {
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
    if let Some(f) = c.contested.iter().find(|f| f.marked) {
        let content = c.draft.get(&f.field).and_then(Value::as_str).unwrap_or_default();
        let rev = |r: &Option<String>| r.as_deref().map(short_rev).unwrap_or_default();
        text.push_str(&format!(
            "The field \"{}\" is merged line by line. Where both sides changed the same lines, it has a marked block. \
             Each marker line starts with 7 or more of one character:\n\
             - \"<<<<<<< ours\" starts the lines of revision {}.\n\
             - \"||||||| original\" starts the lines of the ancestor.\n\
             - \"=======\" starts the lines of revision {}.\n\
             - \">>>>>>> theirs\" ends the block.\n\
             <content>\n{content}</content>\n\n",
            f.field,
            rev(&f.ours),
            rev(&f.theirs),
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
    let (winner, sides, ancestor) = load(store, id)?;
    if sides.is_empty() {
        return Ok(None);
    }
    let conflict = Conflict::from_docs(&winner, &sides, ancestor.as_ref());
    let (body, runner) = if conflict.contested.is_empty() {
        (conflict.draft.clone(), None)
    } else {
        let runner = Runner::get(store, &DocRef::from_cli(runner_ref)?.to_string())?;
        let reply = runner::invoke(&runner, db, &format!("resolve/{id}"), &merge_prompt(&conflict)).await?;
        let body = conflict
            .apply(&parse_reply(&reply)?)
            .map_err(|e| StoreError::Runner { message: format!("the merge does not apply: {e}") })?;
        (body, Some(runner.pinned()))
    };
    let merged = PutInput {
        id: Some(id.to_string()),
        parent: Some(winner.rev.clone()),
        // Unpinned, so the merge pins and validates against the current schema.
        type_id: winner.type_path().map(str::to_string),
        body,
    };
    Ok(Some(Proposal { conflicts: conflict.conflicts, winner, merged, runner }))
}

/// Resolve `id` with an agent's decisions on the conflicts it read. Fails
/// when the conflicts changed since, so the draft is the one it read.
pub fn resolve_with(store: &mut Store, id: &str, conflicts: &[String], reply: &Reply) -> Result<Doc, StoreError> {
    let (winner, sides, ancestor) = load(store, id)?;
    let conflict = Conflict::from_docs(&winner, &sides, ancestor.as_ref());
    let (mut want, mut have) = (conflicts.to_vec(), conflict.conflicts.clone());
    want.sort();
    have.sort();
    if want != have {
        return Err(StoreError::Conflict {
            id: id.to_string(),
            parent: Some(winner.rev.clone()),
            leaves: std::iter::once(winner.rev.clone()).chain(have).collect(),
            hint: Some("the conflicts changed since they were read; read them again".into()),
        });
    }
    let merged = PutInput {
        id: Some(id.to_string()),
        parent: Some(winner.rev.clone()),
        type_id: winner.type_path().map(str::to_string),
        body: conflict.apply(reply)?,
    };
    store.resolve(id, Some(merged), Some(conflicts))
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
        let c = Conflict::from_docs(&winner, &[other], Some(&base));
        assert!(c.contested.is_empty());
        assert_eq!(Value::Object(c.settled), json!({"title": "left", "content": "c"}), "a removed field stays removed");
        assert_eq!(c.conflicts, ["2-c"]);
        assert_eq!(c.ancestor.as_deref(), Some("1-a"));
        assert_eq!(c.diffs.len(), 2);

        let same = side("2-d", json!({"title": "left", "content": "c", "tags": ["x"]}));
        let c = Conflict::from_docs(&winner, &[same], Some(&base));
        assert!(c.contested.is_empty(), "the same change on two sides is one change");
    }

    #[test]
    fn content_merges_by_lines() {
        let base = side("1-a", json!({"content": "one\ntwo\nthree\n"}));
        let winner = doc("2-b", json!({"content": "ONE\ntwo\nthree\n"}));
        let apart = side("2-c", json!({"content": "one\ntwo\nTHREE\n"}));
        let c = Conflict::from_docs(&winner, &[apart], Some(&base));
        assert!(c.contested.is_empty());
        assert_eq!(c.settled["content"], "ONE\ntwo\nTHREE\n");

        let winner = doc("2-b", json!({"title": "w", "content": "one\ntwo\nthree\nfour a\n"}));
        let other = side("2-c", json!({"title": "o", "content": "one\ntwo\nthree\nfour b\n"}));
        let c = Conflict::from_docs(&winner, &[other], Some(&base));
        assert_eq!(fields(&c), ["content", "title"]);
        let content = &c.contested[0];
        assert!(content.marked && content.sides.is_empty());
        assert_eq!((content.ours.as_deref(), content.theirs.as_deref()), (Some("2-b"), Some("2-c")));
        let draft = c.draft["content"].as_str().unwrap();
        assert!(draft.starts_with("one\ntwo\nthree\n<<<<<<< ours\nfour a\n"), "{draft}");
        assert_eq!(c.draft["title"], "w", "the draft has the winner's value");
        assert_eq!(c.contested[1].sides.len(), 2);

        let block = &draft["one\ntwo\nthree\n".len()..];
        let reply = Reply {
            fields: serde_json::from_value(json!({"title": "both", "_rev": "x", "tags": ["no"]})).unwrap(),
            content_edits: vec![Edit { old: block.into(), new: "four a\nfour b\n".into() }],
        };
        let body = c.apply(&reply).unwrap();
        assert_eq!(Value::Object(body), json!({"title": "both", "content": "one\ntwo\nthree\nfour a\nfour b\n"}));
        assert!(c.apply(&Reply::default()).unwrap_err().to_string().contains("conflict markers"));
    }

    #[test]
    fn some_conflicts_stay_whole() {
        // three sides that change content: no line merge
        let base = side("1-a", json!({"content": "c\n"}));
        let winner = doc("2-b", json!({"content": "b\n"}));
        let c = Conflict::from_docs(
            &winner,
            &[side("2-c", json!({"content": "x\n"})), side("2-d", json!({"content": "y\n"}))],
            Some(&base),
        );
        assert!(!c.contested[0].marked && c.contested[0].sides.len() == 3);
        let err = c.apply(&Reply {
            content_edits: vec![Edit { old: "b".into(), new: "z".into() }],
            ..Default::default()
        });
        assert!(err.unwrap_err().to_string().contains("`content_edits` need content"));
        let kept = c.apply(&Reply::default()).unwrap();
        assert_eq!(kept["content"], "b\n", "a field without a decision keeps the winner's value");
        let removed =
            c.apply(&Reply { fields: serde_json::from_value(json!({"content": null})).unwrap(), ..Default::default() });
        assert!(!removed.unwrap().contains_key("content"));

        // no ancestor: only values that every side has settle
        let winner = doc("1-b", json!({"title": "same", "content": "b\n"}));
        let c = Conflict::from_docs(&winner, &[side("1-c", json!({"title": "same", "content": "c\n"}))], None);
        assert_eq!(fields(&c), ["content"]);
        assert!(!c.contested[0].marked);
        assert_eq!(c.settled["title"], "same");

        // a changed type: compare values only
        let mut typed = side("2-c", json!({"title": "t", "tags": ["x"]}));
        typed.type_id = Some("doc://schemas/note?rev=1-x".into());
        let base = side("1-a", json!({"title": "t"}));
        let c = Conflict::from_docs(&doc("2-b", json!({"title": "left"})), &[typed], Some(&base));
        assert_eq!(fields(&c), ["tags", "title"]);
    }

    #[test]
    fn prompt_shows_only_what_is_left() {
        let base = side("1-aaa", json!({"title": "base", "tags": ["t"], "content": "one\n"}));
        let winner = doc("2-bbb", json!({"title": "left", "tags": ["t", "w"], "content": "one\nleft\n"}));
        let other = side("2-ccc", json!({"title": "right", "tags": ["t"], "content": "one\nright\n"}));
        let c = Conflict::from_docs(&winner, &[other], Some(&base));
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
