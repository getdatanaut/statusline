//! The session's name as other sessions address it (`SendMessage` `to`).
//!
//! Claude Code does not put it in the status line payload; it lives in
//! `<config>/sessions/<pid>.json` as `name`, next to the `sessionId` the
//! payload does carry, `<config>` being `CLAUDE_CONFIG_DIR` or `~/.claude`.
use std::path::{Path, PathBuf};

/// The `name` of the session file whose `sessionId` is `session_id`.
pub fn session_name(home: &Path, session_id: &str) -> Option<String> {
    let dirs: Vec<PathBuf> = sl_core::paths::claude_config_dirs(home)
        .into_iter()
        .map(|dir| dir.join("sessions"))
        .collect();
    name_in(&dirs, session_id)
}

/// The name in the newest file under `dirs` for `session_id`: a file left
/// behind by an earlier process of a resumed session is older than the
/// running one's.
fn name_in(dirs: &[PathBuf], session_id: &str) -> Option<String> {
    dirs.iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            if value.get("sessionId")?.as_str()? != session_id {
                return None;
            }
            let name = value.get("name")?.as_str()?.trim();
            let modified = e.metadata().and_then(|m| m.modified()).ok();
            (!name.is_empty()).then(|| (modified, name.to_string()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_name_by_session_id() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1.json"), r#"{"sessionId":"a","name":"zed-c0"}"#).unwrap();
        std::fs::write(
            dir.join("2.json"),
            r#"{"sessionId":"b","name":"statusline-2a"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("3.key"), "x").unwrap();
        assert_eq!(
            session_name(home.path(), "b").as_deref(),
            Some("statusline-2a")
        );
        assert_eq!(session_name(home.path(), "c"), None);
    }

    #[test]
    fn the_newest_file_for_a_session_wins() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("1.json");
        std::fs::write(&old, r#"{"sessionId":"a","name":"before-resume"}"#).unwrap();
        let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(earlier)
            .unwrap();
        std::fs::write(
            dir.path().join("2.json"),
            r#"{"sessionId":"a","name":"after-resume"}"#,
        )
        .unwrap();
        assert_eq!(
            name_in(&[dir.path().to_path_buf()], "a").as_deref(),
            Some("after-resume")
        );
    }
}
