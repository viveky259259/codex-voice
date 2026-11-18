use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn reader(text: &'static str) -> impl Fn(&Path) -> Option<String> {
    move |_| Some(text.to_string())
}

#[test]
fn edit_builds_a_unified_diff_from_old_and_new_strings() {
    let changes = file_changes(
        "Edit",
        &json!({"file_path": "/repo/src/lib.rs", "old_string": "b\n", "new_string": "B\n"}),
        Path::new("/repo"),
        reader("a\nb\nc\n"),
    );
    assert_eq!(
        changes,
        vec![FileUpdateChange {
            path: "/repo/src/lib.rs".into(),
            kind: PatchChangeKind::Update { move_path: None },
            diff: "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n".into(),
        }]
    );
}

#[test]
fn write_to_a_new_file_is_an_add_with_its_content() {
    let changes = file_changes(
        "Write",
        &json!({"file_path": "notes.md", "content": "hello\n"}),
        Path::new("/repo"),
        |_| None,
    );
    assert_eq!(
        changes,
        vec![FileUpdateChange {
            path: "/repo/notes.md".into(),
            kind: PatchChangeKind::Add,
            diff: "hello\n".into(),
        }]
    );
}

#[test]
fn multi_edit_applies_edits_in_order() {
    let changes = file_changes(
        "MultiEdit",
        &json!({"file_path": "/f", "edits": [
            {"old_string": "x", "new_string": "y", "replace_all": true},
            {"old_string": "y1", "new_string": "z"},
        ]}),
        Path::new("/"),
        reader("x1\nx2\n"),
    );
    assert_eq!(changes[0].diff, "@@ -1,2 +1,2 @@\n-x1\n-x2\n+z\n+y2\n");
}

#[test]
fn unmatched_old_text_lists_the_path_without_a_diff() {
    let changes = file_changes(
        "Edit",
        &json!({"file_path": "/f", "old_string": "missing", "new_string": "x"}),
        Path::new("/"),
        reader("text\n"),
    );
    assert_eq!(
        changes,
        vec![FileUpdateChange {
            path: "/f".into(),
            kind: PatchChangeKind::Update { move_path: None },
            diff: String::new(),
        }]
    );
}
