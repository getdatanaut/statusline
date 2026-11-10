use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// The statusline payload reports the model's full context window (1M for
/// `[1m]` models) and knows nothing about compaction. `autoCompactWindow` in
/// settings.json caps the usable window before auto-compact kicks in, so the
/// effective limit shown to the user is `min(model window, autoCompactWindow)`.
///
/// Precedence mirrors Claude Code: project local > project > user.
pub fn resolve_auto_compact_window(project_dir: Option<&str>, home: &Path) -> Option<i64> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = project_dir {
        candidates.push(Path::new(dir).join(".claude").join("settings.local.json"));
        candidates.push(Path::new(dir).join(".claude").join("settings.json"));
    }
    candidates.push(home.join(".claude").join("settings.json"));

    candidates.iter().find_map(|path| read_auto_compact_window(path))
}

fn read_auto_compact_window(path: &Path) -> Option<i64> {
    let contents = fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&contents).ok()?;
    let raw = parsed.get("autoCompactWindow")?;
    let value = match raw {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.parse::<f64>().ok()?,
        _ => return None,
    };
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some(sl_core::num::trunc_i64(value))
}

pub fn effective_context_limit(window_size: i64, auto_compact_window: Option<i64>) -> i64 {
    match auto_compact_window {
        Some(w) if w < window_size => w,
        _ => window_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};


    /// A scratch directory of its own, removed when the guard is dropped.
    fn temp_dir() -> (tempfile::TempDir, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let dir = guard.path().to_path_buf();
        (guard, dir)
    }

    fn write_settings(root: &Path, file: &str, json: &str) {
        let claude_dir = root.join(".claude");
        create_dir_all(&claude_dir).unwrap();
        write(claude_dir.join(file), json).unwrap();
    }

    #[test]
    fn reads_user_settings_when_no_project_settings_exist() {
        let (_home_guard, home) = temp_dir();
        write_settings(&home, "settings.json", r#"{"autoCompactWindow": 600000}"#);
        assert_eq!(resolve_auto_compact_window(None, &home), Some(600_000));
    }

    #[test]
    fn project_settings_beat_user_settings() {
        let (_home_guard, home) = temp_dir();
        let (_project_guard, project) = temp_dir();
        write_settings(&home, "settings.json", r#"{"autoCompactWindow": 600000}"#);
        write_settings(&project, "settings.json", r#"{"autoCompactWindow": 200000}"#);
        assert_eq!(resolve_auto_compact_window(project.to_str(), &home), Some(200_000));
    }

    #[test]
    fn local_project_settings_beat_shared_project_settings() {
        let (_home_guard, home) = temp_dir();
        let (_project_guard, project) = temp_dir();
        write_settings(&project, "settings.json", r#"{"autoCompactWindow": 200000}"#);
        write_settings(&project, "settings.local.json", r#"{"autoCompactWindow": 400000}"#);
        assert_eq!(resolve_auto_compact_window(project.to_str(), &home), Some(400_000));
    }

    #[test]
    fn null_when_unset_anywhere() {
        let (_home_guard, home) = temp_dir();
        write_settings(&home, "settings.json", r#"{"model": "opus"}"#);
        assert_eq!(resolve_auto_compact_window(None, &home), None);
    }

    #[test]
    fn ignores_invalid_values_but_accepts_numeric_strings() {
        let (_home_guard, home) = temp_dir();
        write_settings(&home, "settings.json", r#"{"autoCompactWindow": -1}"#);
        assert_eq!(resolve_auto_compact_window(None, &home), None);
        write_settings(&home, "settings.json", r#"{"autoCompactWindow": "600000"}"#);
        assert_eq!(resolve_auto_compact_window(None, &home), Some(600_000));
        write_settings(&home, "settings.json", r#"{"autoCompactWindow": "lots"}"#);
        assert_eq!(resolve_auto_compact_window(None, &home), None);
    }

    #[test]
    fn missing_files_and_broken_json_are_not_errors() {
        let (_home_guard, home) = temp_dir();
        let (_project_guard, project) = temp_dir();
        assert_eq!(resolve_auto_compact_window(project.to_str(), &home), None);
        let claude_dir = home.join(".claude");
        create_dir_all(&claude_dir).unwrap();
        write(claude_dir.join("settings.json"), "{nope").unwrap();
        assert_eq!(resolve_auto_compact_window(None, &home), None);
    }

    #[test]
    fn caps_the_model_window_at_the_compact_window() {
        assert_eq!(effective_context_limit(1_000_000, Some(600_000)), 600_000);
        assert_eq!(effective_context_limit(1_000_000, Some(200_000)), 200_000);
    }

    #[test]
    fn full_window_when_unset_or_larger_than_the_window() {
        assert_eq!(effective_context_limit(1_000_000, None), 1_000_000);
        assert_eq!(effective_context_limit(200_000, Some(600_000)), 200_000);
    }
}
