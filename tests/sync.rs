use dreams::runner::{RUNNER_TYPE, Runner};
use dreams::seed;
use dreams::sync::{pull, sync};
use dreams::task::{self, RUN_TYPE, TASK_TYPE};
use dreams::{Doc, PutInput, Store, StoreError};
use serde_json::{Value, json};

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn input(v: Value) -> PutInput {
    serde_json::from_value(v).unwrap()
}

fn put(s: &mut Store, v: Value) -> Doc {
    s.put(input(v)).unwrap()
}

fn count(s: &Store) -> i64 {
    s.connection().query_row("SELECT count(*) FROM docs", [], |r| r.get(0)).unwrap()
}

fn sync_ab(a: &mut Store, b: &mut Store) {
    sync(a, "a", b, "b").unwrap();
}

#[test]
fn pull_copies_everything_once_and_resumes() {
    let mut a = store();
    let mut b = store();
    let d1 = put(&mut b, json!({"_id": "x", "title": "one"}));
    put(&mut b, json!({"_id": "x", "_parent": d1.rev, "title": "two", "tags": ["t"]}));
    b.delete("x", &b.get("x").unwrap().rev).unwrap();
    put(&mut b, json!({"_id": "y", "title": "why", "content": "searchable words"}));

    let r = pull(&mut a, &b, "b").unwrap();
    assert_eq!((r.read, r.written, r.present), (4, 4, 0));
    assert_eq!(count(&a), 4);
    assert!(matches!(a.get("x"), Err(StoreError::Deleted { .. })));
    let y = a.get("y").unwrap();
    assert_eq!(y.rev, b.get("y").unwrap().rev);
    assert_eq!(y.created_at, b.get("y").unwrap().created_at, "keeps the source's time");
    assert_eq!(a.search("searchable", &Default::default()).unwrap().results.len(), 1, "projections follow");

    let again = pull(&mut a, &b, "b").unwrap();
    assert_eq!((again.read, again.written), (0, 0));

    put(&mut b, json!({"_id": "z", "title": "zed"}));
    let resumed = pull(&mut a, &b, "b").unwrap();
    assert_eq!((resumed.read, resumed.written), (1, 1));
    assert_eq!(resumed.last_seq, 5);
}

#[test]
fn actor_is_kept() {
    let mut a = store();
    let mut b = store();
    b.set_actor(Some("tasks/digest".into()));
    put(&mut b, json!({"_id": "x", "title": "one"}));
    a.set_actor(Some("someone-else".into()));
    pull(&mut a, &b, "b").unwrap();
    assert_eq!(a.get("x").unwrap().actor.as_deref(), Some("tasks/digest"));
}

#[test]
fn concurrent_edits_conflict_identically_and_resolve() {
    let mut a = store();
    let mut b = store();
    let base = put(&mut a, json!({"_id": "x", "title": "base"}));
    sync_ab(&mut a, &mut b);
    put(&mut a, json!({"_id": "x", "_parent": base.rev, "title": "from a"}));
    put(&mut b, json!({"_id": "x", "_parent": base.rev, "title": "from b"}));
    sync_ab(&mut a, &mut b);

    let (xa, xb) = (a.get("x").unwrap(), b.get("x").unwrap());
    assert_eq!(xa.rev, xb.rev, "same winner in both vaults");
    assert_eq!(xa.conflicts, xb.conflicts);
    assert_eq!(xa.conflicts.len(), 1);
    let loser = xa.conflicts[0].clone();

    // the wrong parent for a merge is a conflict
    let err = a.resolve("x", Some(input(json!({"_parent": loser, "title": "merged"}))), None).unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }), "{err}");

    let resolved = a.resolve("x", Some(input(json!({"title": "merged"}))), None).unwrap();
    assert_eq!(resolved.body["title"], "merged");
    assert_eq!(resolved.parent.as_deref(), Some(xa.rev.as_str()));
    assert!(resolved.conflicts.is_empty());
    sync_ab(&mut a, &mut b);

    let xb = b.get("x").unwrap();
    assert_eq!(xb.body["title"], "merged");
    assert!(xb.conflicts.is_empty());
    let tombstones: i64 = b
        .connection()
        .query_row("SELECT count(*) FROM docs WHERE _parent = ?1 AND _deleted = 1", [&loser], |r| r.get(0))
        .unwrap();
    assert_eq!(tombstones, 1, "the loser is tombstoned");
    assert!(b.get("x").is_ok(), "the tombstone on the loser does not delete the document");
}

#[test]
fn resolve_without_merge_keeps_the_winner() {
    let mut a = store();
    let mut b = store();
    let base = put(&mut a, json!({"_id": "x", "title": "base"}));
    sync_ab(&mut a, &mut b);
    put(&mut a, json!({"_id": "x", "_parent": base.rev, "title": "from a"}));
    put(&mut b, json!({"_id": "x", "_parent": base.rev, "title": "from b"}));
    sync_ab(&mut a, &mut b);
    let winner = a.get("x").unwrap();
    let resolved = a.resolve("x", None, None).unwrap();
    assert_eq!(resolved.rev, winner.rev);
    assert!(resolved.conflicts.is_empty());
    assert_eq!(a.resolve("x", None, None).unwrap().rev, winner.rev, "no conflicts is a no-op");
}

#[test]
fn an_edit_beats_a_concurrent_delete() {
    let mut a = store();
    let mut b = store();
    let base = put(&mut a, json!({"_id": "x", "title": "base"}));
    sync_ab(&mut a, &mut b);
    a.delete("x", &base.rev).unwrap();
    put(&mut b, json!({"_id": "x", "_parent": base.rev, "title": "kept"}));
    sync_ab(&mut a, &mut b);
    for s in [&a, &b] {
        let x = s.get("x").unwrap();
        assert_eq!(x.body["title"], "kept");
        assert!(x.conflicts.is_empty(), "a deleted leaf is not a conflict");
    }
}

#[test]
fn a_tampered_revision_fails_the_batch() {
    let mut a = store();
    let b = store();
    b.connection()
        .execute("INSERT INTO docs(_rev,_id,body) VALUES ('1-abcd','x','{\"title\":\"forged\"}')", [])
        .unwrap();
    let err = pull(&mut a, &b, "b").unwrap_err();
    assert!(err.to_string().contains("does not match its content"), "{err}");
    assert_eq!(count(&a), 0);
    assert_eq!(a.checkpoint("b").unwrap(), None, "the checkpoint does not move");
}

#[test]
fn a_missing_parent_is_skipped() {
    let mut a = store();
    let mut b = store();
    let d1 = put(&mut b, json!({"_id": "x", "title": "one"}));
    let d2 = put(&mut b, json!({"_id": "x", "_parent": d1.rev, "title": "two"}));
    let report = a.apply_replicas("b", std::slice::from_ref(&d2), 2, &d2.rev).unwrap();
    assert_eq!((report.written, report.missing_parent), (0, 1));
    assert_eq!(count(&a), 0);
}

#[test]
fn tasks_runs_and_runners_replicate_but_tasks_arrive_dormant() {
    let mut a = store();
    let mut b = store();
    seed::seed(&mut a).unwrap();
    seed::seed(&mut b).unwrap();
    let fresh = sync(&mut a, "a", &mut b, "b").unwrap();
    for r in &fresh {
        assert_eq!((r.written, r.present), (0, 15), "seeded documents are identical");
    }

    put(
        &mut b,
        json!({"_id": "tasks/t", "_type": TASK_TYPE, "runner": "doc://runners/claude", "every": "1h", "prompt": "go"}),
    );
    let b_vault = b.vault_id().unwrap();
    put(
        &mut b,
        json!({"_id": "runs/1", "_type": RUN_TYPE, "task": "doc://tasks/t", "runner": "doc://runners/claude",
                       "vault": b_vault, "started_at": "2026-09-23T10:00:00.000Z",
                       "finished_at": "2026-09-23T10:01:00.000Z", "tags": ["tasks/t"]}),
    );
    let plan = task::plan_deploy(&b, Some("tasks/t")).unwrap();
    task::apply_deploy(&mut b, &plan).unwrap();
    let claude = b.get("runners/claude").unwrap();
    let mut body = Value::Object(claude.body.clone());
    body["_id"] = json!("runners/claude");
    body["_parent"] = json!(claude.rev);
    body["_type"] = json!(RUNNER_TYPE);
    body["argv"] = json!(["claude", "-p", "--edited"]);
    put(&mut b, body);

    let r = pull(&mut a, &b, "b").unwrap();
    assert_eq!((r.written, r.missing_parent), (3, 0));
    assert_eq!(a.get("runs/1").unwrap().body["vault"], b.vault_id().unwrap());
    assert_ne!(a.vault_id().unwrap(), b.vault_id().unwrap());
    let runner = Runner::get(&a, "doc://runners/claude").unwrap();
    assert_eq!(runner.argv, ["claude", "-p", "--edited"]);

    // deployed on b, dormant on a until a deploys it
    let now = a.now().unwrap();
    let e = task::evaluate(&a, &now, Some("tasks/t")).unwrap().remove(0);
    assert!(e.state.is_none() && !e.due);
    let deployed = task::state(&b, "tasks/t").unwrap().unwrap();
    let plan = task::plan_deploy(&a, Some("tasks/t")).unwrap();
    task::apply_deploy(&mut a, &plan).unwrap();
    assert!(task::state(&a, "tasks/t").unwrap().unwrap().enabled);

    // a synced edit runs on a only after a deploys it; a synced tombstone ends it everywhere
    let head = b.get("tasks/t").unwrap();
    let edit = put(
        &mut b,
        json!({"_id": "tasks/t", "_parent": head.rev, "_type": TASK_TYPE, "runner": "doc://runners/claude",
               "every": "1h", "prompt": "changed on b"}),
    );
    sync_ab(&mut a, &mut b);
    let e = task::evaluate(&a, &now, Some("tasks/t")).unwrap().remove(0);
    assert_eq!((e.task.rev.as_str(), e.drift), (deployed.task_rev.as_str(), true));
    b.delete("tasks/t", &edit.rev).unwrap();
    sync_ab(&mut a, &mut b);
    assert!(task::state(&a, "tasks/t").unwrap().is_none());
    assert!(task::state(&b, "tasks/t").unwrap().is_none());
}

#[test]
fn a_document_that_changes_type_replicates_whole() {
    let mut a = store();
    let mut b = store();
    seed::seed(&mut b).unwrap();
    let note = put(&mut b, json!({"_id": "tasks/t", "title": "an idea"}));
    put(
        &mut b,
        json!({"_id": "tasks/t", "_parent": note.rev, "_type": TASK_TYPE, "runner": "doc://runners/claude",
               "every": "1h", "prompt": "go"}),
    );
    let r = pull(&mut a, &b, "b").unwrap();
    assert_eq!(r.missing_parent, 0);
    assert_eq!(a.get("tasks/t").unwrap().type_path(), Some(TASK_TYPE));
}

#[test]
fn a_typed_document_arrives_with_its_schema() {
    let mut a = store();
    let mut b = store();
    let schema = put(&mut b, json!({"_id": "schemas/note", "type": "object", "required": ["title"]}));
    let note = put(&mut b, json!({"_id": "n", "_type": "doc://schemas/note", "title": "t"}));
    pull(&mut a, &b, "b").unwrap();
    let got = a.get("n").unwrap();
    assert_eq!(got.type_id, note.type_id);
    assert_eq!(a.get_rev(&schema.rev).unwrap().id, "schemas/note");
    // a local edit validates against the replicated schema
    let err = a.put(input(json!({"_id": "n", "_parent": got.rev, "_type": "doc://schemas/note"}))).unwrap_err();
    assert!(matches!(err, StoreError::Validation { .. }), "{err}");
}

#[test]
fn a_replaced_peer_restarts_from_zero() {
    let mut a = store();
    let mut b = store();
    put(&mut b, json!({"_id": "x", "title": "one"}));
    put(&mut b, json!({"_id": "y", "title": "two"}));
    pull(&mut a, &b, "peer").unwrap();

    let mut c = store();
    put(&mut c, json!({"_id": "p", "title": "other"}));
    put(&mut c, json!({"_id": "q", "title": "vault"}));
    put(&mut c, json!({"_id": "r", "title": "entirely"}));
    let r = pull(&mut a, &c, "peer").unwrap();
    assert!(r.restarted);
    assert_eq!((r.read, r.written), (3, 3));
    assert_eq!(count(&a), 5);
}

/// Two vaults, one document, one concurrent edit on each side, synced.
/// Returns (a, b, base rev).
fn forked(id: &str) -> (Store, Store, String) {
    let mut a = store();
    let mut b = store();
    let base = put(&mut a, json!({"_id": id, "title": "base"}));
    sync_ab(&mut a, &mut b);
    put(&mut a, json!({"_id": id, "_parent": base.rev, "title": "from a"}));
    put(&mut b, json!({"_id": id, "_parent": base.rev, "title": "from b"}));
    sync_ab(&mut a, &mut b);
    (a, b, base.rev)
}

#[test]
fn deleted_conflicts_list_the_discarded_losers() {
    let (mut a, _b, _) = forked("x");
    let loser = a.get("x").unwrap().conflicts[0].clone();
    let winner = a.get("x").unwrap().rev;
    assert!(a.deleted_conflicts("x", &winner).unwrap().is_empty());
    let resolved = a.resolve("x", None, None).unwrap();
    assert!(resolved.deleted_conflicts.is_empty(), "only when asked for");
    let tombstones = a.deleted_conflicts("x", &resolved.rev).unwrap();
    assert_eq!(tombstones.len(), 1);
    assert_eq!(a.get_rev(&tombstones[0]).unwrap().parent.as_deref(), Some(loser.as_str()));
}

#[test]
fn conflicted_lists_ids_in_order_with_paging() {
    let (mut a, mut b, _) = forked("m");
    for id in ["c", "q"] {
        let base = put(&mut a, json!({"_id": id, "title": "base"}));
        sync_ab(&mut a, &mut b);
        put(&mut a, json!({"_id": id, "_parent": base.rev, "title": "a"}));
        put(&mut b, json!({"_id": id, "_parent": base.rev, "title": "b"}));
    }
    put(&mut a, json!({"_id": "calm", "title": "no conflict"}));
    sync_ab(&mut a, &mut b);

    let all = a.conflicted(None, None).unwrap();
    let ids: Vec<&str> = all.docs.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["c", "m", "q"]);
    assert_eq!(all.docs[1].rev, a.get("m").unwrap().rev);
    assert_eq!(all.docs[1].conflicts, a.get("m").unwrap().conflicts);
    assert!(all.next.is_none());

    let first = a.conflicted(None, Some(2)).unwrap();
    assert_eq!(first.docs.len(), 2);
    let rest = a.conflicted(first.next.as_deref(), Some(2)).unwrap();
    assert_eq!(rest.docs.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["q"]);

    a.resolve("m", None, None).unwrap();
    let after: Vec<String> = a.conflicted(None, None).unwrap().docs.into_iter().map(|d| d.id).collect();
    assert_eq!(after, ["c", "q"]);
}

#[test]
fn the_common_ancestor_is_the_fork_point() {
    let (a, _b, base) = forked("x");
    let head = a.get("x").unwrap();
    assert_eq!(a.ancestors(&head.rev).unwrap(), [head.rev.clone(), base.clone()]);
    let ancestor = dreams::resolve::common_ancestor(&a, &head.rev, &head.conflicts).unwrap();
    assert_eq!(ancestor, Some(base));

    // two vaults that each created the same id share nothing
    let mut c = store();
    let mut d = store();
    put(&mut c, json!({"_id": "y", "title": "c"}));
    put(&mut d, json!({"_id": "y", "title": "d"}));
    sync(&mut c, "c", &mut d, "d").unwrap();
    let y = c.get("y").unwrap();
    assert_eq!(dreams::resolve::common_ancestor(&c, &y.rev, &y.conflicts).unwrap(), None);
}

#[test]
fn resolve_refuses_when_the_conflicts_changed() {
    let mut a = store();
    let mut b = store();
    let mut c = store();
    let base = put(&mut a, json!({"_id": "x", "title": "base"}));
    sync(&mut a, "a", &mut b, "b").unwrap();
    sync(&mut a, "a", &mut c, "c").unwrap();
    for (s, title) in [(&mut a, "a"), (&mut b, "b"), (&mut c, "c")] {
        put(s, json!({"_id": "x", "_parent": base.rev, "title": title}));
    }
    sync(&mut a, "a", &mut b, "b").unwrap();
    let seen = a.get("x").unwrap().conflicts;
    assert_eq!(seen.len(), 1);

    // a third edit arrives from another sync while the merge is being written
    pull(&mut a, &c, "c").unwrap();
    let before = count(&a);
    let err = a.resolve("x", Some(input(json!({"title": "merged"}))), Some(&seen)).unwrap_err();
    assert!(err.to_string().contains("changed since they were read"), "{err}");
    assert_eq!(count(&a), before, "nothing is written");

    let all = a.get("x").unwrap().conflicts;
    assert_eq!(all.len(), 2);
    assert!(a.resolve("x", None, Some(&all)).unwrap().conflicts.is_empty());
}
