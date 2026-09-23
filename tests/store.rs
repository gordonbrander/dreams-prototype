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
    // note/v1 plus the three built-in schemas (run, runner, task)
    assert_eq!(s.list_schemas().unwrap().schemas.len(), 4);
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
        assert_eq!(n, 2);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- scheduled tasks ------------------------------------------------------

use subconscious::runner::{self, PROTECTED_TYPES, RUNNER_TYPE};
use subconscious::task::{self, RUN_TYPE, TASK_TYPE};

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
    let doc = s
        .put(input(json!({"_id": "runners/x", "_type": RUNNER_TYPE, "argv": ["cat"]})))
        .unwrap();
    s.set_protected(PROTECTED_TYPES);
    assert!(matches!(
        s.put(input(json!({"_id": "runners/y", "_type": RUNNER_TYPE, "argv": ["cat"]}))),
        Err(StoreError::Protected { .. })
    ));
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
    s.set_protected(&[]);
    assert!(s.delete("runners/x", &doc.rev).is_ok());
}

#[test]
fn seed_defaults_is_idempotent_and_respects_deletions() {
    let mut s = store();
    let first = runner::seed_defaults(&mut s).unwrap();
    assert_eq!(first, ["runners/claude", "runners/codex", "runners/pi"]);
    assert!(runner::seed_defaults(&mut s).unwrap().is_empty());
    let pi = s.get("runners/pi").unwrap();
    assert_eq!(pi.type_id.as_deref(), Some(RUNNER_TYPE));
    s.delete("runners/pi", &pi.rev).unwrap();
    assert!(runner::seed_defaults(&mut s).unwrap().is_empty());
    assert!(matches!(s.get("runners/pi"), Err(StoreError::Deleted { .. })));
}

fn add_task(s: &mut Store, id: &str, every: &str, when: Option<Value>) -> subconscious::Doc {
    let mut body = json!({"_id": id, "_type": TASK_TYPE, "runner": "runners/cat", "every": every, "prompt": "go"});
    if let Some(w) = when {
        body["when"] = w;
    }
    s.put(input(body)).unwrap()
}

#[test]
fn evaluate_time_and_change_rules() {
    let mut s = store();
    s.put(input(json!({"_id": "runners/cat", "_type": RUNNER_TYPE, "argv": ["cat"], "timeout": "1m"}))).unwrap();
    let plain = add_task(&mut s, "tasks/plain", "1h", None);
    let watch = add_task(&mut s, "tasks/watch", "15m", Some(json!({"tag": "inbox"})));
    let created = watch.created_at.clone();

    // before the interval: nothing is due, and the cursor is the task's own seq
    let evals = task::evaluate(&s, &plus_secs(&s, &created, 60), None).unwrap();
    assert_eq!(evals.len(), 2);
    let e_plain = evals.iter().find(|e| e.task.id == "tasks/plain").unwrap();
    let e_watch = evals.iter().find(|e| e.task.id == "tasks/watch").unwrap();
    assert!(!e_plain.time_due && !e_plain.due);
    assert_eq!(e_watch.cursor, 3);
    assert_eq!(e_watch.head, 3);
    assert!(e_watch.changes.is_empty());

    // after the interval: the plain task is due; the watcher waits for changes
    let later = plus_secs(&s, &created, 2 * 3600);
    let evals = task::evaluate(&s, &later, None).unwrap();
    let e_plain = evals.iter().find(|e| e.task.id == "tasks/plain").unwrap();
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
    assert_eq!(e_watch.head, 6);

    // a tombstone of a tagged document counts as a change to that tag
    s.delete("n1", &note.rev).unwrap();
    let e_watch = task::evaluate(&s, &later, Some("tasks/watch")).unwrap().remove(0);
    let seen: Vec<(String, bool)> = e_watch.changes.iter().map(|c| (c.id.clone(), c.deleted)).collect();
    assert_eq!(seen, [("n1".to_string(), false), ("n1".to_string(), true)]);

    // a claim marks the task running until its timeout, then it is stale
    let run = task::claim(&mut s, &e_watch, &later).unwrap().unwrap();
    assert_eq!(run.body["seq"], 7);
    assert_eq!(run.body["tags"], json!(["tasks/watch"]));
    let e_watch = task::evaluate(&s, &plus_secs(&s, &later, 30), Some("tasks/watch")).unwrap().remove(0);
    assert!(e_watch.running && !e_watch.due);
    assert_eq!(e_watch.cursor, 7);
    assert_eq!(e_watch.last_run.as_deref(), Some(run.id.as_str()));
    let e_watch = task::evaluate(&s, &plus_secs(&s, &later, 3600), Some("tasks/watch")).unwrap().remove(0);
    assert!(!e_watch.running);
    // nothing changed since the claim's cursor, so it is not due even though time has passed
    assert!(e_watch.time_due && !e_watch.due);

    // a second claim against the same evaluation fails once the newest run moved on
    let fresh = task::evaluate(&s, &later, Some("tasks/plain")).unwrap().remove(0);
    assert!(task::claim(&mut s, &fresh, &later).unwrap().is_some());
    let e_plain_old = e_plain.clone();
    assert!(task::claim(&mut s, &e_plain_old, &later).unwrap().is_none());

    // disabled tasks are skipped by the full evaluation but visible by id
    let plain_doc = s.get("tasks/plain").unwrap();
    s.put(input(json!({"_id": "tasks/plain", "_parent": plain_doc.rev, "_type": TASK_TYPE,
        "runner": "runners/cat", "every": "1h", "prompt": "go", "enabled": false}))).unwrap();
    assert!(task::evaluate(&s, &later, None).unwrap().iter().all(|e| e.task.id != "tasks/plain"));
    let e = task::evaluate(&s, &later, Some("tasks/plain")).unwrap().remove(0);
    assert!(!e.due && !e.parsed.enabled);
    assert!(matches!(task::evaluate(&s, &later, Some("nope")), Err(StoreError::NotFound { .. })));
    assert!(task::evaluate(&s, &later, Some("n2")).is_err());
    let _ = plain;
}
