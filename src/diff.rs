//! Line diffs and merges of document text: diffs for a person or an agent to
//! read, and a three-way merge of `content` that marks the lines both sides
//! changed. An agent resolves the marks with edits, `old` → `new`.

use diffy::{DiffOptions, MergeOptions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::doc::PutInput;
use crate::error::StoreError;
use crate::markdown;

/// The shortest conflict marker, as in git.
const MARKER_LEN: usize = 7;

/// A body as Markdown with frontmatter, without the revision metadata, so a
/// diff shows only what a person edits.
pub fn body_text(type_id: Option<&str>, body: &Map<String, Value>) -> String {
    markdown::render_input(&PutInput {
        id: None,
        parent: None,
        type_id: type_id.map(str::to_string),
        body: body.clone(),
    })
}

/// A unified diff from `old` to `new`. Empty when they are the same.
pub fn unified(old: &str, new: &str, old_name: &str, new_name: &str) -> String {
    let patch = DiffOptions::new()
        .set_context_len(3)
        .set_original_filename(old_name.to_string())
        .set_modified_filename(new_name.to_string())
        .create_patch(old, new);
    if patch.hunks().is_empty() { String::new() } else { patch.to_string() }
}

/// Merge the changes of `ours` and `theirs` to `ancestor`, line by line.
/// `Err` holds the merge with conflict markers around the lines that both
/// sides changed. The markers are longer than any marker-like line in the
/// inputs, so they never match the text itself.
pub fn merge_content(ancestor: &str, ours: &str, theirs: &str) -> Result<String, String> {
    let longest = [ancestor, ours, theirs].iter().flat_map(|t| t.lines()).map(marker_run).max().unwrap_or(0);
    MergeOptions::new().set_conflict_marker_length(MARKER_LEN.max(longest + 1)).merge(ancestor, ours, theirs)
}

/// The length of the run of one marker character that starts `line`.
fn marker_run(line: &str) -> usize {
    match line.chars().next() {
        Some(c @ ('<' | '=' | '|' | '>')) => line.chars().take_while(|&x| x == c).count(),
        _ => 0,
    }
}

/// Whether `text` has a conflict marker from `merge_content`.
pub fn has_markers(text: &str) -> bool {
    text.lines().any(|line| {
        let is = |c: char, label: &str| {
            line.strip_suffix(label).is_some_and(|run| run.len() >= MARKER_LEN && run.chars().all(|x| x == c))
        };
        is('<', " ours") || is('>', " theirs")
    })
}

/// One replacement, as in a file-edit tool: `old` must occur exactly once.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Edit {
    /// The text to replace, copied exactly. It must occur once.
    pub old: String,
    /// The text that replaces it.
    pub new: String,
}

/// Apply `edits` in order. Each `old` must occur exactly once in the text
/// at that step.
pub fn apply_edits(text: &str, edits: &[Edit]) -> Result<String, StoreError> {
    let mut text = text.to_string();
    for (i, edit) in edits.iter().enumerate() {
        let n = i + 1;
        match text.matches(edit.old.as_str()).count() {
            _ if edit.old.is_empty() => return Err(StoreError::invalid(format!("edit {n}: old text is empty"))),
            0 => return Err(StoreError::invalid(format!("edit {n}: old text not found"))),
            1 => text = text.replacen(&edit.old, &edit.new, 1),
            k => return Err(StoreError::invalid(format!("edit {n}: old text occurs {k} times; include more of it"))),
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit { old: old.into(), new: new.into() }
    }

    #[test]
    fn unified_is_empty_for_the_same_text() {
        assert_eq!(unified("a\n", "a\n", "x", "y"), "");
        let d = unified("a\n", "b\n", "x", "y");
        assert!(d.starts_with("--- x\n+++ y\n") && d.contains("-a\n+b\n"), "{d}");
    }

    #[test]
    fn merge_content_settles_different_lines_and_marks_the_same_place() {
        let base = "one\ntwo\nthree\n";
        assert_eq!(merge_content(base, "ONE\ntwo\nthree\n", "one\ntwo\nTHREE\n").unwrap(), "ONE\ntwo\nTHREE\n");
        let marked = merge_content(base, "one\ntwo\nthree\nfour a\n", "one\ntwo\nthree\nfour b\n").unwrap_err();
        assert!(marked.contains("<<<<<<< ours\nfour a\n") && marked.contains("four b\n>>>>>>> theirs\n"), "{marked}");
        assert!(has_markers(&marked));
    }

    #[test]
    fn markers_are_longer_than_marker_like_text() {
        let base = "<<<<<<<< a note\n";
        let marked = merge_content(base, "<<<<<<<< a note\nx\n", "<<<<<<<< a note\ny\n").unwrap_err();
        assert!(marked.contains("\n<<<<<<<<< ours\n"), "{marked}");
        assert!(!has_markers(base), "a line that only looks like a marker is text");
    }

    #[test]
    fn edits_apply_in_order_like_a_file_edit_tool() {
        let text = "a\n<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\nb\n";
        let block = "<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\n";
        assert_eq!(apply_edits(text, &[edit(block, "x\ny\n")]).unwrap(), "a\nx\ny\nb\n");
        assert_eq!(apply_edits(text, &[edit(block, "x\n"), edit("x\nb", "x\nB")]).unwrap(), "a\nx\nB\n");

        let err = |edits: &[Edit]| apply_edits(text, edits).unwrap_err().to_string();
        assert!(err(&[edit("zzz", "")]).contains("edit 1: old text not found"));
        assert!(err(&[edit(block, "b\n"), edit("b\n", "")]).contains("edit 2: old text occurs 2 times"));
        assert!(err(&[edit("", "q")]).contains("empty"));
    }
}
