use serde_json::{Value, json};
use subconscious::{ListQuery, PutInput, Store, StoreError};

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn input(v: Value) -> PutInput {
    serde_json::from_value(v).unwrap()
}

fn note_schema() -> Value {
    json!({
        "$id": "note/v1",
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

fn ids(page: &subconscious::Page) -> Vec<&str> {
    page.docs.iter().map(|d| d.id.as_str()).collect()
}

#[test]
fn genesis_update_and_conflicts() {
    let mut s = store();
    let d1 = s.put(input(json!({"_id": "a", "title": "one"}))).unwrap();
    assert!(d1.rev.starts_with("1-"));
    assert_eq!(d1.parent, None);
    assert_eq!(d1.body["title"], "one");

    let d2 = s
        .put(input(json!({"_id": "a", "_parent": d1.rev, "title": "two"})))
        .unwrap();
    assert!(d2.rev.starts_with("2-"));
    assert_eq!(d2.parent.as_deref(), Some(d1.rev.as_str()));
    assert_eq!(s.get("a").unwrap().rev, d2.rev);

    // stale parent
    let err = s
        .put(input(json!({"_id": "a", "_parent": d1.rev, "title": "three"})))
        .unwrap_err();
    match err {
        StoreError::Conflict { id, parent, leaves } => {
            assert_eq!(id, "a");
            assert_eq!(parent.as_deref(), Some(d1.rev.as_str()));
            assert_eq!(leaves, vec![d2.rev.clone()]);
        }
        other => panic!("{other:?}"),
    }
    // genesis on an existing doc
    assert!(matches!(
        s.put(input(json!({"_id": "a", "title": "four"}))),
        Err(StoreError::Conflict { .. })
    ));
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
    assert!(matches!(
        s.put(input(json!({"_id": "a", "title": "one", "n": 1}))),
        Err(StoreError::Conflict { .. })
    ));
    assert_eq!(s.put(input(json!({"_id": "a", "_parent": d1.rev, "title": "two"}))).unwrap().rev, d2.rev);
}

#[test]
fn generated_ids_are_uuid_v7() {
    let mut s = store();
    let d = s.put(input(json!({"title": "x"}))).unwrap();
    let u = uuid::Uuid::parse_str(&d.id).unwrap();
    assert_eq!(u.get_version_num(), 7);
}

#[test]
fn delete_tombstone_and_undelete() {
    let mut s = store();
    s.register_schema(note_schema()).unwrap();
    let d1 = s
        .put(input(json!({"_id": "a", "_type": "note/v1", "title": "t", "tags": ["x"]})))
        .unwrap();
    // wrong parent
    assert!(matches!(s.delete("a", "1-nope"), Err(StoreError::NotFound { .. })));
    // required title would fail validation, but tombstones skip it
    let tomb = s.delete("a", &d1.rev).unwrap();
    assert!(tomb.deleted);
    assert_eq!(tomb.type_id.as_deref(), Some("note/v1"));
    assert!(tomb.body.is_empty());

    match s.get("a") {
        Err(StoreError::Deleted { id, rev }) => {
            assert_eq!(id, "a");
            assert_eq!(rev, tomb.rev);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(s.delete("a", &tomb.rev), Err(StoreError::Deleted { .. })));
    assert!(s.list(&ListQuery::default()).unwrap().docs.is_empty());
    assert!(s.list(&ListQuery { tag: Some("x".into()), ..Default::default() }).unwrap().docs.is_empty());
    assert!(s.search("t", &ListQuery::default()).unwrap().docs.is_empty());

    let back = s
        .put(input(json!({"_id": "a", "_parent": tomb.rev, "_type": "note/v1", "title": "back", "tags": ["x"]})))
        .unwrap();
    assert!(back.rev.starts_with("3-"));
    assert_eq!(s.get("a").unwrap().body["title"], "back");
    assert_eq!(ids(&s.list(&ListQuery { tag: Some("x".into()), ..Default::default() }).unwrap()), vec!["a"]);
    assert_eq!(s.history("a", None).unwrap().revisions.len(), 3);
}

#[test]
fn tags_follow_the_current_revision() {
    let mut s = store();
    let d1 = s.put(input(json!({"_id": "a", "title": "a", "tags": ["x", "y"]}))).unwrap();
    s.put(input(json!({"_id": "b", "title": "b", "tags": ["x"]}))).unwrap();
    let by = |s: &Store, tag: &str| ids(&s.list(&ListQuery { tag: Some(tag.into()), ..Default::default() }).unwrap()).into_iter().map(String::from).collect::<Vec<_>>();
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
    assert!(matches!(
        s.put(input(json!({"_type": "note/v1", "title": "x"}))),
        Err(StoreError::UnknownType { .. })
    ));
    let summary = s.register_schema(note_schema()).unwrap();
    assert_eq!(summary.title, "Note");
    assert!(s.put(input(json!({"_type": "note/v1", "title": "x"}))).is_ok());

    match s.put(input(json!({"_type": "note/v1", "content": "no title"}))) {
        Err(StoreError::Validation { schema, errors }) => {
            assert_eq!(schema, "note/v1");
            assert!(errors.iter().any(|e| e.message.contains("title")), "{errors:?}");
        }
        other => panic!("{other:?}"),
    }
    match s.put(input(json!({"_type": "note/v1", "title": ""}))) {
        Err(StoreError::Validation { errors, .. }) => {
            assert!(errors.iter().any(|e| e.path == "/title"), "{errors:?}");
            // blessed-field type errors are caught before the schema runs
            assert!(matches!(
                s.put(input(json!({"_type": "note/v1", "title": "x", "tags": [1]}))),
                Err(StoreError::InvalidInput { .. })
            ));
        }
        other => panic!("{other:?}"),
    }
    // registry behavior through the store
    s.register_schema(note_schema()).unwrap();
    let mut changed = note_schema();
    changed["required"] = json!([]);
    assert!(matches!(s.register_schema(changed), Err(StoreError::ImmutableSchema { .. })));
    assert_eq!(s.list_schemas().unwrap().schemas.len(), 1);
    assert_eq!(s.get_schema("note/v1").unwrap()["title"], "Note");
    assert!(matches!(s.get_schema("nope"), Err(StoreError::UnknownType { .. })));
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
    let d1 = s
        .put(input(json!({"_id": "a", "title": "Alpha", "content": "first draft", "tags": ["blue"]})))
        .unwrap();
    s.put(input(json!({"_id": "b", "title": "Beta", "content": "unrelated"}))).unwrap();
    assert_eq!(ids(&s.search("first", &ListQuery::default()).unwrap()), vec!["a"]);
    assert_eq!(ids(&s.search("blue", &ListQuery::default()).unwrap()), vec!["a"]);

    s.put(input(json!({"_id": "a", "_parent": d1.rev, "title": "Alpha", "content": "second draft"}))).unwrap();
    assert!(s.search("first", &ListQuery::default()).unwrap().docs.is_empty());
    assert_eq!(ids(&s.search("second", &ListQuery::default()).unwrap()), vec!["a"]);
    assert!(s.search("blue", &ListQuery::default()).unwrap().docs.is_empty());

    // stemming and operators in user input do not error
    assert_eq!(ids(&s.search("drafts", &ListQuery::default()).unwrap()), vec!["a"]);
    assert!(s.search("hello AND", &ListQuery::default()).is_ok());
    assert!(s.search("\"unbalanced (", &ListQuery::default()).is_ok());
    // filters
    assert!(s.search("second", &ListQuery { tag: Some("blue".into()), ..Default::default() }).unwrap().docs.is_empty());
    // empty query lists
    assert_eq!(s.search("   ", &ListQuery::default()).unwrap().docs.len(), 2);
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
    assert!(conn
        .execute("INSERT INTO docs(_rev,_id,_parent,_type,_deleted,body) VALUES ('2-ff','z',NULL,NULL,0,'{}')", [])
        .is_err());
}

#[test]
fn reopening_a_file_keeps_data_and_does_not_remigrate() {
    let dir = std::env::temp_dir().join(format!("subconscious-test-{}", uuid::Uuid::now_v7()));
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
        assert_eq!(n, 1);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
