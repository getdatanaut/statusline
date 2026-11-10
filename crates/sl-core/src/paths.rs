//! Where this workspace keeps its on-disk state.
//!
//! One resolution of the cache base directory, shared by everything that
//! writes under it: the status line's per-session counters (`sessions/`),
//! `sl-usage`'s parsed-record cache (`usage/`) and the rate-limit capture the
//! status-line hook writes for the tray (`limits.json`). Three private copies
//! of "`$XDG_CACHE_HOME`, else `~/.cache`" would be three chances for them to
//! drift apart and quietly stop finding each other's files -- the same
//! divergence risk that already cost this workspace a duplicated
//! `month_of_date`.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The file the status-line hook writes its verbatim `rate_limits` capture to,
/// under [`cache_dir`]. See [`crate::types::LimitsCapture`] for its contents.
pub const LIMITS_FILE: &str = "limits.json";

/// The directory, under [`cache_dir`], the status line writes each session's
/// context use to for the tray: `context/<session id>.json`. See
/// [`crate::types::ContextCapture`].
pub const CONTEXT_DIR: &str = "context";

/// Where `session_id`'s context capture lives under `cache_dir`; `None` for
/// an id that could not be a file name.
pub fn context_file(cache_dir: &std::path::Path, session_id: &str) -> Option<PathBuf> {
    let safe = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    safe.then(|| {
        cache_dir
            .join(CONTEXT_DIR)
            .join(format!("{session_id}.json"))
    })
}

/// Claude Code's config directories: each entry of `CLAUDE_CONFIG_DIR`
/// (comma-separated) when it is set, else `<home>/.claude`. A set
/// `CLAUDE_CONFIG_DIR` replaces the default rather than adding to it, as in
/// `sl-usage`'s `claude_paths` (which also looks in `$XDG_CONFIG_HOME/claude`
/// for transcripts; session records are only ever in these).
pub fn claude_config_dirs(home: &Path) -> Vec<PathBuf> {
    config_dirs_from(std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(), home)
}

fn config_dirs_from(env: Option<&str>, home: &Path) -> Vec<PathBuf> {
    let set: Vec<PathBuf> = env
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect();
    if set.is_empty() {
        vec![home.join(".claude")]
    } else {
        set
    }
}

/// `$XDG_CACHE_HOME/statusline`, else `~/.cache/statusline`.
///
/// Callers append their own subdirectory (`sessions`, `usage`) or file name;
/// nothing writes directly into this directory except [`limits_path`].
pub fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME").map_or_else(
        |_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".cache")
        },
        PathBuf::from,
    );
    base.join("statusline")
}

/// `<cache dir>/limits.json`: written by the status-line hook every turn, read
/// by the tray on every refresh.
pub fn limits_path() -> PathBuf {
    cache_dir().join(LIMITS_FILE)
}

/// Write `value` to `path` as JSON, atomically: a temp file beside it
/// (`<path>.tmp.<pid>`), then a rename over the old one, so a reader on its
/// own schedule never catches a half-written file. Creates the directory.
/// Best effort, as everything this workspace keeps on disk is: `false` when it
/// could not be written, which callers may ignore.
pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> bool {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = PathBuf::from(format!("{}.tmp.{}", path.display(), std::process::id()));
    let Ok(json) = serde_json::to_string(value) else {
        return false;
    };
    std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, path).is_ok()
}

/// `path` read as JSON into `T`; `None` when it is missing or does not parse.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_config_dir_entries_replace_the_default() {
        let home = std::path::Path::new("/h");
        assert_eq!(
            super::config_dirs_from(None, home),
            vec![home.join(".claude")]
        );
        assert_eq!(
            super::config_dirs_from(Some(" /a , /b,"), home),
            vec![
                std::path::PathBuf::from("/a"),
                std::path::PathBuf::from("/b")
            ]
        );
        assert_eq!(
            super::config_dirs_from(Some(""), home),
            vec![home.join(".claude")]
        );
    }

    use super::*;

    #[test]
    fn json_is_written_whole_and_read_back() {
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let path = dir.join("nested").join("value.json");
        assert!(
            write_json_atomic(&path, &vec![1, 2, 3]),
            "creates the directory and writes"
        );
        assert_eq!(read_json::<Vec<i32>>(&path), Some(vec![1, 2, 3]));
        assert!(write_json_atomic(&path, &vec![4]), "overwrites");
        assert_eq!(read_json::<Vec<i32>>(&path), Some(vec![4]));
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "no temp file is left behind");
        assert_eq!(read_json::<Vec<i32>>(&dir.join("missing.json")), None);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(
            read_json::<Vec<i32>>(&path),
            None,
            "a broken file reads as nothing"
        );
    }

    #[test]
    fn limits_lives_directly_under_the_cache_dir() {
        let path = limits_path();
        assert_eq!(path.file_name().unwrap(), LIMITS_FILE);
        assert_eq!(path.parent().unwrap(), cache_dir());
        assert!(
            cache_dir().ends_with("statusline"),
            "cache dir: {}",
            cache_dir().display()
        );
    }
}
