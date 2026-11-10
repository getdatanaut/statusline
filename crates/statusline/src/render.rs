use crate::ansi::visible_width;
use crate::format::{
    colored, dimmed, fmt_countdown, fmt_elapsed, fmt_tokens, link, make_bar, BRIGHT_GREEN, BRIGHT_RED, CYAN, GREEN,
    RED, WHITE, YELLOW,
};
use crate::gh::{is_non_default_base, CiState, GhInfo, MAX_FAILED_ROWS};
use crate::settings::effective_context_limit;
use crate::types::{RateLimitWindow, SessionState, StatusPayload};

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];
// Auto-compact is imminent once usage nears the effective limit
const COMPACT_WARN_PCT: f64 = 85.0;

/// Everything the renderer needs besides the payload, gathered by main().
#[derive(Clone)]
pub struct RenderInputs {
    pub now_ms: i64,
    pub session_state: Option<SessionState>,
    pub branch: Option<String>,
    pub auto_compact_window: Option<i64>,
    pub gh: Option<GhInfo>,
    pub options: RenderOptions,
}

/// What the user chose on the command line.
#[derive(Clone, Default)]
pub struct RenderOptions {
    /// Segment names from `--hide`: session, weekly, context, cache, cost,
    /// model, tools, api, lines, branch.
    pub hidden: Vec<String>,
    /// `$USER`, whose `user/` branch prefix is left off.
    pub user: Option<String>,
    /// From `--linear-workspace`: the slug in `linear.app/<slug>/issue/…`
    /// that ticket labels link to.
    pub linear_workspace: Option<String>,
}

impl RenderInputs {
    fn shows(&self, segment: &str) -> bool {
        !self.options.hidden.iter().any(|h| h == segment)
    }

    /// The short branch, its ticket (`DN-544-discovery…`) linked to Linear.
    fn branch_label(&self, branch: &str) -> String {
        let label = sl_core::branch::label(branch, self.options.user.as_deref());
        let ticket = label.ticket.map(|id| {
            let text = colored(&id, CYAN);
            match self.options.linear_workspace.as_deref() {
                Some(workspace) => link(&text, &format!("https://linear.app/{workspace}/issue/{id}")),
                None => text,
            }
        });
        let piece = |text: &str| if text.is_empty() { String::new() } else { colored(text, CYAN) };
        format!("{}{}{}", piece(&label.before), ticket.unwrap_or_default(), piece(&label.after))
    }

    /// `(#730)`, linked to the PR. Any PR number in the stack shares the head
    /// PR's repository, so its url is the head PR's with the number swapped.
    fn pr_number(&self, number: i64) -> String {
        let text = colored(&format!("(#{number})"), CYAN);
        let pulls = self.gh.as_ref().and_then(|g| g.url.as_deref()).and_then(|u| u.rsplit_once('/'));
        match pulls {
            Some((pulls, _)) => link(&text, &format!("{pulls}/{number}")),
            None => text,
        }
    }
}

fn separator() -> String {
    dimmed(" | ")
}

/// Columns Claude Code does not give to status output: the row indent plus a
/// little slack. Measured: with `COLUMNS` at the terminal width, rows were
/// cut with `…` at 107 visible columns, a few short of it.
const WRAP_MARGIN: usize = 4;

pub fn render(payload: &StatusPayload, inputs: &RenderInputs, width: Option<usize>) -> String {
    let width = width.map(|w| w.saturating_sub(WRAP_MARGIN).max(1));
    let mut lines = wrap_segments(&render_segments(payload, inputs), width);
    if inputs.shows("branch") {
        lines.extend(wrap_segments(&pr_segments(payload, inputs), width));
        lines.extend(failed_job_rows(inputs));
    }
    lines.join("\n")
}

/// The branch the PR is stacked on, when it is not the default branch.
fn target(inputs: &RenderInputs) -> Option<&str> {
    let gh = inputs.gh.as_ref()?;
    gh.pr_number?;
    gh.base.as_deref().filter(|b| is_non_default_base(b))
}

/// The branch's PR, on a row of its own under the main line:
///
/// ```text
/// marketing-site (#773) ✓ → DN-544 (#771) | draft | ✓ CI passing
/// ```
///
/// the PR with its review state, the branch it is stacked on with that
/// branch's own PR, then the PR's draft and CI state. Empty without a PR: the
/// branch then sits on the main line.
pub fn pr_segments(payload: &StatusPayload, inputs: &RenderInputs) -> Vec<String> {
    let (Some(gh), Some(branch)) = (&inputs.gh, inputs.branch.as_deref()) else { return Vec::new() };
    let Some(number) = gh.pr_number else { return Vec::new() };
    let stacked_on = target(inputs)
        .map(|base| {
            let base_pr = gh.base_pr.map(|n| format!(" {}", inputs.pr_number(n))).unwrap_or_default();
            format!(" {} {}{base_pr}", dimmed("→"), inputs.branch_label(base))
        })
        .unwrap_or_default();
    let mut parts = vec![format!("{} {}{}{stacked_on}", inputs.branch_label(branch), inputs.pr_number(number), review_marker(payload))];

    match gh.is_draft {
        Some(true) => parts.push(colored("draft", YELLOW)),
        Some(false) => parts.push(colored("ready", GREEN)),
        None => {}
    }

    if let Some(ci) = &gh.ci {
        let text = match ci.state {
            CiState::Passing => colored("✓ CI passing", GREEN),
            CiState::Pending => colored(&format!("● CI running ({} pending)", ci.pending), YELLOW),
            CiState::Failing => colored(&format!("✗ CI failed: {}", ci.failed.len()), BRIGHT_RED),
        };
        parts.push(match gh.url.as_deref() {
            Some(url) => link(&text, &format!("{url}/checks")),
            None => text,
        });
    }
    parts
}

/// One row per failed CI job, its name linked to the job.
fn failed_job_rows(inputs: &RenderInputs) -> Vec<String> {
    let Some(ci) = inputs.gh.as_ref().and_then(|g| g.ci.as_ref()) else { return Vec::new() };
    let mut rows: Vec<String> = ci
        .failed
        .iter()
        .take(MAX_FAILED_ROWS)
        .map(|job| {
            let name = colored(&job.name, WHITE);
            let name = job.link.as_deref().map_or_else(|| name.clone(), |url| link(&name, url));
            format!("  {} {name}", colored("✗", BRIGHT_RED))
        })
        .collect();
    if ci.failed.len() > MAX_FAILED_ROWS {
        rows.push(dimmed(&format!("  +{} more", ci.failed.len() - MAX_FAILED_ROWS)));
    }
    rows
}

/// Greedy wrap at segment boundaries: Claude Code renders each stdout line
/// as its own status row, so segments that would overflow the terminal
/// (COLUMNS) start a new row instead of being hard-wrapped mid-segment.
pub fn wrap_segments(segments: &[String], width: Option<usize>) -> Vec<String> {
    if segments.is_empty() {
        return Vec::new();
    }
    let sep = separator();
    let Some(width) = width.filter(|&w| w > 0) else {
        return vec![segments.join(&sep)];
    };

    let sep_width = visible_width(&sep);
    let mut lines = Vec::new();
    let mut line: Vec<&str> = Vec::new();
    let mut line_width = 0usize;
    for segment in segments {
        let segment_width = visible_width(segment);
        if !line.is_empty() && line_width + sep_width + segment_width > width {
            lines.push(line.join(&sep));
            line.clear();
            line_width = 0;
        }
        line.push(segment);
        line_width += if line.len() > 1 { sep_width + segment_width } else { segment_width };
    }
    lines.push(line.join(&sep));
    lines
}

pub fn render_segments(payload: &StatusPayload, inputs: &RenderInputs) -> Vec<String> {
    let mut parts = Vec::new();

    let rate_limits = payload.rate_limits.as_ref();
    if inputs.shows("session")
        && let Some(session) = rate_limit_segment("Session", rate_limits.and_then(|r| r.five_hour.as_ref()), inputs.now_ms)
    {
        parts.push(session);
    }
    if inputs.shows("weekly")
        && let Some(weekly) = rate_limit_segment("Weekly", rate_limits.and_then(|r| r.seven_day.as_ref()), inputs.now_ms)
    {
        parts.push(weekly);
    }

    if inputs.shows("context")
        && let Some(context) = context_segment(payload, inputs.auto_compact_window)
    {
        parts.push(context);
    }

    if inputs.shows("cache")
        && let Some(cache) = cache_segment(payload)
    {
        parts.push(cache);
    }

    if inputs.shows("cost")
        && let Some(cost) = payload.cost.as_ref().and_then(|c| c.total_cost_usd)
    {
        parts.push(colored(&sl_core::format::money(cost), WHITE));
    }

    if inputs.shows("model")
        && let Some(model) = model_name(payload)
    {
        parts.push(format!("{}{}", colored(&model, WHITE), effort_marker(payload)));
    }

    if inputs.shows("tools")
        && let Some(tools) = tools_segment(inputs.session_state.as_ref(), inputs.now_ms)
    {
        parts.push(tools);
    }

    if inputs.shows("api")
        && let Some(api) = api_share_segment(payload)
    {
        parts.push(api);
    }

    let added = payload.cost.as_ref().and_then(|c| c.total_lines_added).unwrap_or(0);
    let removed = payload.cost.as_ref().and_then(|c| c.total_lines_removed).unwrap_or(0);
    if inputs.shows("lines") && (added > 0 || removed > 0) {
        parts.push(format!("{} {}", colored(&format!("+{added}"), BRIGHT_GREEN), colored(&format!("-{removed}"), BRIGHT_RED)));
    }

    // A branch with a PR leads the PR row instead.
    if inputs.shows("branch")
        && inputs.gh.as_ref().and_then(|g| g.pr_number).is_none()
        && let Some(branch) = inputs.branch.as_ref()
    {
        parts.push(inputs.branch_label(branch));
    }

    parts
}

fn review_marker(payload: &StatusPayload) -> String {
    match payload.pr.as_ref().and_then(|p| p.review_state.as_deref()) {
        Some("approved") => format!(" {}", colored("✓", BRIGHT_GREEN)),
        Some("changes_requested") => format!(" {}", colored("✗", BRIGHT_RED)),
        _ => String::new(),
    }
}

fn rate_limit_segment(label: &str, window: Option<&RateLimitWindow>, now_ms: i64) -> Option<String> {
    let window = window?;
    let pct = window.used_percent()?;
    let reset = fmt_countdown(window.resets_at.as_ref(), now_ms);
    let suffix = reset.map(|r| format!(" {r}")).unwrap_or_default();
    let text = format!("{label} {} {}%{suffix}", make_bar(pct, 6, false), sl_core::num::trunc_i64(pct.round()));
    Some(colored(&text, WHITE))
}

/// How full the context is: the percentage of the effective limit (the window,
/// or the auto-compact window when that is smaller), and the tokens behind it
/// when the payload has them. The one computation behind both the Context bar
/// and the capture the tray reads (`context::capture_context`).
pub fn context_use(payload: &StatusPayload, auto_compact_window: Option<i64>) -> Option<(f64, Option<(i64, i64)>)> {
    let ctx = payload.context_window.as_ref()?;
    if let (Some(window_size), Some(input)) = (ctx.context_window_size, ctx.total_input_tokens) {
        let limit = effective_context_limit(window_size, auto_compact_window);
        let used = input + ctx.total_output_tokens.unwrap_or(0);
        return Some(((used as f64 / limit as f64) * 100.0, Some((used, limit))));
    }
    // Early in a session the token fields can be null; fall back to the
    // payload's own percentage (relative to the full model window).
    ctx.used_percentage.map(|pct| (pct, None))
}

fn context_segment(payload: &StatusPayload, auto_compact_window: Option<i64>) -> Option<String> {
    let (pct, tokens) = context_use(payload, auto_compact_window)?;
    let text = match tokens {
        Some((used, limit)) => format!("Context {} {}/{}{}", make_bar(pct, 6, false), fmt_tokens(used), fmt_tokens(limit), compact_warning(pct)),
        None => format!("Context {} {}%{}", make_bar(pct, 6, false), sl_core::num::trunc_i64(pct.round()), compact_warning(pct)),
    };
    Some(colored(&text, WHITE))
}

fn compact_warning(pct: f64) -> String {
    if pct >= COMPACT_WARN_PCT {
        format!(" {}", colored("⚠", RED))
    } else {
        String::new()
    }
}

/// Tokens of the last API call's input served from prompt cache (hit) vs not
/// (miss). Many misses mean full-price tokens for context that should be cached.
fn cache_segment(payload: &StatusPayload) -> Option<String> {
    let usage = payload.context_window.as_ref()?.current_usage.as_ref()?;
    let read = usage.cache_read_input_tokens.unwrap_or(0);
    // Misses are the input paid for at full (or cache-write) price.
    let miss = usage.input_tokens.unwrap_or(0) + usage.cache_creation_input_tokens.unwrap_or(0);
    let total = miss + read;
    if total == 0 {
        return None;
    }
    // The colour still follows the hit rate, so a glance reads the same.
    let hit_pct = (read as f64 / total as f64) * 100.0;
    let color = if hit_pct >= 75.0 { GREEN } else if hit_pct >= 40.0 { YELLOW } else { RED };
    Some(format!(
        "{}{}{}{}",
        dimmed("cache "),
        colored(&format!("{} hit", fmt_tokens(read)), color),
        dimmed(" / "),
        colored(&format!("{} miss", fmt_tokens(miss)), RED)
    ))
}

/// API time as a share of session wall-clock: is the session slow because of
/// the model, or the tools/user?
fn api_share_segment(payload: &StatusPayload) -> Option<String> {
    let cost = payload.cost.as_ref()?;
    let api = cost.total_api_duration_ms?;
    let total = cost.total_duration_ms?;
    if total <= 0.0 {
        return None;
    }
    let pct = sl_core::num::trunc_i64(((api / total) * 100.0).round());
    Some(format!("{}{}", dimmed("api "), colored(&format!("{pct}%"), WHITE)))
}

fn effort_marker(payload: &StatusPayload) -> String {
    let Some(level) = payload.effort.as_ref().and_then(|e| e.level.as_deref()) else {
        return String::new();
    };
    if level == "xhigh" {
        format!(" {}", colored("⚡xhigh", YELLOW))
    } else {
        format!(" {}", dimmed(&format!("⚡{level}")))
    }
}

fn tools_segment(state: Option<&SessionState>, now_ms: i64) -> Option<String> {
    let state = state?;
    let spinner = SPINNER[(state.tool_count % 4) as usize];
    let elapsed = fmt_elapsed(now_ms - state.session_start);
    Some(colored(&format!("[{spinner}] {} tools {elapsed}", state.tool_count), WHITE))
}

fn model_name(payload: &StatusPayload) -> Option<String> {
    if let Some(display) = payload.model.as_ref().and_then(|m| m.display_name.as_deref()) {
        return Some(strip_claude_prefix(display));
    }
    payload.model.as_ref().and_then(|m| m.id.clone())
}

fn strip_claude_prefix(s: &str) -> String {
    match s.strip_prefix("Claude") {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start().to_string(),
        _ => s.to_string(),
    }
}
