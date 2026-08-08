//! The thinking setting new sessions start with, when the user picked it in the
//! app rather than declaring it in config.
//!
//! Same split as the theme: this file holds what you last chose, and
//! `always_thinking` in `init.lua` still wins for anyone who would rather say
//! it once and have it stay said.

use std::fs;

use tracing::warn;

use crate::StateDir;
use crate::sessions::StoredThinking;

const THINKING_FILE: &str = "thinking";

pub fn persist_default(dir: &StateDir, thinking: StoredThinking) {
    let json = match serde_json::to_vec(&thinking) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "failed to serialize default thinking");
            return;
        }
    };
    if let Err(e) = crate::atomic_write(dir.path().join(THINKING_FILE).as_path(), &json) {
        warn!(error = %e, "failed to persist default thinking");
    }
}

/// `None` when never set, or when the file is unreadable: a corrupt default is
/// not worth refusing to start over.
pub fn read_default(dir: &StateDir) -> Option<StoredThinking> {
    let raw = fs::read_to_string(dir.path().join(THINKING_FILE)).ok()?;
    match serde_json::from_str(&raw) {
        Ok(t) => Some(t),
        Err(e) => {
            warn!(error = %e, "failed to parse default thinking, ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::Effort;
    use tempfile::TempDir;
    use test_case::test_case;

    const CORRUPT: &str = "{not json";

    #[test_case(StoredThinking::Off                            ; "off")]
    #[test_case(StoredThinking::Adaptive                       ; "adaptive")]
    #[test_case(StoredThinking::Effort { level: Effort::Max }  ; "effort")]
    #[test_case(StoredThinking::Budget { tokens: 8192 }        ; "budget")]
    fn default_round_trips(thinking: StoredThinking) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert!(read_default(&dir).is_none());
        persist_default(&dir, thinking);
        assert_eq!(read_default(&dir), Some(thinking));
    }

    #[test]
    fn corrupt_default_is_ignored_rather_than_fatal() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        fs::write(dir.path().join(THINKING_FILE), CORRUPT).unwrap();

        assert!(read_default(&dir).is_none());
    }
}
