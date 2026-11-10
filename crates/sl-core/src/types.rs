use serde::{Deserialize, Serialize};

/// JSON piped to the statusLine command by Claude Code (fields we use).
///
/// Parsed in one pass, so a field of a type this does not expect fails the
/// whole payload and blanks the line: only fields something reads belong here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatusPayload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<Model>,
    pub workspace: Option<Workspace>,
    pub worktree: Option<Worktree>,
    pub cost: Option<Cost>,
    pub context_window: Option<ContextWindow>,
    pub effort: Option<Effort>,
    pub rate_limits: Option<RateLimits>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Model {
    pub id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Workspace {
    pub current_dir: Option<String>,
    pub project_dir: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Worktree {
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Cost {
    pub total_cost_usd: Option<f64>,
    pub total_duration_ms: Option<f64>,
    pub total_api_duration_ms: Option<f64>,
    pub total_lines_added: Option<i64>,
    pub total_lines_removed: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContextWindow {
    pub total_input_tokens: Option<i64>,
    pub total_output_tokens: Option<i64>,
    pub context_window_size: Option<i64>,
    pub used_percentage: Option<f64>,
    /// Last API call's token breakdown; absent before the first call.
    pub current_usage: Option<CurrentUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CurrentUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_input_tokens: Option<i64>,
    pub cache_read_input_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Effort {
    pub level: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RateLimits {
    pub five_hour: Option<RateLimitWindow>,
    pub seven_day: Option<RateLimitWindow>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RateLimitWindow {
    /// Documented name, and what Claude Code 2.1.258 actually sends (it
    /// multiplies the header's 0-1 utilization by 100 on the way out).
    pub used_percentage: Option<f64>,
    /// ...but real payloads have been seen using this one.
    pub utilization: Option<f64>,
    /// Epoch seconds (what 2.1.258 sends) or an ISO 8601 string, depending on
    /// version. [`crate::format::resets_at_ms`] normalises both.
    pub resets_at: Option<ResetsAt>,
}

impl RateLimitWindow {
    /// How much of the window is used, 0-100: `used_percentage` first, then
    /// `utilization`, so the status line and the tray can never disagree.
    pub fn used_percent(&self) -> Option<f64> {
        self.used_percentage.or(self.utilization)
    }
}

/// One session's context use, as the status line last showed it: written by
/// the status line to [`crate::paths::context_file`], read by the tray for
/// the ring on each idle session's row. The same numbers as the status line's
/// own Context bar, so the two never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContextCapture {
    /// Of the effective limit (the window, or the auto-compact window when
    /// that is smaller), 0-100 and beyond.
    pub percent: f64,
    pub used_tokens: Option<i64>,
    pub limit_tokens: Option<i64>,
    pub captured_at_ms: i64,
}

/// `limits.json`: the status-line payload's `rate_limits` value, exactly as
/// Claude Code sent it, plus when the hook saw it.
///
/// Deliberately asymmetric with its writer. `statusline::limits` keeps the
/// value as a raw `serde_json::Value` so that fields this workspace does not
/// model yet still survive into the file; this is the *reader's* view, which
/// only has to model what the tray uses. The two sides agree on the file's two
/// key names and nothing else -- `statusline::limits`'s tests round-trip a
/// written file through this struct to keep them agreeing.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LimitsCapture {
    /// Epoch milliseconds the hook wrote the file. `None` for a file written
    /// by some other tool, which is not a capture this workspace can date and
    /// so not one it will present as live.
    pub captured_at_ms: Option<i64>,
    pub rate_limits: Option<RateLimits>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ResetsAt {
    Epoch(f64),
    Iso(String),
}

/// JSON piped to PostToolUse hook commands (fields we use).
#[derive(Debug, Clone, Deserialize)]
pub struct HookPayload {
    pub session_id: Option<String>,
    pub tool_name: Option<String>,
}

/// Per-session counters, persisted under the state dir keyed by session_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    pub tool_count: u64,
    /// Epoch milliseconds of the first tracked tool use.
    pub session_start: i64,
    pub last_tool: Option<String>,
    /// Epoch milliseconds of the most recent tracked tool use.
    pub last_refresh: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_percentage_wins_over_utilization() {
        let window = |used_percentage, utilization| RateLimitWindow {
            used_percentage,
            utilization,
            resets_at: None,
        };
        assert_eq!(window(Some(12.5), Some(99.0)).used_percent(), Some(12.5));
        assert_eq!(window(None, Some(40.0)).used_percent(), Some(40.0));
        assert_eq!(window(None, None).used_percent(), None);
    }

    #[test]
    fn fields_nothing_reads_cannot_fail_the_payload() {
        // Shapes nobody expects, in fields the status line has no use for:
        // the windows and the session still parse.
        let payload: StatusPayload = serde_json::from_str(
            r#"{
              "session_id": "abc",
              "version": 2,
              "worktree": {"name": 5, "branch": "feat/x"},
              "pr": "https://github.com/o/r/pull/1",
              "rate_limits": {
                "five_hour": {"used_percentage": 12.5, "resets_at": 1787580599},
                "limits": [{"is_active": 1, "scope": {"surface": {"display_name": "web"}}}],
                "spend_limit": {"used_percentage": 3, "period": "month"}
              }
            }"#,
        )
        .expect("parses");
        assert_eq!(payload.session_id.as_deref(), Some("abc"));
        assert_eq!(
            payload
                .rate_limits
                .and_then(|limits| limits.five_hour)
                .and_then(|window| window.used_percentage),
            Some(12.5)
        );
    }

    #[test]
    fn a_capture_file_parses_and_a_broken_one_does_not_take_the_payload_with_it() {
        let capture: LimitsCapture =
            serde_json::from_str(r#"{"captured_at_ms": 1788200392112, "rate_limits": {"five_hour": {"used_percentage": 7}}}"#)
                .expect("parses");
        assert_eq!(capture.captured_at_ms, Some(1_788_200_392_112));
        assert_eq!(
            capture
                .rate_limits
                .expect("rate_limits")
                .five_hour
                .expect("five_hour")
                .used_percentage,
            Some(7.0)
        );

        // Both halves are independently optional: a file with neither is a
        // capture that says nothing, not a parse error.
        let empty: LimitsCapture = serde_json::from_str("{}").expect("parses");
        assert!(empty.captured_at_ms.is_none() && empty.rate_limits.is_none());
    }
}
