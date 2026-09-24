use dreams::{ListQuery, PutInput, Store, StoreError};
use serde_json::{Value, json};

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn input(v: Value) -> PutInput {
    serde_json::from_value(v).unwrap()
}

const NOTE: &str = "doc://schemas/note";

fn note_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "Note",
        "description": "A note",
        "type": "object",
        "required": ["title"],
        "properties": {
            "title": {"type": "string", "minLength": 1},
            "content": {"type": "string"},
            "tags": {"type": "array", "items": {"type": "string"}}
        }
    })
}

fn ids(page: &dreams::Page) -> Vec<&str> {
    page.docs.iter().map(|d| d.id.as_str()).collect()
}

fn hit_ids(page: &dreams::SearchPage) -> Vec<&str> {
    page.results.iter().map(|r| r.id.as_str()).collect()
}

/// Write the note schema as the document `schemas/note`.
fn put_note_schema(s: &mut Store) -> dreams::Doc {
    let mut body = note_schema();
    body["_id"] = json!("schemas/note");
    s.put(input(body)).unwrap()
}

fn pinned(id: &str, rev: &str) -> String {
    format!("doc://{id}?rev={rev}")
}

#[test]
fn genesis_update_and_conflicts() {
    let mut s = store();
    let d1 = s.put(input(json!({"_id": "a", "title": "one"}))).unwrap();
    assert!(d1.rev.starts_with("1-"));
    assert_eq!(d1.parent, None);
    assert_eq!(d1.body["title"], "one");

    let d2 = s.put(input(json!({"_id": "a", "_parent": d1.rev, "title": "two"}))).unwrap();
    assert!(d2.rev.starts_with("2-"));
    assert_eq!(d2.parent.as_deref(), Some(d1.rev.as_str()));
    assert_eq!(s.get("a").unwrap().rev, d2.rev);

    // stale parent
    let err = s.put(input(json!({"_id": "a", "_parent": d1.rev, "title": "three"}))).unwrap_err();
    match err {
        StoreError::Conflict { id, parent, leaves, .. } => {
            assert_eq!(id, "a");
            assert_eq!(parent.as_deref(), Some(d1.rev.as_str()));
            assert_eq!(leaves, vec![d2.rev.clone()]);
        }
        other => panic!("{other:?}"),
    }
    // genesis on an existing doc
    assert!(matches!(s.put(input(json!({"_id": "a", "title": "four"}))), Err(StoreError::Conflict { .. })));
    // parent that never existed
    assert!(matches!(
        s.put(input(json!({"_id": "a", "_parent": "2-deadbeef", "title": "x"}))),
        Err(StoreError::Conflict { .. })
    ));
    // parent belonging to another doc
    s.put(input(json!({"_id": "b", "title": "b"}))).unwrap();
    assert!(matches!(
        s.put(input(json!({"_id": "b", "_parent": d2.rev, "title": "x"}))),
        Err(StoreError::Conflict { .. })
    ));
}

#[test]
fn idempotent_replay() {
    let mut s = store();
    let d1 = s.put(input(json!({"_id": "a", "title": "one", "n": 1}))).unwrap();
    let again = s.put(input(json!({"_id": "a", "n": 1, "title": "one"}))).unwrap();
    assert_eq!(again.rev, d1.rev);
    assert_eq!(s.changes(0, None).unwrap().results.len(), 1);

    let d2 = s.put(input(json!({"_id": "a", "_parent": d1.rev, "title": "two"}))).unwrap();
    // replaying the old genesis after an update is a conflict, not a silent stale return
    assert!(matches!(s.put(input(json!({"_id": "a", "title": "one", "n": 1}))), Err(StoreError::Conflict { .. })));
    assert_eq!(s.put(input(json!({"_id": "a", "_parent": d1.rev, "title": "two"}))).unwrap().rev, d2.rev);
}

#[test]
fn generated_ids_are_uuid_v7_markdown() {
    let mut s = store();
    let d = s.put(input(json!({"title": "x"}))).unwrap();
    let u = uuid::Uuid::parse_str(d.id.strip_suffix(".md").unwrap()).unwrap();
    assert_eq!(u.get_version_num(), 7);
}

#[test]
fn delete_tombstone_and_undelete() {
    let mut s = store();
    let schema = put_note_schema(&mut s);
    let d1 = s.put(input(json!({"_id": "a", "_type": NOTE, "title": "t", "tags": ["x"]}))).unwrap();
    assert_eq!(d1.type_id.as_deref(), Some(pinned("schemas/note", &schema.rev).as_str()));
    // wrong parent
    assert!(matches!(s.delete("a", "1-nope"), Err(StoreError::NotFound { .. })));
    // required title would fail validation, but tombstones skip it
    let tomb = s.delete("a", &d1.rev).unwrap();
    assert!(tomb.deleted);
    assert_eq!(tomb.type_id, d1.type_id);
    assert!(tomb.body.is_empty());

    match s.get("a") {
        Err(StoreError::Deleted { id, rev }) => {
            assert_eq!(id, "a");
            assert_eq!(rev, tomb.rev);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(s.delete("a", &tomb.rev), Err(StoreError::Deleted { .. })));
    assert_eq!(ids(&s.list(&ListQuery::default()).unwrap()), vec!["schemas/note"]);
    assert!(s.list(&ListQuery { tag: Some("x".into()), ..Default::default() }).unwrap().docs.is_empty());
    assert!(s.search("t", &ListQuery::default()).unwrap().results.is_empty());

    let back =
        s.put(input(json!({"_id": "a", "_parent": tomb.rev, "_type": NOTE, "title": "back", "tags": ["x"]}))).unwrap();
    assert!(back.rev.starts_with("3-"));
    assert_eq!(s.get("a").unwrap().body["title"], "back");
    assert_eq!(ids(&s.list(&ListQuery { tag: Some("x".into()), ..Default::default() }).unwrap()), vec!["a"]);
    assert_eq!(s.history("a", None).unwrap().revisions.len(), 3);
}

#[test]
fn get_href_reads_current_or_pinned() {
    let mut s = store();
    let a1 = s.put(input(json!({"_id": "a", "title": "one"}))).unwrap();
    let a2 = s.put(input(json!({"_id": "a", "_parent": a1.rev, "title": "two"}))).unwrap();
    let b = s.put(input(json!({"_id": "b"}))).unwrap();

    // bare id and doc:// give the current revision
    assert_eq!(s.get_href("a", false).unwrap().rev, a2.rev);
    assert_eq!(s.get_href("doc://a", false).unwrap().rev, a2.rev);
    // a pinned href gives that revision
    assert_eq!(s.get_href(&pinned("a", &a1.rev), false).unwrap().body["title"], "one");
    // only this document's revisions
    assert!(matches!(s.get_href(&pinned("a", &b.rev), false), Err(StoreError::InvalidInput { .. })));
    // deleted_conflicts needs an unpinned href
    assert!(matches!(s.get_href(&pinned("a", &a1.rev), true), Err(StoreError::InvalidInput { .. })));
    assert!(s.get_href("a", true).unwrap().deleted_conflicts.is_empty());
    assert!(matches!(s.get_href("doc://", false), Err(StoreError::InvalidInput { .. })));

    // a pinned tombstone is returned, not reported as Deleted
    let tomb = s.delete("a", &a2.rev).unwrap();
    assert!(matches!(s.get_href("a", false), Err(StoreError::Deleted { .. })));
    assert!(s.get_href(&pinned("a", &tomb.rev), false).unwrap().deleted);
}

#[test]
fn tags_follow_the_current_revision() {
    let mut s = store();
    let d1 = s.put(input(json!({"_id": "a", "title": "a", "tags": ["x", "y"]}))).unwrap();
    s.put(input(json!({"_id": "b", "title": "b", "tags": ["x"]}))).unwrap();
    let by = |s: &Store, tag: &str| {
        ids(&s.list(&ListQuery { tag: Some(tag.into()), ..Default::default() }).unwrap())
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    assert_eq!(by(&s, "x"), vec!["b", "a"]); // most recently modified first
    assert_eq!(by(&s, "y"), vec!["a"]);

    s.put(input(json!({"_id": "a", "_parent": d1.rev, "title": "a", "tags": ["y"]}))).unwrap();
    assert_eq!(by(&s, "x"), vec!["b"]);
    assert_eq!(by(&s, "y"), vec!["a"]);
    assert!(by(&s, "zzz").is_empty());

    // historical tags are still in the old revision's body
    let old = s.get_rev(&d1.rev).unwrap();
    assert_eq!(old.body["tags"], json!(["x", "y"]));
}

#[test]
fn input_validation() {
    let mut s = store();
    assert!(matches!(s.put(input(json!({"_foo": 1}))), Err(StoreError::InvalidInput { .. })));
    assert!(matches!(s.put(input(json!({"_rev": "1-a"}))), Err(StoreError::InvalidInput { .. })));
    assert!(matches!(s.put(input(json!({"title": 7}))), Err(StoreError::InvalidInput { .. })));
    assert!(matches!(s.put(input(json!({"tags": "x"}))), Err(StoreError::InvalidInput { .. })));
    assert!(matches!(s.put(input(json!({"_id": "_x"}))), Err(StoreError::InvalidInput { .. })));
    let long_id = "x".repeat(513);
    assert!(matches!(s.put(input(json!({"_id": long_id}))), Err(StoreError::InvalidInput { .. })));
    let big = "y".repeat(1 << 20);
    assert!(matches!(s.put(input(json!({"content": big}))), Err(StoreError::InvalidInput { .. })));
    // untyped docs with arbitrary fields are fine
    assert!(s.put(input(json!({"anything": {"nested": [1, 2]}}))).is_ok());
}

#[test]
fn schema_validation_on_write() {
    let mut s = store();
    assert!(matches!(s.put(input(json!({"_type": NOTE, "title": "x"}))), Err(StoreError::UnknownType { .. })));
    assert!(matches!(s.put(input(json!({"_type": "note/v1", "title": "x"}))), Err(StoreError::InvalidInput { .. })));
    let schema = put_note_schema(&mut s);
    let pin = pinned("schemas/note", &schema.rev);

    let ok = s.put(input(json!({"_type": NOTE, "title": "x"}))).unwrap();
    assert_eq!(ok.type_id.as_deref(), Some(pin.as_str()));
    assert_eq!(ok.type_path(), Some(NOTE));
    // a pinned reference works too, and must belong to the named document
    assert!(s.put(input(json!({"_type": pin, "title": "y"}))).is_ok());
    let wrong = format!("doc://schemas/other?rev={}", schema.rev);
    assert!(matches!(s.put(input(json!({"_type": wrong, "title": "y"}))), Err(StoreError::InvalidInput { .. })));
    assert!(matches!(
        s.put(input(json!({"_type": "doc://schemas/note?rev=9-ffff", "title": "y"}))),
        Err(StoreError::UnknownType { .. })
    ));

    match s.put(input(json!({"_type": NOTE, "content": "no title"}))) {
        Err(StoreError::Validation { schema, errors }) => {
            assert_eq!(schema, pin);
            assert!(errors.iter().any(|e| e.message.contains("title")), "{errors:?}");
        }
        other => panic!("{other:?}"),
    }
    match s.put(input(json!({"_type": NOTE, "title": ""}))) {
        Err(StoreError::Validation { errors, .. }) => {
            assert!(errors.iter().any(|e| e.path == "/title"), "{errors:?}");
            // blessed-field type errors are caught before the schema runs
            assert!(matches!(
                s.put(input(json!({"_type": NOTE, "title": "x", "tags": [1]}))),
                Err(StoreError::InvalidInput { .. })
            ));
        }
        other => panic!("{other:?}"),
    }
    // a schema that is not a schema fails at the document write, naming the schema
    s.put(input(json!({"_id": "schemas/bad", "type": 12}))).unwrap();
    let err = s.put(input(json!({"_type": "doc://schemas/bad", "title": "x"}))).unwrap_err();
    assert!(err.to_string().contains("doc://schemas/bad?rev="), "{err}");
    // a deleted schema cannot be referenced unpinned, but its old pin still validates
    let bad = s.get("schemas/bad").unwrap();
    s.delete("schemas/bad", &bad.rev).unwrap();
    assert!(matches!(
        s.put(input(json!({"_type": "doc://schemas/bad", "title": "x"}))),
        Err(StoreError::Deleted { .. })
    ));
}

#[test]
fn pinning_makes_replay_a_no_op() {
    let mut s = store();
    let schema = put_note_schema(&mut s);
    let first = s.put(input(json!({"_id": "n", "_type": NOTE, "title": "x"}))).unwrap();
    let again = s.put(input(json!({"_id": "n", "_type": NOTE, "title": "x"}))).unwrap();
    assert_eq!(again.rev, first.rev);
    assert_eq!(s.history("n", None).unwrap().revisions.len(), 1);

    // the schema moves: the same body with an unpinned type is a new revision, pinned to the new schema
    let schema2 = put_schema_revision(&mut s, &schema.rev, |v| v["description"] = json!("second"));
    assert!(schema2.rev.starts_with("2-"));
    // a create over the same body now conflicts, and the error says why
    match s.put(input(json!({"_id": "n", "_type": NOTE, "title": "x"}))) {
        Err(StoreError::Conflict { hint: Some(hint), .. }) => assert!(hint.contains("schema moved"), "{hint}"),
        other => panic!("{other:?}"),
    }
    let moved = s.put(input(json!({"_id": "n", "_parent": first.rev, "_type": NOTE, "title": "x"}))).unwrap();
    assert!(moved.rev.starts_with("2-"));
    assert_eq!(moved.type_id.as_deref(), Some(pinned("schemas/note", &schema2.rev).as_str()));

    // a stricter third revision: the old pin still validates against the old body
    put_schema_revision(&mut s, &schema2.rev, |v| v["properties"]["title"]["minLength"] = json!(2));
    let old_pin = pinned("schemas/note", &schema.rev);
    let short = s.put(input(json!({"_id": "m", "_type": old_pin, "title": "x"}))).unwrap();
    assert_eq!(short.type_id.as_deref(), Some(old_pin.as_str()));
    assert!(matches!(
        s.put(input(json!({"_id": "m2", "_type": NOTE, "title": "x"}))),
        Err(StoreError::Validation { .. })
    ));
}

/// Write the next revision of `schemas/note` with one edit applied to the note schema body.
fn put_schema_revision(s: &mut Store, parent: &str, edit: impl FnOnce(&mut Value)) -> dreams::Doc {
    let mut body = note_schema();
    edit(&mut body);
    body["_id"] = json!("schemas/note");
    body["_parent"] = json!(parent);
    s.put(input(body)).unwrap()
}

#[test]
fn type_filters_match_by_path_or_pin() {
    let mut s = store();
    let schema = put_note_schema(&mut s);
    let a = s.put(input(json!({"_id": "a", "_type": NOTE, "title": "a", "content": "findable"}))).unwrap();
    put_schema_revision(&mut s, &schema.rev, |v| v["description"] = json!("changed"));
    let b = s.put(input(json!({"_id": "b", "_type": NOTE, "title": "b", "content": "findable"}))).unwrap();
    assert_ne!(a.type_id, b.type_id);

    let by_path = s.list(&ListQuery { type_id: Some(NOTE.into()), ..Default::default() }).unwrap();
    assert_eq!(ids(&by_path), vec!["b", "a"]);
    let by_pin = s.list(&ListQuery { type_id: a.type_id.clone(), ..Default::default() }).unwrap();
    assert_eq!(ids(&by_pin), vec!["a"]);
    let searched = s.search("findable", &ListQuery { type_id: b.type_id.clone(), ..Default::default() }).unwrap();
    assert_eq!(hit_ids(&searched), vec!["b"]);
    let searched = s.search("findable", &ListQuery { type_id: Some(NOTE.into()), ..Default::default() }).unwrap();
    assert_eq!(searched.results.len(), 2);
}

#[test]
fn changes_feed() {
    let mut s = store();
    let a = s.put(input(json!({"_id": "a", "title": "a"}))).unwrap();
    s.put(input(json!({"_id": "b", "title": "b"}))).unwrap();
    s.put(input(json!({"_id": "a", "_parent": a.rev, "title": "a2"}))).unwrap();

    let c = s.changes(0, None).unwrap();
    let seqs: Vec<i64> = c.results.iter().map(|d| d.seq.unwrap()).collect();
    assert_eq!(seqs, vec![1, 2, 3]);
    assert_eq!(c.last_seq, 3);
    assert_eq!(c.results[2].body["title"], "a2");

    let c2 = s.changes(1, Some(1)).unwrap();
    assert_eq!(c2.results.len(), 1);
    assert_eq!(c2.last_seq, 2);
    let c3 = s.changes(3, None).unwrap();
    assert!(c3.results.is_empty());
    assert_eq!(c3.last_seq, 3);
}

#[test]
fn search_sees_only_current_revisions() {
    let mut s = store();
    let d1 = s.put(input(json!({"_id": "a", "title": "Alpha", "content": "first draft", "tags": ["blue"]}))).unwrap();
    s.put(input(json!({"_id": "b", "title": "Beta", "content": "unrelated"}))).unwrap();
    assert_eq!(hit_ids(&s.search("first", &ListQuery::default()).unwrap()), vec!["a"]);
    assert_eq!(hit_ids(&s.search("blue", &ListQuery::default()).unwrap()), vec!["a"]);

    s.put(input(json!({"_id": "a", "_parent": d1.rev, "title": "Alpha", "content": "second draft"}))).unwrap();
    assert!(s.search("first", &ListQuery::default()).unwrap().results.is_empty());
    assert_eq!(hit_ids(&s.search("second", &ListQuery::default()).unwrap()), vec!["a"]);
    assert!(s.search("blue", &ListQuery::default()).unwrap().results.is_empty());

    // stemming and operators in user input do not error
    assert_eq!(hit_ids(&s.search("drafts", &ListQuery::default()).unwrap()), vec!["a"]);
    assert!(s.search("hello AND", &ListQuery::default()).is_ok());
    assert!(s.search("\"unbalanced (", &ListQuery::default()).is_ok());
    // filters
    assert!(s.search("second", &ListQuery { tag: Some("blue".into()), ..Default::default() }).unwrap().results.is_empty());
    // empty query lists, with no matches
    let listed = s.search("   ", &ListQuery::default()).unwrap();
    assert_eq!(listed.results.len(), 2);
    assert!(listed.results.iter().all(|r| r.content_matches.is_none() && r.title.is_some()));
}

#[test]
fn search_results_carry_metadata_and_matches() {
    let mut s = store();
    let a = s.put(input(json!({"_id": "a", "title": "Alpha", "content": "the first draft", "tags": ["blue"]}))).unwrap();

    let page = s.search("first", &ListQuery::default()).unwrap();
    let hit = &page.results[0];
    assert_eq!(hit.rev, a.rev);
    assert_eq!(hit.created_at, a.created_at);
    assert_eq!(hit.title.as_deref(), Some("Alpha"));
    assert_eq!(hit.content_matches.as_deref(), Some("the **first** draft"));

    let page = s.search("alpha", &ListQuery::default()).unwrap();
    assert_eq!(page.results[0].content_matches.as_deref(), Some("**Alpha**"));

    let json = serde_json::to_value(&page.results[0]).unwrap();
    for key in ["content", "tags", "_seq", "_conflicts", "_parent"] {
        assert!(json.get(key).is_none(), "{key} in {json}");
    }
}

#[test]
fn list_pagination_walks_every_head_once() {
    let mut s = store();
    for i in 0..7 {
        s.put(input(json!({"_id": format!("d{i}"), "title": format!("{i}")}))).unwrap();
    }
    let mut seen = Vec::new();
    let mut q = ListQuery { limit: Some(3), ..Default::default() };
    loop {
        let page = s.list(&q).unwrap();
        seen.extend(page.docs.iter().map(|d| d.id.clone()));
        match page.next {
            Some(next) => q.before = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, vec!["d6", "d5", "d4", "d3", "d2", "d1", "d0"]);
    // list results do not expose _seq
    assert!(s.list(&ListQuery::default()).unwrap().docs.iter().all(|d| d.seq.is_none()));
}

#[test]
fn docs_rows_are_immutable() {
    let mut s = store();
    s.put(input(json!({"_id": "a", "title": "a"}))).unwrap();
    let conn = s.connection();
    assert!(conn.execute("UPDATE docs SET _type = 'x' WHERE _id = 'a'", []).is_err());
    assert!(conn.execute("DELETE FROM docs WHERE _id = 'a'", []).is_err());
    // rev-chain trigger rejects a forged genesis at gen 2
    assert!(
        conn.execute("INSERT INTO docs(_rev,_id,_parent,_type,_deleted,body) VALUES ('2-ff','z',NULL,NULL,0,'{}')", [])
            .is_err()
    );
}

#[test]
fn reopening_a_file_keeps_data_and_does_not_remigrate() {
    let dir = std::env::temp_dir().join(format!("dreams-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("vault.db");
    {
        let mut s = Store::open(&path).unwrap();
        s.put(input(json!({"_id": "a", "title": "persisted"}))).unwrap();
    }
    {
        let s = Store::open(&path).unwrap();
        assert_eq!(s.get("a").unwrap().body["title"], "persisted");
        let n: i64 = s.connection().query_row("SELECT count(*) FROM migrations", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 5);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn opening_creates_missing_folders() {
    let dir = std::env::temp_dir().join(format!("dreams-test-{}", uuid::Uuid::now_v7()));
    let path = dir.join("nested").join("vault.db");
    Store::open(&path).unwrap();
    assert!(path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- scheduled tasks ------------------------------------------------------

use dreams::runner::{PROTECTED_IDS, PROTECTED_TYPES, RUNNER_TYPE};
use dreams::seed;
use dreams::task::{self, RUN_TYPE, TASK_TYPE};

fn plus_secs(s: &Store, from: &str, secs: i64) -> String {
    s.connection()
        .query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', unixepoch(?1) + ?2, 'unixepoch')",
            rusqlite::params![from, secs],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn actor_is_recorded_per_store() {
    let mut s = store();
    let plain = s.put(input(json!({"_id": "a", "title": "a"}))).unwrap();
    assert_eq!(plain.actor, None);
    s.set_actor(Some("tasks/t".into()));
    let by_task = s.put(input(json!({"_id": "b", "title": "b"}))).unwrap();
    assert_eq!(by_task.actor.as_deref(), Some("tasks/t"));
    assert_eq!(serde_json::to_value(&by_task).unwrap()["_actor"], "tasks/t");
    assert!(serde_json::to_value(&plain).unwrap().get("_actor").is_none());
    s.set_actor(None);
    let again = s.put(input(json!({"_id": "c", "title": "c"}))).unwrap();
    assert_eq!(again.actor, None);
    assert_eq!(s.get("b").unwrap().actor.as_deref(), Some("tasks/t"));
}

#[test]
fn put_if_writes_only_when_the_check_passes() {
    let mut s = store();
    let written = s.put_if(input(json!({"_id": "a", "title": "a"})), |_| Ok(true)).unwrap();
    assert!(written.is_some());
    let refused = s.put_if(input(json!({"_id": "b", "title": "b"})), |_| Ok(false)).unwrap();
    assert!(refused.is_none());
    assert!(matches!(s.get("b"), Err(StoreError::NotFound { .. })));
    // the check sees the same transaction the write would use
    let seen = s
        .put_if(input(json!({"_id": "c", "title": "c"})), |conn| {
            Ok(conn.query_row("SELECT count(*) FROM docs", [], |r| r.get::<_, i64>(0))? == 1)
        })
        .unwrap();
    assert!(seen.is_some());
}

#[test]
fn protected_types_are_read_only() {
    let mut s = store();
    seed::seed_schemas(&mut s).unwrap();
    let doc = s.put(input(json!({"_id": "runners/x", "_type": RUNNER_TYPE, "argv": ["cat"]}))).unwrap();
    s.set_protected(PROTECTED_TYPES, PROTECTED_IDS);
    assert!(matches!(
        s.put(input(json!({"_id": "runners/y", "_type": RUNNER_TYPE, "argv": ["cat"]}))),
        Err(StoreError::Protected { .. })
    ));
    // pinning to a specific schema revision does not get around the path check
    assert!(matches!(
        s.put(input(json!({"_id": "runners/y", "_type": doc.type_id, "argv": ["cat"]}))),
        Err(StoreError::Protected { .. })
    ));
    // the seeded schema documents are protected by id
    let task_schema = s.get("schemas/task").unwrap();
    assert!(matches!(
        s.put(input(json!({"_id": "schemas/task", "_parent": task_schema.rev, "type": "object"}))),
        Err(StoreError::Protected { id: Some(_), .. })
    ));
    assert!(matches!(s.delete("schemas/task", &task_schema.rev), Err(StoreError::Protected { .. })));
    // an update that drops the type is still an update of a protected document
    assert!(matches!(
        s.put(input(json!({"_id": "runners/x", "_parent": doc.rev, "title": "plain"}))),
        Err(StoreError::Protected { .. })
    ));
    assert!(matches!(s.delete("runners/x", &doc.rev), Err(StoreError::Protected { .. })));
    assert!(matches!(
        s.put(input(json!({"_id": "runs/t/1", "_type": RUN_TYPE, "task": "t", "runner": "r", "started_at": "x", "seq": 1, "tags": ["t"]}))),
        Err(StoreError::Protected { .. })
    ));
    // reads and ordinary writes still work
    assert_eq!(s.get("runners/x").unwrap().body["argv"], json!(["cat"]));
    assert!(s.put(input(json!({"_id": "note", "title": "n"}))).is_ok());
    s.set_protected(&[], &[]);
    assert!(s.delete("runners/x", &doc.rev).is_ok());
}

#[test]
fn seed_is_idempotent() {
    let mut s = store();
    let first = seed::seed(&mut s).unwrap();
    let ids: Vec<String> = seed::defaults().into_iter().map(|d| d.id.unwrap()).collect();
    assert_eq!(first, ids);
    assert!(seed::seed(&mut s).unwrap().is_empty());
    let pi = s.get("runners/pi").unwrap();
    assert_eq!(pi.type_path(), Some(RUNNER_TYPE));
    let runner_schema = s.get("schemas/runner").unwrap();
    assert_eq!(pi.type_id.as_deref(), Some(pinned("schemas/runner", &runner_schema.rev).as_str()));
    assert_eq!(runner_schema.type_id, None);

    // a vault from before doc:// types is refused
    let mut old = store();
    old.connection()
        .execute("INSERT INTO docs(_rev,_id,_parent,_type,_deleted,body) VALUES ('1-aa','x',NULL,'note/v1',0,'{}')", [])
        .unwrap();
    assert!(seed::seed(&mut old).unwrap_err().to_string().contains("predates"));
}

#[test]
fn seed_rewrites_edited_and_deleted_defaults() {
    let mut s = store();
    seed::seed(&mut s).unwrap();
    let claude = s.get("runners/claude").unwrap();
    s.put(input(json!({"_id": "runners/claude", "_parent": claude.rev, "_type": RUNNER_TYPE, "argv": ["cat"]})))
        .unwrap();
    let skill = s.get("skills/daily-note").unwrap();
    s.delete("skills/daily-note", &skill.rev).unwrap();

    assert_eq!(seed::seed(&mut s).unwrap(), ["runners/claude", "skills/daily-note"]);
    assert_eq!(s.get("runners/claude").unwrap().body, claude.body);
    assert_eq!(s.get("skills/daily-note").unwrap().body, skill.body);
    assert!(seed::seed(&mut s).unwrap().is_empty());
    // the edit stays in history
    assert_eq!(s.history("runners/claude", None).unwrap().revisions.len(), 3);
}

/// Deploy one task without a confirmation; the front ends ask.
fn deploy(s: &mut Store, id: &str) -> task::TaskState {
    let plan = task::plan_deploy(s, Some(id)).unwrap();
    task::apply_deploy(s, &plan).unwrap().remove(0)
}

fn add_task(s: &mut Store, id: &str, every: &str, when: Option<Value>) -> dreams::Doc {
    let mut body =
        json!({"_id": id, "_type": TASK_TYPE, "runner": "doc://runners/cat", "every": every, "prompt": "go"});
    if let Some(w) = when {
        body["when"] = w;
    }
    s.put(input(body)).unwrap()
}

#[test]
fn evaluate_time_and_change_rules() {
    let mut s = store();
    seed::seed_schemas(&mut s).unwrap();
    let base = s.last_seq().unwrap();
    let cat =
        s.put(input(json!({"_id": "runners/cat", "_type": RUNNER_TYPE, "argv": ["cat"], "timeout": "1m"}))).unwrap();
    let plain = add_task(&mut s, "tasks/plain", "1h", None);
    let watch = add_task(&mut s, "tasks/watch", "15m", Some(json!({"tag": "inbox"})));
    let created = watch.created_at.clone();

    // a task with no state is dormant: listed, never due
    let evals = task::evaluate(&s, &plus_secs(&s, &created, 2 * 3600), None).unwrap();
    assert_eq!(evals.len(), 2);
    assert!(evals.iter().all(|e| e.state.is_none() && !e.due && !e.time_due));

    // a deploy starts the schedule now and the cursor at the head, pinned to the heads
    let state = deploy(&mut s, "tasks/plain");
    assert!(state.enabled && state.last_run_at.is_none() && state.lease_until.is_none());
    assert_eq!(state.task_rev, plain.rev);
    assert_eq!(state.runner, pinned("runners/cat", &cat.rev));
    deploy(&mut s, "tasks/watch");
    let evals = task::evaluate(&s, &plus_secs(&s, &created, 60), None).unwrap();
    let e_plain = evals.iter().find(|e| e.task.id == "tasks/plain").unwrap();
    let e_watch = evals.iter().find(|e| e.task.id == "tasks/watch").unwrap();
    assert!(!e_plain.time_due && !e_plain.due);
    assert_eq!(e_watch.cursor, base + 3);
    assert_eq!(e_watch.head, base + 3);
    assert!(e_watch.changes.is_empty());
    assert_eq!(e_watch.runner.as_ref().unwrap().rev, cat.rev);

    // after the interval: the plain task is due; the watcher waits for changes
    let later = plus_secs(&s, &created, 2 * 3600);
    let evals = task::evaluate(&s, &later, None).unwrap();
    let e_plain = evals.iter().find(|e| e.task.id == "tasks/plain").unwrap().clone();
    let e_watch = evals.iter().find(|e| e.task.id == "tasks/watch").unwrap();
    assert!(e_plain.due);
    assert!(e_watch.time_due && !e_watch.due);

    // a tagged write, an untagged write, and the watcher's own write
    let note = s.put(input(json!({"_id": "n1", "title": "n", "tags": ["inbox"]}))).unwrap();
    s.put(input(json!({"_id": "n2", "title": "other"}))).unwrap();
    s.set_actor(Some("tasks/watch".into()));
    s.put(input(json!({"_id": "n3", "title": "self", "tags": ["inbox"]}))).unwrap();
    s.set_actor(None);
    let e_watch = task::evaluate(&s, &later, Some("tasks/watch")).unwrap().remove(0);
    let ids: Vec<&str> = e_watch.changes.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["n1"]);
    assert!(e_watch.due);
    assert_eq!(e_watch.head, base + 6);

    // a tombstone of a tagged document counts as a change to that tag
    s.delete("n1", &note.rev).unwrap();
    let e_watch = task::evaluate(&s, &later, Some("tasks/watch")).unwrap().remove(0);
    let seen: Vec<(String, bool)> = e_watch.changes.iter().map(|c| (c.id.clone(), c.deleted)).collect();
    assert_eq!(seen, [("n1".to_string(), false), ("n1".to_string(), true)]);

    // a claim takes the lease until the timeout plus a minute; no document is written
    let docs_before = s.last_seq().unwrap();
    let claim = task::claim(&mut s, &e_watch, &later, false).unwrap().unwrap();
    assert_eq!(s.last_seq().unwrap(), docs_before);
    assert!(claim.run_id.starts_with("runs/tasks/watch/") && claim.run_id.ends_with(".md"));
    assert_eq!(claim.runner, pinned("runners/cat", &cat.rev));
    assert_eq!(claim.head, base + 7);
    let e_running = task::evaluate(&s, &plus_secs(&s, &later, 30), Some("tasks/watch")).unwrap().remove(0);
    assert!(e_running.running && !e_running.due);
    assert!(task::claim(&mut s, &e_watch, &later, false).unwrap().is_none(), "the lease is held");
    let e_stale = task::evaluate(&s, &plus_secs(&s, &later, 121), Some("tasks/watch")).unwrap().remove(0);
    assert!(!e_stale.running, "the lease expired");
    assert_eq!(e_stale.cursor, base + 3, "the cursor moves only when a run finishes");

    // finishing writes one receipt, moves the cursor, and releases the lease
    let outcome = task::Outcome { exit_code: Some(0), stdout: b"done".to_vec(), ..Default::default() };
    let finished_at = plus_secs(&s, &later, 5);
    let receipt = task::finish(
        &mut s,
        &claim,
        &outcome,
        std::path::Path::new("/nonexistent"),
        std::time::Duration::from_secs(60),
        &finished_at,
    )
    .unwrap();
    assert_eq!(receipt.id, claim.run_id);
    assert_eq!(receipt.parent, None);
    assert_eq!(receipt.type_path(), Some(RUN_TYPE));
    assert_eq!(receipt.body["vault"], s.vault_id().unwrap());
    assert_eq!(receipt.body["tags"], json!(["tasks/watch"]));
    assert_eq!(receipt.body["content"], "done");
    assert!(receipt.body.get("seq").is_none());
    let e_watch = task::evaluate(&s, &plus_secs(&s, &later, 30), Some("tasks/watch")).unwrap().remove(0);
    assert!(!e_watch.running);
    assert_eq!(e_watch.cursor, base + 7);
    assert_eq!(e_watch.last_run_at.as_deref(), Some(later.as_str()));
    // only the receipt changed since the cursor, and it is the watcher's own write
    assert!(e_watch.changes.is_empty() && !e_watch.due);

    // a claim from an evaluation made before a finished run fails
    assert!(task::claim(&mut s, &e_plain, &later, false).unwrap().is_some());
    let plain_claim = task::claim(&mut s, &e_plain, &later, true).unwrap().unwrap();
    task::finish(
        &mut s,
        &plain_claim,
        &outcome,
        std::path::Path::new("/nonexistent"),
        std::time::Duration::from_secs(60),
        &later,
    )
    .unwrap();
    assert!(task::claim(&mut s, &e_plain, &later, false).unwrap().is_none(), "stale evaluation");

    // disabled tasks are listed but never due
    task::disable(&mut s, "tasks/plain").unwrap();
    let e = task::evaluate(&s, &plus_secs(&s, &later, 7200), Some("tasks/plain")).unwrap().remove(0);
    assert!(!e.due && !e.state.as_ref().unwrap().enabled);
    assert_eq!(e.cursor, plain_claim.head, "disabling keeps the cursor");
    assert!(matches!(task::evaluate(&s, &later, Some("nope")), Err(StoreError::NotFound { .. })));
    assert!(task::evaluate(&s, &later, Some("n2")).is_err());
    assert!(task::plan_deploy(&s, Some("n2")).is_err());
    assert!(task::disable(&mut s, "n2").is_err());
}

#[test]
fn a_deploy_pins_the_task_and_runner_revisions() {
    let mut s = store();
    seed::seed_schemas(&mut s).unwrap();
    let cat = s.put(input(json!({"_id": "runners/cat", "_type": RUNNER_TYPE, "argv": ["cat"]}))).unwrap();
    let v1 = add_task(&mut s, "tasks/t", "1h", None);
    deploy(&mut s, "tasks/t");
    let now = s.now().unwrap();

    // edits to the template and to the runner do not change what runs
    let v2 = s
        .put(input(json!({"_id": "tasks/t", "_parent": v1.rev, "_type": TASK_TYPE,
            "runner": "doc://runners/cat", "every": "1h", "prompt": "edited"})))
        .unwrap();
    let e = task::evaluate(&s, &now, Some("tasks/t")).unwrap().remove(0);
    assert_eq!((e.task.rev.as_str(), e.head_rev.as_str(), e.drift), (v1.rev.as_str(), v2.rev.as_str(), true));
    assert_eq!(e.parsed.prompt, "go");
    let cat2 =
        s.put(input(json!({"_id": "runners/cat", "_parent": cat.rev, "_type": RUNNER_TYPE, "argv": ["tac"]}))).unwrap();
    let e = task::evaluate(&s, &now, Some("tasks/t")).unwrap().remove(0);
    assert_eq!(e.runner.as_ref().unwrap().argv, ["cat"]);

    // a plan made before an edit cannot be applied after it
    let plan = task::plan_deploy(&s, None).unwrap();
    assert_eq!(plan.len(), 1);
    assert!(plan[0].describe().contains("edited") && plan[0].describe().contains("command: tac"));
    let cursor = task::state(&s, "tasks/t").unwrap().unwrap().cursor;
    let v3 = s
        .put(input(json!({"_id": "tasks/t", "_parent": v2.rev, "_type": TASK_TYPE,
            "runner": "doc://runners/cat", "every": "1h", "prompt": "again"})))
        .unwrap();
    assert!(!task::plan_is_current(&s, &plan).unwrap());
    assert!(task::apply_deploy(&mut s, &plan).is_err());

    // a redeploy runs the new revisions and keeps the cursor
    let state = deploy(&mut s, "tasks/t");
    assert_eq!((state.task_rev, state.runner, state.cursor), (v3.rev, pinned("runners/cat", &cat2.rev), cursor));
    let e = task::evaluate(&s, &now, Some("tasks/t")).unwrap().remove(0);
    assert!(!e.drift);
    assert_eq!(e.parsed.prompt, "again");
    assert!(task::plan_deploy(&s, None).unwrap().is_empty(), "nothing left to deploy");

    // deploy-all covers dormant and disabled tasks too
    add_task(&mut s, "tasks/new", "1h", None);
    task::disable(&mut s, "tasks/t").unwrap();
    let ids: Vec<String> = task::plan_deploy(&s, None).unwrap().into_iter().map(|d| d.task_id).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"tasks/new".to_string()) && ids.contains(&"tasks/t".to_string()));
}

#[test]
fn a_tombstone_ends_the_task_and_a_revived_task_is_dormant() {
    let mut s = store();
    seed::seed_schemas(&mut s).unwrap();
    s.put(input(json!({"_id": "runners/cat", "_type": RUNNER_TYPE, "argv": ["cat"]}))).unwrap();
    let t = add_task(&mut s, "tasks/t", "1h", None);
    deploy(&mut s, "tasks/t");
    let tomb = s.delete("tasks/t", &t.rev).unwrap();
    assert!(task::state(&s, "tasks/t").unwrap().is_none());
    s.put(input(json!({"_id": "tasks/t", "_parent": tomb.rev, "_type": TASK_TYPE,
        "runner": "doc://runners/cat", "every": "1h", "prompt": "back"})))
        .unwrap();
    let now = s.now().unwrap();
    let e = task::evaluate(&s, &now, Some("tasks/t")).unwrap().remove(0);
    assert!(e.state.is_none() && !e.due);

    // a type change ends it too
    let t = s.get("tasks/t").unwrap();
    deploy(&mut s, "tasks/t");
    s.put(input(json!({"_id": "tasks/t", "_parent": t.rev, "title": "just a note"}))).unwrap();
    assert!(task::state(&s, "tasks/t").unwrap().is_none());
}

#[test]
fn a_manual_run_of_a_dormant_task_keeps_it_dormant() {
    let mut s = store();
    seed::seed_schemas(&mut s).unwrap();
    s.put(input(json!({"_id": "runners/cat", "_type": RUNNER_TYPE, "argv": ["cat"]}))).unwrap();
    add_task(&mut s, "tasks/t", "1h", None);
    let now = s.now().unwrap();
    let e = task::evaluate(&s, &now, Some("tasks/t")).unwrap().remove(0);
    assert!(e.state.is_none());
    let claim = task::claim(&mut s, &e, &now, false).unwrap().unwrap();
    let state = task::state(&s, "tasks/t").unwrap().unwrap();
    assert!(!state.enabled && state.lease_until.is_some());
    assert_eq!(state.task_rev, e.task.rev);
    let outcome = task::Outcome { exit_code: Some(0), ..Default::default() };
    let receipt = task::finish(
        &mut s,
        &claim,
        &outcome,
        std::path::Path::new("/nonexistent"),
        std::time::Duration::from_secs(60),
        &now,
    )
    .unwrap();
    assert_eq!(receipt.body["task"], pinned("tasks/t", &e.task.rev));
}

#[test]
fn vault_id_is_made_once() {
    let s = store();
    let id = s.vault_id().unwrap();
    assert_eq!(uuid::Uuid::parse_str(&id).unwrap().get_version_num(), 7);
    assert_eq!(s.vault_id().unwrap(), id);
}
