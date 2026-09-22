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
