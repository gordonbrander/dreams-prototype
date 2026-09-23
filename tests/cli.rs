use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use subconscious::{Changes, History, Page, SchemaList, cli};

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("subconscious-cli-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Sandbox { dir }
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
        let mut argv = vec!["subconscious".to_string(), "--db".to_string(), self.db()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let mut input = stdin.as_bytes();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = cli::run(argv, &mut input, &mut out, &mut err);
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
$id: note/v1
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
_type: note/v1
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
    let out = sb.ok(&["schema", "register", &schema], "");
    assert_eq!(out, "registered note/v1: Note\n");

    let note = sb.file("note.md", NOTE_MD);
    let out = sb.ok(&["doc", "put", &note], "");
    let fm = frontmatter(&out);
    assert_eq!(fm["_id"], "n1");
    assert!(fm["_rev"].as_str().unwrap().starts_with("1-"));
    assert_eq!(fm["_type"], "note/v1");
    assert_eq!(fm["tags"], json!(["demo", "blue"]));
    assert!(fm.get("_parent").is_none());
    assert!(out.ends_with("---\nfirst draft\n"), "{out}");
}

#[test]
fn markdown_round_trip_unchanged_then_edited() {
    let sb = Sandbox::new();
    let schema = sb.file("note.yaml", SCHEMA_YAML);
    sb.ok(&["schema", "register", &schema], "");
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
    assert_eq!(changes.results.len(), 1);

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

    // get --rev reads history, but only this document's
    let old = sb.json(&["doc", "get", "a", "--rev", &rev1], "");
    assert_eq!(old["title"], "one");
    let b = sb.json(&["doc", "get", "b"], "");
    let err = sb.fails(&["doc", "get", "a", "--rev", b["_rev"].as_str().unwrap()], "");
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
    assert_eq!(page.docs.len(), 2);
    let page: Page = serde_json::from_value(sb.json(&["doc", "list", "--limit", "1"], "")).unwrap();
    assert!(page.next.is_some());
    let text = sb.ok(&["doc", "list", "--limit", "1"], "");
    assert!(text.trim_end().ends_with(&format!("next: {}", page.next.unwrap())));

    let page: Page = serde_json::from_value(sb.json(&["doc", "search", "searchable"], "")).unwrap();
    assert_eq!(page.docs[0].id, "b");

    let text = sb.ok(&["doc", "changes"], "");
    assert!(text.starts_with("SEQ"));
    assert!(text.trim_end().ends_with("last_seq: 2"));

    let schema = sb.file("note.yaml", SCHEMA_YAML);
    sb.ok(&["schema", "register", &schema], "");
    let list: SchemaList = serde_json::from_value(sb.json(&["schema", "list"], "")).unwrap();
    assert_eq!(list.schemas[0].id, "note/v1");
    let text = sb.ok(&["schema", "list"], "");
    let row: Vec<&str> = text.lines().nth(1).unwrap().split_whitespace().collect();
    assert_eq!(row, ["note/v1", "Note", "A", "note"], "{text}");
    let schema: Value = sb.json(&["schema", "get", "note/v1"], "");
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
    let del = sb.json(&["doc", "put"], r#"{"_id":"gone.md","title":"Gone"}"#);
    sb.json(&["doc", "delete", "gone.md", "--parent", del["_rev"].as_str().unwrap()], "");

    let out_dir = sb.dir.join("export");
    let out = sb.ok(&["export", out_dir.to_str().unwrap()], "");
    assert!(out.trim_end().ends_with("3 exported, 0 errors"), "{out}");
    assert!(out_dir.join("a.md").exists());
    assert!(out_dir.join("notes/2026/b.md").exists());
    assert!(out_dir.join("plain").exists());
    assert!(!out_dir.join("gone.md").exists());
    let text = std::fs::read_to_string(out_dir.join("a.md")).unwrap();
    assert!(text.starts_with("---\n_id: a.md\n_rev: 1-"), "{text}");
    assert!(text.ends_with("---\nalpha body\n"), "{text}");

    // exported and unchanged: every .md file is a no-op; `plain` is not a .md file and is skipped
    let report = sb.json(&["import", out_dir.to_str().unwrap()], "");
    let statuses: Vec<&str> = report["results"].as_array().unwrap().iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["unchanged", "unchanged"]);
    assert_eq!(report["errors"], 0);
    let changes: Changes = serde_json::from_value(sb.json(&["doc", "changes"], "")).unwrap();
    assert_eq!(changes.results.len(), 5);

    // edit one, add one, and drop a copied file whose frontmatter names another doc
    std::fs::write(out_dir.join("a.md"), text.replace("alpha body", "alpha edited")).unwrap();
    std::fs::write(out_dir.join("notes/new.md"), "---\ntitle: New\n---\nfresh\n").unwrap();
    std::fs::copy(out_dir.join("notes/2026/b.md"), out_dir.join("copy.md")).unwrap();
    let out = sb.ok(&["import", out_dir.to_str().unwrap()], "");
    assert!(out.contains("updated    a.md 2-"), "{out}");
    assert!(out.contains("created    notes/new.md 1-"), "{out}");
    assert!(out.contains("created    copy.md 1-"), "{out}");
    assert!(out.contains("unchanged  notes/2026/b.md"), "{out}");
    assert!(out.trim_end().ends_with("2 created, 1 updated, 1 unchanged, 0 errors"), "{out}");
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
        &["runner", "add", "runners/env", "--", "sh", "-c", "cat >/dev/null; echo $SUBCONSCIOUS_TASK $SUBCONSCIOUS_ACTOR $SUBCONSCIOUS_DB"],
        "",
    );
}

#[test]
fn runners_are_documents_seeded_once() {
    let sb = Sandbox::new();
    let out = sb.ok(&["init"], "");
    assert!(out.contains("seeded runners/claude"), "{out}");
    let text = sb.ok(&["runner", "list"], "");
    let ids: Vec<&str> = text.lines().skip(1).map(|l| l.split_whitespace().next().unwrap()).collect();
    assert_eq!(ids, ["runners/claude", "runners/codex", "runners/pi"]);
    add_test_runners(&sb);
    let out = sb.ok(&["runner", "add", "runners/cat", "--", "cat"], "");
    assert!(out.starts_with("unchanged runners/cat"), "{out}");
    sb.ok(&["runner", "rm", "runners/pi"], "");
    let text = sb.ok(&["runner", "list"], "");
    assert!(!text.contains("runners/pi"), "{text}");
    assert_eq!(text.lines().count(), 1 + 6, "{text}");
    let doc = sb.json(&["doc", "get", "runners/slow"], "");
    assert_eq!(doc["_type"], "runner/v1");
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

    let err = sb.fails(&["task", "add", "t1", "--runner", "runners/nope", "--every", "1h", &prompt], "");
    assert_eq!(err["name"], "not_found");
    let err = sb.fails(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1x", &prompt], "");
    assert_eq!(err["name"], "invalid_input");

    let out = sb.ok(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1h", "--tag", "inbox", &prompt], "");
    assert!(out.starts_with("created t1 1-"), "{out}");
    let task = sb.json(&["doc", "get", "t1"], "");
    assert_eq!(task["_type"], "task/v1");
    assert_eq!(task["when"], json!({"tag": "inbox"}));
    assert_eq!(task["prompt"], "Triage these.\n");

    let list: Value = sb.json(&["task", "list"], "");
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["task"]["_id"], "t1");
    assert_eq!(list[0]["due"], false);
    assert!(list[0]["last_run_at"].is_null());
    let text = sb.ok(&["task", "list"], "");
    assert!(text.lines().next().unwrap().starts_with("ID"), "{text}");
    assert!(text.contains("tag=inbox"), "{text}");

    // fire now: the runner echoes the prompt back, and the run records it
    let run = sb.json(&["task", "run", "t1"], "");
    assert_eq!(run["_type"], "run/v1");
    assert_eq!(run["_actor"], "t1");
    assert_eq!(run["task"], "t1");
    assert_eq!(run["exit_code"], 0);
    assert!(run["_rev"].as_str().unwrap().starts_with("2-"));
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
    let in1_seq = in1["results"].as_array().unwrap().iter().find(|d| d["_id"] == "in1").unwrap()["_seq"].as_i64().unwrap();
    assert!(newest.body["seq"].as_i64().unwrap() >= in1_seq);

    // nothing new since that run: a later tick fires nothing
    let report = sb.json(&["tick", "--now", FUTURE], "");
    assert_eq!(report["fired"].as_array().unwrap().len(), 0);

    let out = sb.ok(&["task", "rm", "t1"], "");
    assert_eq!(out, "removed t1\n");
    assert!(sb.json(&["task", "list"], "").as_array().unwrap().is_empty());
    let err = sb.fails(&["task", "rm", "runners/cat"], "");
    assert_eq!(err["name"], "invalid_input");
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
fn run_refuses_while_a_claim_is_open_unless_forced() {
    let sb = Sandbox::new();
    add_test_runners(&sb);
    let prompt = sb.file("p.txt", "hello");
    sb.ok(&["task", "add", "t1", "--runner", "runners/cat", "--every", "1h", &prompt], "");
    // the CLI is trusted: it can write a claim by hand
    let claim = format!(
        r#"{{"_id":"runs/t1/manual","_type":"run/v1","task":"t1","runner":"runners/cat","started_at":"{FUTURE}","seq":1,"tags":["t1"]}}"#
    );
    sb.json(&["doc", "put"], &claim);
    let check = sb.json(&["task", "check", "t1"], "");
    assert_eq!(check["running"], true);
    assert_eq!(check["last_run"], "runs/t1/manual");
    let err = sb.fails(&["task", "run", "t1"], "");
    assert!(err["message"].as_str().unwrap().contains("--force"), "{err}");
    let run = sb.json(&["task", "run", "t1", "--force"], "");
    assert_eq!(run["exit_code"], 0);
}

#[test]
fn serve_protects_runner_and_run_documents() {
    // the boundary itself is covered in tests/store.rs; here: the CLI never sets it
    let sb = Sandbox::new();
    let doc = sb.json(&["doc", "put"], r#"{"_id":"runners/x","_type":"runner/v1","argv":["cat"]}"#);
    assert_eq!(doc["_type"], "runner/v1");
    let tomb = sb.json(&["doc", "delete", "runners/x"], "");
    assert_eq!(tomb["_deleted"], true);
}
