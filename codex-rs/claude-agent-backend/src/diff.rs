//! File changes for Claude Code's edit tools, computed before the tool runs.

use codex_app_server_protocol::FileUpdateChange;
use codex_app_server_protocol::PatchChangeKind;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

/// The tools shown as `FileChange` items and `item/fileChange/requestApproval`.
pub fn is_edit_tool(name: &str) -> bool {
    matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
}

/// One edit as Claude Code applies it: replace `old` with `new`, once or everywhere.
struct Replacement<'a> {
    old: &'a str,
    new: &'a str,
    all: bool,
}

/// The changes an edit tool will make. `read` returns a file's current text, if
/// it exists. When the old text cannot be found, the path is listed with an
/// empty diff rather than a guessed one.
pub fn file_changes(
    tool: &str,
    input: &Value,
    cwd: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Vec<FileUpdateChange> {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let Some(raw_path) = field("file_path").or_else(|| field("notebook_path")) else {
        return Vec::new();
    };
    let path = absolute(cwd, raw_path);
    let display = path.to_string_lossy().into_owned();
    let current = read(&path);
    let replacements: Vec<Replacement<'_>> = match tool {
        "Edit" => vec![Replacement {
            old: field("old_string").unwrap_or_default(),
            new: field("new_string").unwrap_or_default(),
            all: input.get("replace_all").and_then(Value::as_bool) == Some(true),
        }],
        "MultiEdit" => input
            .get("edits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|edit| Replacement {
                old: edit
                    .get("old_string")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                new: edit
                    .get("new_string")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                all: edit.get("replace_all").and_then(Value::as_bool) == Some(true),
            })
            .collect(),
        "Write" => {
            let content = field("content").unwrap_or_default();
            return vec![match current {
                Some(old) => update(display, &old, content),
                None => FileUpdateChange {
                    path: display,
                    kind: PatchChangeKind::Add,
                    diff: content.to_string(),
                },
            }];
        }
        _ => {
            return vec![FileUpdateChange {
                path: display,
                kind: PatchChangeKind::Update { move_path: None },
                diff: String::new(),
            }];
        }
    };
    // An empty `old_string` on a missing file creates it.
    if current.is_none()
        && let [only] = replacements.as_slice()
        && only.old.is_empty()
    {
        return vec![FileUpdateChange {
            path: display,
            kind: PatchChangeKind::Add,
            diff: only.new.to_string(),
        }];
    }
    let Some(old) = current else {
        return vec![listed_only(display)];
    };
    let mut new = old.clone();
    for replacement in &replacements {
        if replacement.old.is_empty() || !new.contains(replacement.old) {
            return vec![listed_only(display)];
        }
        new = if replacement.all {
            new.replace(replacement.old, replacement.new)
        } else {
            new.replacen(replacement.old, replacement.new, 1)
        };
    }
    vec![update(display, &old, &new)]
}

fn listed_only(path: String) -> FileUpdateChange {
    FileUpdateChange {
        path,
        kind: PatchChangeKind::Update { move_path: None },
        diff: String::new(),
    }
}

fn update(path: String, old: &str, new: &str) -> FileUpdateChange {
    FileUpdateChange {
        path,
        kind: PatchChangeKind::Update { move_path: None },
        diff: unified_diff(old, new),
    }
}

/// Hunks only (`@@ -a,b +c,d @@`), like Codex's own `apply_patch` items.
pub fn unified_diff(old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string()
}

fn absolute(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
#[path = "diff_tests.rs"]
mod tests;
