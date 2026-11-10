use crate::ansi::{printable, visible_width};
use crate::format::{
    BRIGHT_GREEN, BRIGHT_RED, CYAN, GREEN, RED, WHITE, YELLOW, colored, dimmed, fmt_countdown,
    fmt_elapsed, fmt_tokens, make_bar,
};
use crate::gh::{CiState, GhInfo, MAX_FAILED_ROWS, Verdict, is_non_default_base};
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
    let review = inputs.gh.as_ref().map(review_marker).unwrap_or_default();
    lines.extend(gh_rows(inputs, &review, width));
    lines.join("\n")
}

/// The PR's target branch, when it targets something other than the default
/// branch -- the only case that gets the two-row head/target layout.
fn target(inputs: &RenderInputs) -> Option<&str> {
    let gh = inputs.gh.as_ref()?;
    gh.pr_number?;
    gh.base.as_deref().filter(|b| is_non_default_base(b))
}

fn pr_number(number: i64) -> String {
    colored(&format!("(#{number})"), CYAN)
}

/// The GitHub rows under the main line. For a PR that targets another branch:
///
/// ```text
/// head-branch (#730) ✓
///   → base-branch (1adc44e1c5) (#729) | draft | ✓ CI passing
/// ```
///
/// the head with its own PR, then the base with its tip commit and *its* PR
/// (the parent in a stack). Then one row per failed job. Each is its own
/// stdout line, so Claude Code shows it as its own status row.
pub fn gh_rows(inputs: &RenderInputs, review: &str, width: Option<usize>) -> Vec<String> {
    let Some(gh) = &inputs.gh else {
        return Vec::new();
    };
    let mut head_row = None;
    let mut parts = Vec::new();

    if let Some(base) = target(inputs) {
        if let (Some(branch), Some(number)) = (inputs.branch.as_deref(), gh.pr_number) {
            head_row = Some(format!(
                "{} {}{review}",
                colored(branch, CYAN),
                pr_number(number)
            ));
        }
        let sha = gh
            .base_sha
            .as_deref()
            .map(|s| format!(" {}", dimmed(&format!("({s})"))))
            .unwrap_or_default();
        let base_pr = gh
            .base_pr
            .map(|n| format!(" {}", pr_number(n)))
            .unwrap_or_default();
        parts.push(format!(
            "  {} {}{sha}{base_pr}",
            dimmed("→"),
            colored(base, CYAN)
        ));
    }

    match gh.is_draft {
        Some(true) => parts.push(colored("draft", YELLOW)),
        Some(false) if gh.pr_number.is_some() => parts.push(colored("ready", GREEN)),
        _ => {}
    }

    let mut rows = Vec::new();
    if let Some(ci) = &gh.ci {
        parts.push(match ci.state {
            CiState::Passing => colored("✓ CI passing", BRIGHT_GREEN),
            CiState::Pending => colored(&format!("● CI running ({} pending)", ci.pending), YELLOW),
            CiState::Failing => colored(&format!("✗ CI failed: {}", ci.failed.len()), BRIGHT_RED),
        });
        for job in ci.failed.iter().take(MAX_FAILED_ROWS) {
            let link = job
                .link
                .as_deref()
                .map(|l| format!(" {}", dimmed(&printable(l))))
                .unwrap_or_default();
            rows.push(format!(
                "  {} {}{link}",
                colored("✗", BRIGHT_RED),
                colored(&printable(&job.name), WHITE)
            ));
        }
        if ci.failed.len() > MAX_FAILED_ROWS {
            rows.push(dimmed(&format!(
                "  +{} more",
                ci.failed.len() - MAX_FAILED_ROWS
            )));
        }
    }

    let mut out: Vec<String> = head_row.into_iter().collect();
    out.extend(wrap_segments(&parts, width));
    out.extend(rows);
    out
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
        line_width += if line.len() > 1 {
            sep_width + segment_width
        } else {
            segment_width
        };
    }
    lines.push(line.join(&sep));
    lines
}

pub fn render_segments(payload: &StatusPayload, inputs: &RenderInputs) -> Vec<String> {
    let mut parts = Vec::new();

    let rate_limits = payload.rate_limits.as_ref();
    if let Some(session) = rate_limit_segment(
        "Session",
        rate_limits.and_then(|r| r.five_hour.as_ref()),
        inputs.now_ms,
    ) {
        parts.push(session);
    }
    if let Some(weekly) = rate_limit_segment(
        "Weekly",
        rate_limits.and_then(|r| r.seven_day.as_ref()),
        inputs.now_ms,
    ) {
        parts.push(weekly);
    }

    if let Some(context) = context_segment(payload, inputs.auto_compact_window) {
        parts.push(context);
    }

    if let Some(cache) = cache_segment(payload) {
        parts.push(cache);
    }

    if let Some(cost) = payload.cost.as_ref().and_then(|c| c.total_cost_usd) {
        parts.push(colored(&sl_core::format::money(cost), WHITE));
    }

    if let Some(model) = model_name(payload) {
        parts.push(format!(
            "{}{}",
            colored(&model, WHITE),
            effort_marker(payload)
        ));
    }

    if let Some(tools) = tools_segment(inputs.session_state.as_ref(), inputs.now_ms) {
        parts.push(tools);
    }

    if let Some(api) = api_share_segment(payload) {
        parts.push(api);
    }

    let added = payload
        .cost
        .as_ref()
        .and_then(|c| c.total_lines_added)
        .unwrap_or(0);
    let removed = payload
        .cost
        .as_ref()
        .and_then(|c| c.total_lines_removed)
        .unwrap_or(0);
    if added > 0 || removed > 0 {
        parts.push(format!(
            "{} {}",
            colored(&format!("+{added}"), BRIGHT_GREEN),
            colored(&format!("-{removed}"), BRIGHT_RED)
        ));
    }

    // With a target the branch leads the GitHub rows instead, so it is not
    // said twice; otherwise it carries its PR number and review state.
    if let Some(branch) = inputs.branch.as_ref().filter(|_| target(inputs).is_none()) {
        let pr = inputs.gh.as_ref().and_then(|gh| {
            gh.pr_number
                .map(|n| format!(" {}{}", pr_number(n), review_marker(gh)))
        });
        parts.push(format!(
            "{}{}",
            colored(branch, CYAN),
            pr.unwrap_or_default()
        ));
    }

    parts
}

/// The review outcome of the PR the row shows, as the gh store holds it.
fn review_marker(gh: &GhInfo) -> String {
    match gh.verdict() {
        Some(Verdict::Approved) => format!(" {}", colored("✓", BRIGHT_GREEN)),
        Some(Verdict::ChangesRequested) => format!(" {}", colored("✗", BRIGHT_RED)),
        None => String::new(),
    }
}

fn rate_limit_segment(
    label: &str,
    window: Option<&RateLimitWindow>,
    now_ms: i64,
) -> Option<String> {
    let window = window?;
    let pct = window.used_percent()?;
    let reset = fmt_countdown(window.resets_at.as_ref(), now_ms);
    let suffix = reset.map(|r| format!(" {r}")).unwrap_or_default();
    let text = format!(
        "{label} {} {}%{suffix}",
        make_bar(pct, 6, false),
        sl_core::num::trunc_i64(pct.round())
    );
    Some(colored(&text, WHITE))
}

/// How full the context is: the percentage of the effective limit (the window,
/// or the auto-compact window when that is smaller), and the tokens behind it
/// when the payload has them. The one computation behind both the Context bar
/// and the capture the tray reads (`context::capture_context`).
pub fn context_use(
    payload: &StatusPayload,
    auto_compact_window: Option<i64>,
) -> Option<(f64, Option<(i64, i64)>)> {
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
        Some((used, limit)) => format!(
            "Context {} {}/{}{}",
            make_bar(pct, 6, false),
            fmt_tokens(used),
            fmt_tokens(limit),
            compact_warning(pct)
        ),
        None => format!(
            "Context {} {}%{}",
            make_bar(pct, 6, false),
            sl_core::num::trunc_i64(pct.round()),
            compact_warning(pct)
        ),
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
    let color = if hit_pct >= 75.0 {
        GREEN
    } else if hit_pct >= 40.0 {
        YELLOW
    } else {
        RED
    };
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
    Some(format!(
        "{}{}",
        dimmed("api "),
        colored(&format!("{pct}%"), WHITE)
    ))
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
    Some(colored(
        &format!("[{spinner}] {} tools {elapsed}", state.tool_count),
        WHITE,
    ))
}

fn model_name(payload: &StatusPayload) -> Option<String> {
    if let Some(display) = payload
        .model
        .as_ref()
        .and_then(|m| m.display_name.as_deref())
    {
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
