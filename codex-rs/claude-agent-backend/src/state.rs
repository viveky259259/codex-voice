//! What a resumed session needs that `claude --resume` does not restore: the
//! model and effort in use, per session id, in `<state_dir>/sessions.json`.

use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

/// Sessions kept in the file; the least recently used are dropped beyond this.
const MAX_SESSIONS: usize = 200;
const FILE_NAME: &str = "sessions.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedSettings {
    /// The `--model` value; `None` is Claude Code's default model.
    pub model: Option<String>,
    /// The `--effort` level; `None` is the model's default effort.
    pub effort: Option<String>,
    /// Unix seconds of the last save.
    pub updated_at: i64,
}

pub(crate) struct SessionStore {
    path: Option<PathBuf>,
}

impl SessionStore {
    /// `None` remembers nothing (each resume starts from the configured model).
    pub(crate) fn new(dir: Option<&Path>) -> Self {
        Self {
            path: dir.map(|dir| dir.join(FILE_NAME)),
        }
    }

    fn load(&self) -> BTreeMap<String, SavedSettings> {
        self.path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub(crate) fn get(&self, session_id: &str) -> Option<SavedSettings> {
        self.load().remove(session_id)
    }

    /// Best effort: a failed write is logged, never fatal.
    pub(crate) fn put(&self, session_id: &str, settings: SavedSettings) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let mut sessions = self.load();
        if sessions.get(session_id) == Some(&settings) {
            return;
        }
        sessions.insert(session_id.to_string(), settings);
        while sessions.len() > MAX_SESSIONS {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, saved)| saved.updated_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            sessions.remove(&oldest);
        }
        if let Err(err) = write(path, &sessions) {
            tracing::warn!(%err, path = %path.display(), "could not save Claude Code session settings");
        }
    }
}

fn write(path: &Path, sessions: &BTreeMap<String, SavedSettings>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(sessions).map_err(std::io::Error::other)?;
    let partial = path.with_extension("json.tmp");
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn settings_round_trip_and_the_oldest_are_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(Some(dir.path()));
        assert_eq!(store.get("a"), None);
        let saved = SavedSettings {
            model: Some("claude-haiku-5-5".into()),
            effort: Some("high".into()),
            updated_at: 10,
        };
        store.put("a", saved.clone());
        assert_eq!(store.get("a"), Some(saved));
        for index in 0..MAX_SESSIONS {
            store.put(
                &format!("s{index}"),
                SavedSettings {
                    updated_at: 100 + index as i64,
                    ..Default::default()
                },
            );
        }
        assert_eq!(store.get("a"), None, "the least recently used goes first");
        assert!(store.get("s0").is_some());
        assert_eq!(SessionStore::new(None).get("s0"), None);
    }
}
