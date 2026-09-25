use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use dreams::{Changes, History, Page, SearchPage, cli, seed};
use serde_json::{Value, json};

struct Sandbox {
    dir: PathBuf,
    /// The person's answer to a deploy confirmation. `None`: no terminal to ask on.
    answer: Cell<Option<bool>>,
    /// Every confirmation text shown.
    asked: RefCell<Vec<String>>,
}

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("dreams-cli-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Sandbox { dir, answer: Cell::new(Some(true)), asked: RefCell::new(Vec::new()) }
    }

    fn db(&self) -> String {
        self.dir.join("vault.db").to_string_lossy().into_owned()
    }

    fn file(&self, name: &str, text: &str) -> String {
        let path = self.dir.join(name);
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Run the CLI in-process. Returns (exit code, stdout, stderr).
    fn run(&self, args: &[&str], stdin: &str) -> (i32, String, String) {
        let mut argv = vec!["dreams".to_string(), "--db".to_string(), self.db()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let mut input = stdin.as_bytes();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let answer = self.answer.get();
        let mut confirm = |text: &str| {
            self.asked.borrow_mut().push(text.to_string());
            answer.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no terminal"))
        };
        let code = cli::run(argv, &mut input, &mut out, &mut err, &mut confirm);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    fn ok(&self, args: &[&str], stdin: &str) -> String {
        let (code, out, err) = self.run(args, stdin);
        assert_eq!(code, 0, "args={args:?}\nstdout={out}\nstderr={err}");
        out
    }

    fn json(&self, args: &[&str], stdin: &str) -> Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        serde_json::from_str(&self.ok(&full, stdin)).unwrap()
    }

    fn fails(&self, args: &[&str], stdin: &str) -> Value {
        let (code, out, err) = self.run(args, stdin);
        assert_eq!(code, 1, "args={args:?}\nstdout={out}\nstderr={err}");
        serde_json::from_str(&err).unwrap_or_else(|_| panic!("stderr was not JSON: {err}"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const SCHEMA_YAML: &str = "\
_id: schemas/note
title: Note
description: A note
type: object
required: [title]
properties:
  title: {type: string, minLength: 1}
  content: {type: string}
  tags: {type: array, items: {type: string}}
";

const NOTE_MD: &str = "\
---
_id: n1
_type: doc://schemas/note
title: Hello
tags: [demo, blue]
---
first draft
";

fn frontmatter(md: &str) -> Value {
    let body = md.strip_prefix("---\n").unwrap();
    let end = body.find("\n---\n").unwrap();
    let map = cli::parse_input(&body[..end], cli::Format::Yaml).unwrap();
    Value::Object(map)
}

#[test]
fn register_and_put_from_files() {
    let sb = Sandbox::new();
    let schema = sb.file("note.yaml", SCHEMA_YAML);
    let out = sb.ok(&["doc", "put", &schema], "");
    let fm = frontmatter(&out);
    assert_eq!(fm["_id"], "schemas/note");
    assert!(fm.get("_type").is_none());
    let schema_rev = fm["_rev"].as_str().unwrap().to_string();

    let note = sb.file("note.md", NOTE_MD);
    let out = sb.ok(&["doc", "put", &note], "");
    let fm = frontmatter(&out);
    assert_eq!(fm["_id"], "n1");
    assert!(fm["_rev"].as_str().unwrap().starts_with("1-"));
    assert_eq!(fm["_type"], format!("doc://schemas/note?rev={schema_rev}"));
    assert_eq!(fm["tags"], json!(["demo", "blue"]));
    assert!(fm.get("_parent").is_none());
    assert!(out.ends_with("---\nfirst draft\n"), "{out}");
}

#[test]
fn markdown_round_trip_unchanged_then_edited() {
    let sb = Sandbox::new();
    let schema = sb.file("note.yaml", SCHEMA_YAML);
    sb.ok(&["doc", "put", &schema], "");
    let note = sb.file("note.md", NOTE_MD);
    sb.ok(&["doc", "put", &note], "");

    let fetched = sb.ok(&["doc", "get", "n1"], "");
    let rev1 = frontmatter(&fetched)["_rev"].as_str().unwrap().to_string();

    // unchanged: put and update are both no-ops
    let again = sb.ok(&["doc", "put", "--format", "md"], &fetched);
    assert_eq!(again, fetched);
    let again = sb.ok(&["doc", "update", "n1", "--format", "md"], &fetched);
    assert_eq!(again, fetched);
    let changes: Changes = serde_json::from_value(sb.json(&["doc", "changes"], "")).unwrap();
    assert_eq!(changes.results.len(), seeded() + 2, "the seeded documents, the schema, the note");

    // edited: update yields gen 2 with parent = rev 1
    let edited = fetched.replace("first draft", "second draft");
    let out = sb.ok(&["doc", "update", "n1", "--format", "md"], &edited);
    let fm = frontmatter(&out);
    assert!(fm["_rev"].as_str().unwrap().starts_with("2-"));
    assert_eq!(fm["_parent"], rev1);
    assert!(out.ends_with("second draft\n"));

    // a gen-2 fetch edited again: its _rev, not its stale _parent, becomes the parent
    let fetched2 = sb.ok(&["doc", "get", "n1"], "");
    let rev2 = frontmatter(&fetched2)["_rev"].as_str().unwrap().to_string();
    let out = sb.ok(&["doc", "put", "--format", "md"], &fetched2.replace("second", "third"));
    let fm = frontmatter(&out);
    assert!(fm["_rev"].as_str().unwrap().starts_with("3-"));
    assert_eq!(fm["_parent"], rev2);

    // explicit stale parent conflicts
    let err = sb.fails(&["doc", "update", "n1", "--parent", &rev1, "--format", "md"], &fetched2);
    assert_eq!(err["name"], "conflict");
    assert!(err["message"].as_str().unwrap().contains("conflict"));
}

#[test]
fn delete_revive_and_get_rev() {
    let sb = Sandbox::new();
    let d1 = sb.json(&["doc", "put"], r#"{"_id":"a","title":"one"}"#);
    let rev1 = d1["_rev"].as_str().unwrap().to_string();
    sb.json(&["doc", "put"], r#"{"_id":"b","title":"bee"}"#);

    let tomb = sb.json(&["doc", "delete", "a"], "");
    assert_eq!(tomb["_deleted"], true);
    assert!(tomb["_rev"].as_str().unwrap().starts_with("2-"));
    let err = sb.fails(&["doc", "get", "a"], "");
    assert_eq!(err["name"], "deleted");

    // update revives from the tombstone
    let back = sb.json(&["doc", "update", "a"], r#"{"title":"three"}"#);
    assert!(back["_rev"].as_str().unwrap().starts_with("3-"));
    assert_eq!(back["title"], "three");

    // putting a tombstone is refused
    let err = sb.fails(&["doc", "put"], &tomb.to_string());
    assert_eq!(err["name"], "invalid_input");

    // a pinned href reads history, but only this document's
    let old = sb.json(&["doc", "get", &format!("doc://a?rev={rev1}")], "");
    assert_eq!(old["title"], "one");
    let b = sb.json(&["doc", "get", "b"], "");
    assert_eq!(sb.json(&["doc", "get", "doc://b"], ""), b);
    let err = sb.fails(&["doc", "get", &format!("doc://a?rev={}", b["_rev"].as_str().unwrap())], "");
    assert_eq!(err["name"], "invalid_input");
    let err = sb.fails(&["doc", "get", &format!("doc://a?rev={rev1}"), "--deleted-conflicts"], "");
    assert_eq!(err["name"], "invalid_input");

    let history: History = serde_json::from_value(sb.json(&["doc", "history", "a"], "")).unwrap();
    assert_eq!(history.revisions.len(), 3);
    let text = sb.ok(&["doc", "history", "a"], "");
    assert!(text.starts_with("REV"), "{text}");
    assert!(text.contains("yes"), "{text}");
}

#[test]
fn lists_tables_and_json_shapes() {
    let sb = Sandbox::new();
    sb.json(&["doc", "put"], r#"{"_id":"a","title":"Alpha","tags":["x"]}"#);
    sb.json(&["doc", "put"], r#"{"_id":"b","title":"Beta","content":"searchable words"}"#);

    let text = sb.ok(&["doc", "list", "--tag", "x"], "");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), ["ID", "REV", "TYPE", "TITLE", "TAGS"]);
    assert_eq!(lines.len(), 2);
    let cells: Vec<&str> = lines[1].split_whitespace().collect();
    assert_eq!(cells.len(), 4, "{text}");
    assert_eq!(cells[0], "a");
    assert!(cells[1].starts_with("1-") && cells[1].len() == 10, "{text}");
    assert_eq!(&cells[2..], ["Alpha", "x"]);

    let page: Page = serde_json::from_value(sb.json(&["doc", "list"], "")).unwrap();
    assert_eq!(page.docs.len(), seeded() + 2, "the seeded documents plus a and b");
    let page: Page = serde_json::from_value(sb.json(&["doc", "list", "--limit", "1"], "")).unwrap();
    assert!(page.next.is_some());
    let text = sb.ok(&["doc", "list", "--limit", "1"], "");
    assert!(text.trim_end().ends_with(&format!("next: {}", page.next.unwrap())));

    let page: SearchPage = serde_json::from_value(sb.json(&["doc", "search", "searchable"], "")).unwrap();
    assert_eq!(page.results[0].id, "b");
    assert_eq!(page.results[0].content_matches.as_deref(), Some("**searchable** words"));
    let text = sb.ok(&["doc", "search", "searchable"], "");
    let header = text.lines().next().unwrap();
    assert_eq!(header.split_whitespace().collect::<Vec<_>>(), ["ID", "REV", "TYPE", "TITLE", "MATCH"]);
    assert!(text.contains("**searchable** words"), "{text}");

    let text = sb.ok(&["doc", "changes"], "");
    assert!(text.starts_with("SEQ"));
    assert!(text.trim_end().ends_with(&format!("last_seq: {}", seeded() + 2)));

    // type filters: a path matches every pinned revision, the TYPE column shows the path
    let schema = sb.file("note.yaml", SCHEMA_YAML);
    sb.ok(&["doc", "put", &schema], "");
    let typed = sb.json(&["doc", "put"], r#"{"_id":"c","_type":"doc://schemas/note","title":"Typed"}"#);
    let page: Page = serde_json::from_value(sb.json(&["doc", "list", "--type", "doc://schemas/note"], "")).unwrap();
    assert_eq!(page.docs.len(), 1);
    let page: Page =
        serde_json::from_value(sb.json(&["doc", "list", "--type", typed["_type"].as_str().unwrap()], "")).unwrap();
    assert_eq!(page.docs[0].id, "c");
    let text = sb.ok(&["doc", "list", "--type", "doc://schemas/note"], "");
    let row: Vec<&str> = text.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(row, ["c", row[1], "doc://schemas/note", "Typed"], "{text}");
    let schema: Value = sb.json(&["doc", "get", "schemas/note"], "");
    assert_eq!(schema["title"], "Note");
}

#[test]
fn input_formats_and_usage_errors() {
    let sb = Sandbox::new();
    let d = sb.json(&["doc", "put", "--format", "yaml"], "_id: y\ntitle: from yaml\ntags:\n  - a\n");
    assert_eq!(d["title"], "from yaml");
    assert_eq!(d["tags"], json!(["a"]));

    let d = sb.json(&["doc", "put", "-", "--format", "md"], "plain body\n");
    assert_eq!(d["content"], "plain body\n");

    let json_file = sb.file("doc.json", r#"{"_id":"j","title":"from file"}"#);
    let d = sb.json(&["doc", "put", &json_file], "");
    assert_eq!(d["_id"], "j");

    let unknown = sb.file("doc.txt", "x");
    let err = sb.fails(&["doc", "put", &unknown], "");
    assert!(err["message"].as_str().unwrap().contains("--format"));
    let err = sb.fails(&["doc", "put"], "[1,2]");
    assert_eq!(err["name"], "invalid_input");
    let err = sb.fails(&["doc", "put", Path::new("/nonexistent/x.json").to_str().unwrap()], "");
    assert_eq!(err["name"], "invalid_input");

    let (code, out, err) = sb.run(&["--help"], "");
    assert_eq!(code, 0);
    assert!(out.contains("Usage:"));
    assert!(err.is_empty());
    let (code, out, err) = sb.run(&["doc", "--bogus"], "");
    assert_eq!(code, 2);
    assert!(out.is_empty());
    assert!(err.contains("--bogus"));
}

#[test]
fn export_then_import_round_trip() {
    let sb = Sandbox::new();
    sb.json(&["doc", "put"], r#"{"_id":"a.md","title":"Alpha","content":"alpha body\n","tags":["x"]}"#);
    sb.json(&["doc", "put"], r#"{"_id":"notes/2026/b.md","title":"Bee","content":"bee body\n"}"#);
    sb.json(&["doc", "put"], r#"{"_id":"plain","title":"No extension"}"#);
    sb.json(&["doc", "put"], r#"{"_id":"data/c.json","title":"Sea","n":1}"#);
    sb.json(&["doc", "put"], r#"{"_id":"conf/d.yaml","title":"Dee","content":"dee body\n"}"#);
    let del = sb.json(&["doc", "put"], r#"{"_id":"gone.md","title":"Gone"}"#);
    sb.json(&["doc", "delete", "gone.md", "--parent", del["_rev"].as_str().unwrap()], "");

    let out_dir = sb.dir.join("export");
    let out = sb.ok(&["export", out_dir.to_str().unwrap()], "");
    assert!(out.trim_end().ends_with(&format!("{} exported, 0 errors", seeded() + 5)), "{out}");
    assert!(out_dir.join("schemas/task").exists());
    assert!(out_dir.join("a.md").exists());
    assert!(out_dir.join("notes/2026/b.md").exists());
    assert!(out_dir.join("plain").exists());
    assert!(!out_dir.join("gone.md").exists());
    let text = std::fs::read_to_string(out_dir.join("a.md")).unwrap();
    assert!(text.starts_with("---\n_id: a.md\n_rev: 1-"), "{text}");
    assert!(text.ends_with("---\nalpha body\n"), "{text}");
    let c: Value = serde_json::from_str(&std::fs::read_to_string(out_dir.join("data/c.json")).unwrap()).unwrap();
    assert_eq!(c["_id"], "data/c.json");
    assert_eq!(c["n"], 1);
    // empty reserved fields are left out, as in Markdown
    for key in ["_parent", "_type", "_deleted"] {
        assert!(c.get(key).is_none(), "{key}: {c}");
    }
    let d: Value = serde_yaml_ng::from_str(&std::fs::read_to_string(out_dir.join("conf/d.yaml")).unwrap()).unwrap();
    assert_eq!(d["_id"], "conf/d.yaml");
    assert_eq!(d["content"], "dee body\n");

    // exported and unchanged: every file is a no-op; `plain` has no format extension and is skipped
    let report = sb.json(&["import", out_dir.to_str().unwrap()], "");
    let statuses: Vec<&str> =
        report["results"].as_array().unwrap().iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["unchanged"; 4]);
    assert_eq!(report["errors"], 0);
    let changes: Changes = serde_json::from_value(sb.json(&["doc", "changes"], "")).unwrap();
    assert_eq!(changes.results.len(), seeded() + 7);

    // edit one, add one, and drop a copied file whose frontmatter names another doc
    std::fs::write(out_dir.join("a.md"), text.replace("alpha body", "alpha edited")).unwrap();
    std::fs::write(out_dir.join("notes/new.md"), "---\ntitle: New\n---\nfresh\n").unwrap();
    std::fs::copy(out_dir.join("notes/2026/b.md"), out_dir.join("copy.md")).unwrap();
    let c_text = std::fs::read_to_string(out_dir.join("data/c.json")).unwrap();
    std::fs::write(out_dir.join("data/c.json"), c_text.replace("\"n\": 1", "\"n\": 2")).unwrap();
    std::fs::write(out_dir.join("notes/new.yml"), "title: Yam\n").unwrap();
    let out = sb.ok(&["import", out_dir.to_str().unwrap()], "");
    assert!(out.contains("updated    a.md 2-"), "{out}");
    assert!(out.contains("created    notes/new.md 1-"), "{out}");
    assert!(out.contains("created    copy.md 1-"), "{out}");
    assert!(out.contains("updated    data/c.json 2-"), "{out}");
    assert!(out.contains("created    notes/new.yml 1-"), "{out}");
    assert!(out.contains("unchanged  notes/2026/b.md"), "{out}");
    assert!(out.contains("unchanged  conf/d.yaml"), "{out}");
    assert!(out.trim_end().ends_with("3 created, 2 updated, 2 unchanged, 0 errors"), "{out}");
    assert_eq!(sb.json(&["doc", "get", "data/c.json"], "")["n"], 2);
    assert_eq!(sb.json(&["doc", "get", "notes/new.yml"], "")["title"], "Yam");
    assert_eq!(sb.json(&["doc", "get", "a.md"], "")["content"], "alpha edited\n");
    assert_eq!(sb.json(&["doc", "get", "notes/new.md"], "")["title"], "New");
    let copy = sb.json(&["doc", "get", "copy.md"], "");
    assert_eq!(copy["title"], "Bee");
    assert!(copy["_rev"].as_str().unwrap().starts_with("1-"));

    // a file that fails to import is reported and the command exits 1, but the rest still land
    std::fs::write(out_dir.join("bad.md"), "---\ntitle: 7\n---\n").unwrap();
    std::fs::write(out_dir.join("good.md"), "---\ntitle: Good\n---\n").unwrap();
    let (code, out, err) = sb.run(&["import", out_dir.to_str().unwrap()], "");
    assert_eq!(code, 1);
    assert!(out.contains("error      bad.md:"), "{out}");
    assert!(out.contains("created    good.md"), "{out}");
    assert!(err.contains("1 file(s) failed"), "{err}");

    // export filters
    let filtered = sb.dir.join("tagged");
    sb.ok(&["export", filtered.to_str().unwrap(), "--tag", "x"], "");
    assert!(filtered.join("a.md").exists());
    assert!(!filtered.join("notes").exists());
}

// ---- scheduled tasks ------------------------------------------------------

const FUTURE: &str = "2099-01-01T00:00:00.000Z";

fn add_test_runners(sb: &Sandbox) {
    sb.ok(&["runner", "add", "runners/cat", "--", "cat"], "");
    sb.ok(&["runner", "add", "runners/fail", "--", "false"], "");
    sb.ok(&["runner", "add", "runners/slow", "--timeout", "1s", "--", "sleep", "30"], "");
    sb.ok(
        &[
            "runner",
            "add",
            "runners/env",
            "--",
            "sh",
            "-c",
            "cat >/dev/null; echo $DREAMS_TASK $DREAMS_ACTOR $DREAMS_DB",
        ],
        "",
    );
}

#[test]
fn seed_writes_the_built_in_documents_again() {
    let sb = Sandbox::new();
    sb.ok(&["init"], "");
    assert_eq!(sb.ok(&["restore-defaults"], ""), "nothing to seed\n");
    let skill = sb.file(
        "skill.json",
        r#"{"_type": "doc://schemas/skill", "name": "daily-note", "description": "x", "content": "y"}"#,
    );
    sb.ok(&["doc", "update", "skills/daily-note", &skill], "");
    sb.ok(&["runner", "rm", "runners/pi"], "");
    // other commands leave the edit and the deletion as they are
    assert_eq!(sb.json(&["doc", "get", "skills/daily-note"], "")["content"], "y");
    let out = sb.ok(&["restore-defaults"], "");
    assert_eq!(out, "seeded runners/pi\nseeded skills/daily-note\n");
    assert!(sb.json(&["doc", "get", "skills/daily-note"], "")["content"].as_str().unwrap().contains("daily"));
    assert_eq!(sb.json(&["restore-defaults"], "")["seeded"], serde_json::json!([]));
    // init also writes the defaults again
    sb.ok(&["doc", "update", "skills/daily-note", &skill], "");
    assert!(sb.ok(&["init"], "").contains("seeded skills/daily-note\n"));
}

#[test]
fn runners_are_documents_seeded_once() {
    let sb = Sandbox::new();
    let out = sb.ok(&["init"], "");
    assert!(out.contains("seeded schemas/task"), "{out}");
    assert!(out.contains("seeded runners/claude"), "{out}");
    assert!(out.contains("seeded skills/daily-note"), "{out}");
    let task_schema = sb.json(&["doc", "get", "schemas/task"], "");
    assert_eq!(task_schema["title"], "Scheduled task");
    assert!(task_schema["_type"].is_null());
    let text = sb.ok(&["runner", "list"], "");
    let ids: Vec<&str> = text.lines().skip(1).map(|l| l.split_whitespace().next().unwrap()).collect();
    let mut seeded: Vec<&str> = seed::RUNNERS.iter().map(|(id, _, _)| *id).collect();
    seeded.sort();
    assert_eq!(ids, seeded);
    add_test_runners(&sb);
    let out = sb.ok(&["runner", "add", "runners/cat", "--", "cat"], "");
    assert!(out.starts_with("unchanged runners/cat"), "{out}");
    sb.ok(&["runner", "rm", "runners/pi"], "");
    let text = sb.ok(&["runner", "list"], "");
    assert!(!text.contains("runners/pi"), "{text}");
    assert_eq!(text.lines().count(), 1 + seeded.len() - 1 + 4, "{text}");
    let doc = sb.json(&["doc", "get", "runners/slow"], "");
    assert!(doc["_type"].as_str().unwrap().starts_with("doc://schemas/runner?rev=1-"), "{doc}");
    assert_eq!(doc["argv"], json!(["sleep", "30"]));
    assert_eq!(doc["timeout"], "1s");
    let err = sb.fails(&["runner", "add", "runners/bad", "--timeout", "soon", "--", "cat"], "");
    assert_eq!(err["name"], "invalid_input");
    let err = sb.fails(&["runner", "rm", "nope"], "");
    assert_eq!(err["name"], "not_found");
}

#[test]
fn task_lifecycle_with_change_trigger() {
    let sb = Sandbox::new();
    add_test_runners(&sb);
    let prompt = sb.file("prompt.md", "Triage these.\n");
    let seeded_tasks = sb.json(&["task", "list"], "").as_array().unwrap().len();

    let err = sb.fails(&["task", "add", "t1", "--runner", "runners/nope", "--every", "1h", &prompt], "");
    assert_eq!(err["name"], "not_found");
    let err = sb.fails(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1x", &prompt], "");
    assert_eq!(err["name"], "invalid_input");

    let out = sb.ok(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1h", "--tag", "inbox", &prompt], "");
    assert!(out.starts_with("created t1 1-"), "{out}");
    assert!(out.contains("\ndeployed t1 1-"), "{out}");
    // the person confirmed exactly what runs: the prompt and the command
    let asked = sb.asked.borrow_mut().pop().unwrap();
    assert!(asked.contains("Triage these.") && asked.contains("command: cat"), "{asked}");
    assert!(asked.contains("(now dormant)"), "{asked}");
    let task = sb.json(&["doc", "get", "t1"], "");
    assert!(task["_type"].as_str().unwrap().starts_with("doc://schemas/task?rev=1-"), "{task}");
    assert_eq!(task["runner"], "doc://runners/cat");
    assert_eq!(task["when"], json!({"tag": "inbox"}));
    assert_eq!(task["prompt"], "Triage these.\n");
    // the doc:// form of --runner is accepted too, and an identical re-add writes nothing
    let out =
        sb.ok(&["task", "add", "t1", "--runner", "doc://runners/cat", "--every", "1h", "--tag", "inbox", &prompt], "");
    assert!(out.starts_with("unchanged t1 1-"), "{out}");
    assert!(sb.asked.borrow().is_empty(), "nothing new to deploy, so nothing to confirm");

    let list: Value = sb.json(&["task", "list"], "");
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), seeded_tasks + 1);
    let list = list.iter().find(|t| t["task"]["_id"] == "t1").unwrap();
    assert_eq!(list["due"], false);
    assert_eq!(list["state"]["enabled"], true, "task add deploys the task here");
    assert!(list["last_run_at"].is_null());
    let text = sb.ok(&["task", "list"], "");
    assert!(text.lines().next().unwrap().starts_with("ID"), "{text}");
    assert!(text.contains("tag=inbox"), "{text}");
    assert!(text.contains("deployed"), "{text}");

    // disable and deploy change only this vault's state, not the document
    assert_eq!(sb.ok(&["task", "disable", "t1"], ""), "disabled t1\n");
    assert!(sb.ok(&["task", "list"], "").contains("disabled"));
    let states = sb.json(&["task", "deploy", "t1"], "");
    assert_eq!(states[0]["enabled"], true);
    assert!(sb.asked.borrow_mut().pop().unwrap().contains("(now disabled at 1-"));
    assert!(sb.json(&["doc", "get", "t1"], "")["_rev"].as_str().unwrap().starts_with("1-"));
    assert_eq!(sb.ok(&["task", "deploy", "t1"], ""), "nothing to deploy\n");
    let err = sb.fails(&["task", "deploy", "nope"], "");
    assert_eq!(err["name"], "not_found");
    let err = sb.fails(&["task", "disable", "runners/cat"], "");
    assert_eq!(err["name"], "invalid_input");

    // --no-deploy writes the task but leaves it dormant here, without asking
    sb.ok(&["task", "add", "t2", "--runner", "runners/cat", "--every", "1h", "--no-deploy", &prompt], "");
    assert!(sb.asked.borrow().is_empty());
    let check = sb.json(&["task", "check", "t2"], "");
    assert!(check["state"].is_null(), "{check}");
    assert!(sb.ok(&["task", "check", "t2"], "").contains("state:     dormant"));
    sb.ok(&["task", "rm", "t2"], "");

    // fire now: the runner echoes the prompt back, and the run records it
    let run = sb.json(&["task", "run", "t1"], "");
    assert!(run["_type"].as_str().unwrap().starts_with("doc://schemas/run?rev=1-"), "{run}");
    assert!(run["runner"].as_str().unwrap().starts_with("doc://runners/cat?rev=1-"), "{run}");
    assert_eq!(run["_actor"], "t1");
    assert!(run["task"].as_str().unwrap().starts_with("doc://t1?rev=1-"), "{run}");
    assert_eq!(run["exit_code"], 0);
    assert!(run["_rev"].as_str().unwrap().starts_with("1-"), "a receipt has one revision");
    assert!(run["_id"].as_str().unwrap().starts_with("runs/t1/") && run["_id"].as_str().unwrap().ends_with(".md"));
    assert!(run["vault"].is_string(), "{run}");
    assert!(run.get("seq").is_none(), "{run}");
    let content = run["content"].as_str().unwrap();
    assert!(content.starts_with("Triage these.\n\n---\nChanged since last run (seq"), "{content}");
    assert!(run.get("error").is_none(), "{run}");
    let runs: Page = serde_json::from_value(sb.json(&["task", "runs", "t1"], "")).unwrap();
    assert_eq!(runs.docs.len(), 1);
    assert_eq!(runs.docs[0].id, run["_id"]);
    let text = sb.ok(&["task", "runs", "t1"], "");
    assert!(text.starts_with("STARTED"), "{text}");

    // changes: a tagged doc shows, a doc written as the task itself does not
    let check = sb.json(&["task", "check", "t1"], "");
    assert_eq!(check["changes"].as_array().unwrap().len(), 0);
    sb.json(&["doc", "put"], r#"{"_id":"in1","title":"one","tags":["inbox"]}"#);
    sb.json(&["--actor", "t1", "doc", "put"], r#"{"_id":"in2","title":"self","tags":["inbox"]}"#);
    sb.json(&["doc", "put"], r#"{"_id":"other","title":"untagged"}"#);
    let check = sb.json(&["task", "check", "t1"], "");
    let changed: Vec<&str> = check["changes"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(changed, ["in1"]);
    assert_eq!(check["due"], false);
    assert_eq!(check["argv"], json!(["cat"]));
    assert!(check["prompt"].as_str().unwrap().contains("- in1  rev 1-"), "{check}");
    let text = sb.ok(&["task", "check", "t1"], "");
    assert!(text.contains("status:    "), "{text}");
    assert!(text.contains("command:   cat"), "{text}");

    // a tick well after the interval fires it, and the prompt lists the change
    let out = sb.ok(&["tick", "--now", FUTURE], "");
    assert!(out.starts_with("fired  t1 -> runs/t1/"), "{out}");
    assert!(out.trim_end().ends_with("1 fired, 0 skipped, 0 errors"), "{out}");
    let runs: Page = serde_json::from_value(sb.json(&["task", "runs", "t1"], "")).unwrap();
    assert_eq!(runs.docs.len(), 2);
    let newest = runs.docs.iter().find(|d| d.body["started_at"] == FUTURE).unwrap();
    assert!(newest.body["content"].as_str().unwrap().contains("- in1  rev 1-"), "{:?}", newest.body);
    let in1 = sb.json(&["doc", "changes"], "");
    let in1_seq =
        in1["results"].as_array().unwrap().iter().find(|d| d["_id"] == "in1").unwrap()["_seq"].as_i64().unwrap();
    assert!(sb.json(&["task", "check", "t1"], "")["cursor"].as_i64().unwrap() >= in1_seq);

    // nothing new since that run: a later tick fires nothing
    let report = sb.json(&["tick", "--now", FUTURE], "");
    assert_eq!(report["fired"].as_array().unwrap().len(), 0);

    let out = sb.ok(&["task", "rm", "t1"], "");
    assert_eq!(out, "removed t1\n");
    assert_eq!(sb.json(&["task", "list"], "").as_array().unwrap().len(), seeded_tasks, "only seeded tasks are left");
    let err = sb.fails(&["task", "rm", "runners/cat"], "");
    assert_eq!(err["name"], "invalid_input");
}

#[test]
fn the_seeded_brief_task_is_listed_dormant() {
    let sb = Sandbox::new();
    let list: Value = sb.json(&["task", "list"], "");
    let brief = list.as_array().unwrap().iter().find(|t| t["task"]["_id"] == "tasks/brief").unwrap();
    assert!(brief["state"].is_null(), "{brief}");
}

#[test]
fn task_runs_record_failures_timeouts_and_environment() {
    let sb = Sandbox::new();
    add_test_runners(&sb);
    let prompt = sb.file("p.txt", "hello");
    sb.ok(&["task", "add", "t-fail", "--runner", "runners/fail", "--every", "1h", &prompt], "");
    sb.ok(&["task", "add", "t-slow", "--runner", "runners/slow", "--every", "1h", &prompt], "");
    sb.ok(&["task", "add", "t-env", "--runner", "runners/env", "--every", "1h", &prompt], "");
    sb.ok(&["task", "add", "t-gone", "--runner", "runners/cat", "--every", "1h", &prompt], "");
    sb.ok(&["runner", "rm", "runners/cat"], "");

    let (code, out, err) = sb.run(&["--json", "task", "run", "t-fail"], "");
    assert_eq!(code, 1, "{out}{err}");
    let run: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(run["exit_code"], 1);
    assert!(run["error"].as_str().unwrap().starts_with("exit code 1"), "{run}");
    assert!(err.contains("failed"), "{err}");

    let start = std::time::Instant::now();
    let (code, out, _) = sb.run(&["--json", "task", "run", "t-slow"], "");
    assert_eq!(code, 1);
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let run: Value = serde_json::from_str(&out).unwrap();
    assert!(run["exit_code"].is_null());
    assert!(run["error"].as_str().unwrap().contains("timeout after 1s"), "{run}");

    let run = sb.json(&["task", "run", "t-env"], "");
    assert_eq!(run["exit_code"], 0);
    let expected = format!("t-env t-env {}\n", sb.db());
    assert_eq!(run["content"], expected);

    let (code, out, _) = sb.run(&["--json", "task", "run", "t-gone"], "");
    assert_eq!(code, 1);
    let run: Value = serde_json::from_str(&out).unwrap();
    assert!(run["error"].as_str().unwrap().contains("deleted"), "{run}");

    // a plain task with no `when` gets the prompt and nothing else
    let out = sb.ok(&["tick", "--now", FUTURE], "");
    assert!(out.contains("fired  t-env"), "{out}");
    let runs: Page = serde_json::from_value(sb.json(&["task", "runs", "t-env"], "")).unwrap();
    assert!(runs.docs.iter().all(|d| d.body["content"] == expected), "{:?}", runs.docs);
}

#[test]
fn tasks_run_in_workspace_or_their_cwd() {
    let sb = Sandbox::new();
    sb.ok(&["runner", "add", "runners/pwd", "--", "pwd", "-P"], "");
    let prompt = sb.file("p.txt", "hello");
    sb.ok(&["task", "add", "t-default", "--runner", "runners/pwd", "--every", "1h", &prompt], "");
    sb.ok(&["task", "add", "t-own", "--runner", "runners/pwd", "--every", "1h", "--cwd", "repos/a", &prompt], "");
    let physical = |p: PathBuf| format!("{}\n", p.canonicalize().unwrap().display());

    // the folder is made on the first run
    let run = sb.json(&["task", "run", "t-default"], "");
    assert_eq!(run["content"], physical(sb.dir.join("workspace")));
    let run = sb.json(&["task", "run", "t-own"], "");
    assert_eq!(run["content"], physical(sb.dir.join("repos/a")));

    let check = sb.json(&["task", "check", "t-own"], "");
    assert_eq!(check["cwd"], sb.dir.join("repos/a").to_string_lossy().as_ref());
    assert!(sb.ok(&["task", "check", "t-default"], "").contains("cwd:       "));
}

#[test]
fn deploys_need_confirmation_and_pin_revisions() {
    let sb = Sandbox::new();
    add_test_runners(&sb);
    let prompt = sb.file("p.txt", "first");

    // a "no" leaves the template written and dormant
    sb.answer.set(Some(false));
    let out = sb.ok(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1h", &prompt], "");
    assert!(out.contains("not deployed; t1 does not run on this vault"), "{out}");
    assert!(sb.json(&["task", "check", "t1"], "")["state"].is_null());
    let err = sb.fails(&["task", "deploy", "t1"], "");
    assert!(err["message"].as_str().unwrap().contains("not deployed"), "{err}");

    // with no terminal to ask on, a deploy fails unless --yes
    sb.answer.set(None);
    let err = sb.fails(&["task", "deploy", "t1"], "");
    assert!(err["message"].as_str().unwrap().contains("--yes"), "{err}");
    sb.asked.borrow_mut().clear();
    assert!(sb.ok(&["task", "deploy", "t1", "--yes"], "").starts_with("deployed t1 1-"));
    assert!(sb.asked.borrow().is_empty(), "--yes does not ask");
    sb.answer.set(Some(true));

    // an edit to the template does not run until the next deploy
    let edited = sb.file("p.txt", "second");
    sb.answer.set(Some(false));
    let out = sb.ok(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1h", &edited], "");
    assert!(out.contains("not deployed; this vault still runs t1 1-"), "{out}");
    sb.answer.set(Some(true));
    let out = sb.ok(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1h", "--no-deploy", &edited], "");
    assert_eq!(out.lines().count(), 1, "--no-deploy says nothing about deploys: {out}");
    sb.ok(&["task", "add", "t2", "--runner", "runners/cat", "--every", "1h", "--no-deploy", &edited], "");
    assert!(sb.ok(&["task", "list"], "").contains("deployed, changed"));
    let check = sb.json(&["task", "check", "t1"], "");
    assert_eq!(check["drift"], true);
    assert!(check["task"]["_rev"].as_str().unwrap().starts_with("1-"));
    assert!(check["head_rev"].as_str().unwrap().starts_with("2-"));
    assert_eq!(sb.json(&["task", "run", "t1"], "")["content"], "first");

    // deploy with no id: every task not deployed at its current revisions, in one confirmation
    let out = sb.ok(&["task", "deploy"], "");
    assert!(out.contains("deployed t1 2-") && out.contains("deployed t2 1-"), "{out}");
    let asked = sb.asked.borrow_mut().pop().unwrap();
    assert!(asked.contains("t1  rev 2-") && asked.contains("t2  rev 1-") && asked.contains("second"), "{asked}");
    assert_eq!(sb.ok(&["task", "deploy"], ""), "nothing to deploy\n");
    assert_eq!(sb.json(&["task", "run", "t1", "--force"], "")["content"], "second");
}

#[test]
fn run_refuses_while_a_claim_is_open_unless_forced() {
    let sb = Sandbox::new();
    add_test_runners(&sb);
    let prompt = sb.file("p.txt", "hello");
    sb.ok(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1h", &prompt], "");
    // another process holds the lease
    let conn = rusqlite::Connection::open(sb.db()).unwrap();
    conn.execute("UPDATE task_state SET lease_until = ?1 WHERE task_id = 't1'", [FUTURE]).unwrap();
    let check = sb.json(&["task", "check", "t1"], "");
    assert_eq!(check["running"], true);
    assert_eq!(check["state"]["lease_until"], FUTURE);
    let err = sb.fails(&["task", "run", "t1"], "");
    assert!(err["message"].as_str().unwrap().contains("--force"), "{err}");
    let run = sb.json(&["task", "run", "t1", "--force"], "");
    assert_eq!(run["exit_code"], 0);
}

#[test]
fn serve_protects_runner_and_run_documents() {
    // the boundary itself is covered in tests/store.rs; here: the CLI never sets it
    let sb = Sandbox::new();
    let doc = sb.json(&["doc", "put"], r#"{"_id":"runners/x","_type":"doc://schemas/runner","argv":["cat"]}"#);
    assert!(doc["_type"].as_str().unwrap().starts_with("doc://schemas/runner?rev="), "{doc}");
    let tomb = sb.json(&["doc", "delete", "runners/x"], "");
    assert_eq!(tomb["_deleted"], true);
}

/// The number of built-in documents a new vault has.
fn seeded() -> usize {
    seed::defaults().len()
}

// ---- sync -----------------------------------------------------------------

#[test]
fn pull_and_sync_between_two_vaults() {
    let a = Sandbox::new();
    let b = Sandbox::new();
    a.ok(&["init"], "");
    b.ok(&["init"], "");
    let b_db = b.db();

    let missing = a.dir.join("nope.db").to_string_lossy().into_owned();
    let err = a.fails(&["pull", &missing], "");
    assert!(err["message"].as_str().unwrap().contains("no vault at"), "{err}");
    let err = a.fails(&["pull", &a.db()], "");
    assert!(err["message"].as_str().unwrap().contains("is this vault"), "{err}");

    b.ok(&["doc", "put", "-"], r#"{"_id": "x", "title": "from b"}"#);
    let out = a.ok(&["pull", &b_db], "");
    assert!(out.starts_with("pulled 1 revision from "), "{out}");
    assert!(out.contains(&format!("{} present", seeded())), "{out}");
    assert_eq!(a.json(&["doc", "get", "x"], "")["title"], "from b");

    a.ok(&["doc", "put", "-"], r#"{"_id": "y", "title": "from a"}"#);
    let reports = a.json(&["sync", &b_db], "");
    assert_eq!(reports[0]["written"], 0);
    assert_eq!(reports[1]["written"], 1);
    assert_eq!(b.json(&["doc", "get", "y"], "")["title"], "from a");
    let out = a.ok(&["sync", &b_db], "");
    let b_path = std::fs::canonicalize(&b_db).unwrap().to_string_lossy().into_owned();
    assert!(out.contains(&format!("pulled 0 revisions from {b_path}")), "{out}");
    assert!(out.contains(&format!("pushed 0 revisions to {b_path}")), "{out}");
}

#[test]
fn conflicts_show_on_get_and_resolve() {
    let a = Sandbox::new();
    let b = Sandbox::new();
    a.ok(&["init"], "");
    b.ok(&["init"], "");
    let b_db = b.db();
    a.ok(&["doc", "put", "-"], r#"{"_id": "x", "title": "base"}"#);
    a.ok(&["sync", &b_db], "");
    a.ok(&["doc", "update", "x", "-"], r#"{"title": "from a"}"#);
    b.ok(&["doc", "update", "x", "-"], r#"{"title": "from b"}"#);
    a.ok(&["sync", &b_db], "");

    let got = a.json(&["doc", "get", "x"], "");
    assert_eq!(got["_conflicts"].as_array().unwrap().len(), 1);
    let md = a.ok(&["doc", "get", "x"], "");
    assert!(md.contains("_conflicts:"), "{md}");

    // get output is still valid input: an unchanged file is a no-op
    let file = a.file("x.md", &md);
    let same = a.json(&["doc", "put", &file], "");
    assert_eq!(same["_rev"], got["_rev"]);

    // resolve with the edited get output as the merge
    let edited = a.file("x.md", &md.replace(&format!("title: {}", got["title"].as_str().unwrap()), "title: merged"));
    let resolved = a.json(&["doc", "resolve", "x", &edited], "");
    assert_eq!(resolved["title"], "merged");
    assert!(resolved.get("_conflicts").is_none());
    a.ok(&["sync", &b_db], "");
    let on_b = b.json(&["doc", "get", "x"], "");
    assert_eq!(on_b["title"], "merged");
    assert!(on_b.get("_conflicts").is_none());
}

#[test]
fn resolve_without_a_file_keeps_the_winner() {
    let a = Sandbox::new();
    let b = Sandbox::new();
    a.ok(&["init"], "");
    b.ok(&["init"], "");
    let b_db = b.db();
    a.ok(&["doc", "put", "-"], r#"{"_id": "x", "title": "base"}"#);
    a.ok(&["sync", &b_db], "");
    a.ok(&["doc", "update", "x", "-"], r#"{"title": "from a"}"#);
    b.ok(&["doc", "update", "x", "-"], r#"{"title": "from b"}"#);
    a.ok(&["sync", &b_db], "");
    let winner = a.json(&["doc", "get", "x"], "");
    let resolved = a.json(&["doc", "resolve", "x"], "");
    assert_eq!(resolved["_rev"], winner["_rev"]);
    assert!(resolved.get("_conflicts").is_none());
}

/// Two initialized vaults with one conflicting document `x`. Returns (a, b).
fn conflicted_vaults() -> (Sandbox, Sandbox) {
    let a = Sandbox::new();
    let b = Sandbox::new();
    a.ok(&["init"], "");
    b.ok(&["init"], "");
    a.ok(&["doc", "put", "-"], r#"{"_id": "x", "title": "base"}"#);
    a.ok(&["sync", &b.db()], "");
    a.ok(&["doc", "update", "x", "-"], r#"{"title": "from a"}"#);
    b.ok(&["doc", "update", "x", "-"], r#"{"title": "from b"}"#);
    a.ok(&["sync", &b.db()], "");
    (a, b)
}

#[test]
fn doc_conflicts_lists_conflicted_ids() {
    let (a, _b) = conflicted_vaults();
    let table = a.ok(&["doc", "conflicts"], "");
    assert!(table.contains("WINNER") && table.lines().any(|l| l.starts_with("x ")), "{table}");
    let page = a.json(&["doc", "conflicts"], "");
    assert_eq!(page["docs"][0]["id"], "x");
    assert_eq!(page["docs"][0]["conflicts"].as_array().unwrap().len(), 1);
    a.ok(&["doc", "resolve", "x"], "");
    assert_eq!(a.json(&["doc", "conflicts"], "")["docs"], json!([]));
}

#[test]
fn resolve_auto_merges_with_a_runner() {
    let (a, b) = conflicted_vaults();
    a.ok(
        &[
            "runner",
            "add",
            "runners/echo",
            "--",
            "echo",
            r#"```json
{"title": "merged", "_rev": "ignored"}
```"#,
        ],
        "",
    );
    let conflicts = a.json(&["doc", "get", "x"], "")["_conflicts"].clone();

    // flags that need --auto, and --auto with a file, are usage errors
    assert_eq!(a.run(&["doc", "resolve", "x", "--dry-run"], "").0, 2);
    assert_eq!(a.run(&["doc", "resolve", "x", "f.md", "--auto"], "").0, 2);

    let dry = a.ok(&["doc", "resolve", "x", "--auto", "--runner", "runners/echo", "--dry-run"], "");
    assert!(dry.contains("title: merged") && dry.contains("_parent:") && !dry.contains("_rev"), "{dry}");
    assert_eq!(a.json(&["doc", "get", "x"], "")["_conflicts"], conflicts, "a dry run writes nothing");

    // the dry-run output is valid input for a manual resolve; here the agent's merge is applied as is
    let resolved = a.json(&["doc", "resolve", "x", "--auto", "--runner", "runners/echo"], "");
    assert_eq!(resolved["title"], "merged");
    assert!(resolved.get("_conflicts").is_none());
    assert!(resolved["_actor"].as_str().unwrap().starts_with("doc://runners/echo?rev="), "{resolved}");
    let deleted = a.json(&["doc", "get", "x", "--deleted-conflicts"], "");
    assert_eq!(deleted["_deleted_conflicts"].as_array().unwrap().len(), 1);
    assert!(a.json(&["doc", "get", "x"], "").get("_deleted_conflicts").is_none());

    a.ok(&["sync", &b.db()], "");
    assert_eq!(b.json(&["doc", "get", "x"], "")["title"], "merged");
}

#[test]
fn resolve_auto_fails_cleanly_on_a_bad_reply() {
    let (a, _b) = conflicted_vaults();
    a.ok(&["runner", "add", "runners/chatty", "--", "echo", "I could not decide."], "");
    a.ok(&["runner", "add", "runners/broken", "--", "false"], "");
    let before = a.json(&["doc", "history", "x"], "");

    let err = a.fails(&["doc", "resolve", "x", "--auto", "--runner", "runners/chatty"], "");
    assert_eq!(err["name"], "runner");
    assert!(err["message"].as_str().unwrap().contains("no JSON object"), "{err}");
    let err = a.fails(&["doc", "resolve", "x", "--auto", "--runner", "runners/broken"], "");
    assert!(err["message"].as_str().unwrap().contains("exit code 1"), "{err}");

    assert_eq!(a.json(&["doc", "history", "x"], ""), before, "nothing is written");
    assert_eq!(a.json(&["doc", "get", "x"], "")["_conflicts"].as_array().unwrap().len(), 1);
}

#[test]
fn resolve_auto_without_conflicts_prints_the_document() {
    let a = Sandbox::new();
    a.ok(&["init"], "");
    a.ok(&["doc", "put", "-"], r#"{"_id": "x", "title": "calm"}"#);
    // no runner is started, so a missing runner does not matter
    let doc = a.json(&["doc", "resolve", "x", "--auto", "--runner", "runners/missing"], "");
    assert_eq!(doc["title"], "calm");
}

#[test]
fn mcp_json_prints_a_server_entry_with_absolute_paths() {
    let sb = Sandbox::new();
    let entry: Value = serde_json::from_str(&sb.ok(&["mcp-json"], "")).unwrap();
    assert!(Path::new(entry["command"].as_str().unwrap()).is_absolute());
    assert_eq!(entry["args"], json!(["--db", sb.db(), "serve"]));
    assert!(!Path::new(&sb.db()).exists(), "mcp-json must not create the vault");

    let entry: Value = serde_json::from_str(&sb.ok(&["--actor", "claude", "mcp-json"], "")).unwrap();
    assert_eq!(entry["args"], json!(["--db", sb.db(), "--actor", "claude", "serve"]));
}

type Pages = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>;

/// Serve `pages` (path to body) over HTTP on a local port until the test
/// ends. A path with no page is a 404. Returns the base URL.
fn serve_pages(pages: Pages) -> String {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                    break;
                }
            }
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let page = pages.lock().unwrap().get(&path).cloned();
            let (status, body) = match &page {
                Some(b) => ("200 OK", b.as_str()),
                None => ("404 Not Found", ""),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    base
}

const FEED_RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Local</title><link>http://localhost/</link>
  <item><title>Two</title><guid>two</guid><description>The second item</description></item>
  <item><title>One</title><guid>one</guid><description>The first item</description></item>
</channel></rss>"#;

#[test]
fn feeds_pull_only_new_items() {
    let sb = Sandbox::new();
    sb.ok(&["init"], "");
    let pages: Pages = Default::default();
    pages.lock().unwrap().insert("/rss".into(), FEED_RSS.into());
    pages.lock().unwrap().insert("/page".into(), "<p>Version one</p>".into());
    let base = serve_pages(pages.clone());
    let rss_url = format!("{base}/rss");

    // The default id comes from the origin.
    let doc = sb.json(&["feed", "add", &rss_url, "--title", "Local"], "");
    let rss_id = format!("feeds/{}.md", dreams::feed::origin_slug(&base));
    assert_eq!(doc["_id"], rss_id.as_str());
    assert!(sb.ok(&["feed", "add", &rss_url, "--title", "Local"], "").starts_with("unchanged "));
    // A second feed on the same origin needs its own id.
    let err = sb.fails(&["feed", "add", &format!("{base}/page")], "");
    assert_eq!(err["name"], "invalid_input");
    assert!(err["message"].as_str().unwrap().contains("--id"), "{err}");
    sb.ok(&["feed", "add", &format!("{base}/page"), "--kind", "html", "--id", "feeds/page.md"], "");
    let list = sb.ok(&["feed", "list"], "");
    assert!(list.contains(&rss_id) && list.contains("feeds/page.md"), "{list}");

    let report = sb.json(&["feed", "pull"], "");
    let items = report["items"].as_array().unwrap();
    assert_eq!(items.len(), FEED_RSS.matches("<item>").count() + 1, "{report}");
    assert_eq!(report["errors"], json!([]));
    let one = items.iter().find(|i| i["title"] == "One").unwrap();
    assert_eq!(one["description"], "The first item");
    let doc = sb.json(&["doc", "get", one["href"].as_str().unwrap()], "");
    assert!(doc["_type"].as_str().unwrap().starts_with("doc://schemas/feed-item?rev="), "{doc}");
    assert_eq!(doc["feed"], format!("doc://{rss_id}"));
    assert!(doc["_id"].as_str().unwrap().starts_with(rss_id.trim_end_matches(".md")));

    assert_eq!(sb.json(&["feed", "pull"], "")["items"], json!([]), "a second pull sees nothing new");
    assert!(sb.ok(&["feed", "pull"], "").contains("no new items"));

    // A page whose text changed is new again.
    pages.lock().unwrap().insert("/page".into(), "<p>Version two</p>".into());
    let report = sb.json(&["feed", "pull", "feeds/page.md"], "");
    assert_eq!(report["items"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["items"][0]["description"], "Version two");

    // One broken feed fails the command, but the others are still pulled.
    pages.lock().unwrap().insert("/page".into(), "<p>Version three</p>".into());
    sb.ok(&["feed", "add", &format!("{base}/missing"), "--id", "feeds/missing.md"], "");
    let (code, out, _) = sb.run(&["--json", "feed", "pull"], "");
    assert_eq!(code, 1);
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["errors"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["errors"][0]["feed"], "feeds/missing.md");
    assert_eq!(report["items"].as_array().unwrap().len(), 1, "{report}");

    sb.ok(&["feed", "rm", "feeds/missing.md"], "");
    assert_eq!(sb.json(&["feed", "pull"], "")["errors"], json!([]));
    sb.ok(&["doc", "put", "-"], r#"{"_id": "note", "title": "not a feed"}"#);
    assert_eq!(sb.fails(&["feed", "rm", "note"], "")["name"], "invalid_input");
    assert_eq!(sb.fails(&["feed", "pull", "note"], "")["name"], "invalid_input");
}
