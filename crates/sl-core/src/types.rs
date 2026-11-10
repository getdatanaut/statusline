use serde::{Deserialize, Serialize};

/// JSON piped to the statusLine command by Claude Code (fields we use).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatusPayload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub version: Option<String>,
    pub model: Option<Model>,
    pub workspace: Option<Workspace>,
    pub worktree: Option<Worktree>,
    pub cost: Option<Cost>,
    pub context_window: Option<ContextWindow>,
    pub effort: Option<Effort>,
    pub rate_limits: Option<RateLimits>,
    /// Present while an open PR exists for the current branch.
    pub pr: Option<Pr>,
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
    pub name: Option<String>,
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
    /// The server's own per-window rows, when a payload carries them.
    ///
    /// Additive and entirely optional. Claude Code 2.1.258's status-line
    /// payload does **not** send this -- it builds `rate_limits` from the
    /// `anthropic-ratelimit-unified-*` response headers, which only yield the
    /// two windows above. The array does exist in `~/.claude.json`'s
    /// `cachedUsageUtilization.utilization`, where it is the only source of
    /// the per-model (`weekly_scoped`) rows, so it is modelled here for the
    /// day a payload starts carrying it -- and the hook's capture keeps the
    /// raw JSON either way, so nothing is lost until then.
    pub limits: Option<Vec<RateLimitEntry>>,
}

/// One row of [`RateLimits::limits`]: a single window the server tracks.
///
/// Every field is optional because none of them is guaranteed -- a
/// `weekly_scoped` row has a `scope` and a null `resets_at`, a `session` row
/// is the other way round.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RateLimitEntry {
    /// `"session"`, `"weekly_all"`, `"weekly_scoped"`, ...
    pub kind: Option<String>,
    /// `"session"` / `"weekly"` -- which bucket `kind` belongs to.
    pub group: Option<String>,
    /// Percent of the window used, 0-100.
    pub percent: Option<f64>,
    /// `"normal"`, and whatever else the server decides to send.
    pub severity: Option<String>,
    pub resets_at: Option<ResetsAt>,
    /// What the row is scoped to, for a row that is not account-wide.
    pub scope: Option<RateLimitScope>,
    pub is_active: Option<bool>,
}

/// What a [`RateLimitEntry`] applies to. Nothing here is hardcoded to a model
/// name: a per-model row is whatever `model.display_name` the payload happens
/// to carry.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RateLimitScope {
    pub model: Option<Model>,
    /// Shape unspecified and always null in every payload seen so far, so it
    /// is kept as raw JSON rather than guessed at -- a wrong guess here would
    /// fail the whole `StatusPayload` deserialization, which renders as a
    /// blank status line.
    pub surface: Option<serde_json::Value>,
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

/// `limits.json`: the status-line payload's `rate_limits` value, exactly as
/// Claude Code sent it, plus when the hook saw it.
///
/// Deliberately asymmetric with its writer. `statusline::limits` keeps the
/// value as a raw `serde_json::Value` so that fields this workspace does not
/// model yet still survive into the file; this is the *reader's* view, which
/// only has to model what the tray uses. The two sides agree on the file's two
/// key names and nothing else -- `statusline::limits`'s tests round-trip a
/// written file through this struct to keep them agreeing.
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Pr {
    pub number: Option<i64>,
    pub url: Option<String>,
    pub review_state: Option<String>,
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
        let window = |used_percentage, utilization| RateLimitWindow { used_percentage, utilization, resets_at: None };
        assert_eq!(window(Some(12.5), Some(99.0)).used_percent(), Some(12.5));
        assert_eq!(window(None, Some(40.0)).used_percent(), Some(40.0));
        assert_eq!(window(None, None).used_percent(), None);
    }

    /// Verbatim from `~/.claude.json`'s `.cachedUsageUtilization.utilization`
    /// -- the only place this workspace has ever seen a `limits[]` array. The
    /// status-line payload itself does not carry one (see [`RateLimits::limits`]),
    /// which is precisely why this is pinned as a fixture rather than derived
    /// from a live capture.
    const REAL_LIMITS: &str = r#"{
      "five_hour":  { "utilization": 12, "resets_at": "2026-08-24T14:09:59.911461+00:00",
                      "limit_dollars": null, "used_dollars": null, "remaining_dollars": null },
      "seven_day":  { "utilization": 15, "resets_at": "2026-08-28T03:59:59.911480+00:00",
                      "limit_dollars": null, "used_dollars": null, "remaining_dollars": null },
      "limits": [
        { "kind": "session",       "group": "session", "percent": 12, "severity": "normal",
          "resets_at": "2026-08-24T14:09:59.911461+00:00", "scope": null, "is_active": false },
        { "kind": "weekly_all",    "group": "weekly",  "percent": 15, "severity": "normal",
          "resets_at": "2026-08-28T03:59:59.911480+00:00", "scope": null, "is_active": true },
        { "kind": "weekly_scoped", "group": "weekly",  "percent": 0,  "severity": "normal",
          "resets_at": null,
          "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null },
          "is_active": false }
      ]
    }"#;

    fn iso(resets_at: &ResetsAt) -> &str {
        match resets_at {
            ResetsAt::Iso(text) => text,
            ResetsAt::Epoch(_) => panic!("expected an ISO 8601 reset time"),
        }
    }

    #[test]
    fn the_real_limits_array_deserializes() {
        let limits: RateLimits = serde_json::from_str(REAL_LIMITS).expect("the real shape parses");
        assert_eq!(limits.five_hour.expect("five_hour").utilization, Some(12.0));

        let rows = limits.limits.expect("limits[] is present");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind.as_deref(), Some("session"));
        assert_eq!(rows[0].group.as_deref(), Some("session"));
        assert_eq!(rows[0].percent, Some(12.0));
        assert_eq!(rows[0].severity.as_deref(), Some("normal"));
        assert_eq!(rows[0].is_active, Some(false));
        // A null `scope` is an account-wide row, not a parse failure.
        assert!(rows[0].scope.is_none());
        assert_eq!(iso(rows[1].resets_at.as_ref().expect("weekly_all resets")), "2026-08-28T03:59:59.911480+00:00");
        assert_eq!(rows[1].is_active, Some(true));
    }

    #[test]
    fn a_scoped_row_carries_its_model_name_and_no_reset_time() {
        let limits: RateLimits = serde_json::from_str(REAL_LIMITS).expect("the real shape parses");
        let scoped = limits.limits.expect("limits[]").into_iter().find(|row| row.kind.as_deref() == Some("weekly_scoped")).expect("scoped row");

        assert!(scoped.resets_at.is_none(), "a scoped row's resets_at is null");
        let scope = scoped.scope.expect("a scoped row has a scope");
        let model = scope.model.expect("scope.model");
        // Nothing is hardcoded about the name -- whatever the payload says.
        assert_eq!(model.display_name.as_deref(), Some("Fable"));
        assert!(model.id.is_none());
        assert!(scope.surface.is_none_or(|surface| surface.is_null()), "a null surface stays null rather than failing the parse");
    }

    #[test]
    fn limits_is_purely_additive() {
        // The two windows still parse from a payload that has never heard of
        // `limits[]` -- which is every real status-line payload so far.
        let without: RateLimits =
            serde_json::from_str(r#"{"five_hour": {"used_percentage": 12.5, "resets_at": 1787580599}}"#).expect("parses");
        assert!(without.limits.is_none());
        assert_eq!(without.five_hour.expect("five_hour").used_percentage, Some(12.5));
    }

    #[test]
    fn a_capture_file_parses_and_a_broken_one_does_not_take_the_payload_with_it() {
        let capture: LimitsCapture =
            serde_json::from_str(r#"{"captured_at_ms": 1788200392112, "rate_limits": {"five_hour": {"used_percentage": 7}}}"#)
                .expect("parses");
        assert_eq!(capture.captured_at_ms, Some(1_788_200_392_112));
        assert_eq!(capture.rate_limits.expect("rate_limits").five_hour.expect("five_hour").used_percentage, Some(7.0));

        // Both halves are independently optional: a file with neither is a
        // capture that says nothing, not a parse error.
        let empty: LimitsCapture = serde_json::from_str("{}").expect("parses");
        assert!(empty.captured_at_ms.is_none() && empty.rate_limits.is_none());
    }
}
