#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "fixtures, which fail the test loudly"
)]
use statusline::ansi::strip_ansi;
use statusline::format::parse_iso8601_to_epoch_ms as iso_ms;
use statusline::render::{RenderInputs, RenderOptions, render, wrap_segments};
use statusline::types::{SessionState, StatusPayload};

// Modeled on a real payload from Claude Code 2.1.173 (Fable 5 [1m] on Max 5x).
const BASE_PAYLOAD: &str = r#"{
  "model": {"display_name": "Claude Fable 5"},
  "cost": {"total_cost_usd": 1.846, "total_lines_added": 4, "total_lines_removed": 1},
  "context_window": {
    "total_input_tokens": 44000,
    "total_output_tokens": 2090,
    "context_window_size": 1000000,
    "used_percentage": 5
  },
  "rate_limits": {
    "five_hour": {"utilization": 3, "resets_at": "2026-06-11T19:20:00+00:00"},
    "seven_day": {"used_percentage": 45, "resets_at": "2026-06-12T04:00:00+00:00"}
  }
}"#;

fn parse(json: &str) -> StatusPayload {
    serde_json::from_str(json).expect("valid fixture json")
}

fn base_payload() -> StatusPayload {
    parse(BASE_PAYLOAD)
}

fn base_inputs() -> RenderInputs {
    RenderInputs {
        now_ms: iso_ms("2026-06-11T14:34:00Z").unwrap(),
        session_state: Some(SessionState {
            tool_count: 12,
            session_start: iso_ms("2026-06-11T14:29:00Z").unwrap(),
            last_tool: None,
            last_refresh: 0,
        }),
        branch: Some("main".to_string()),
        auto_compact_window: None,
        gh: None,
        options: RenderOptions::default(),
    }
}

fn plain(payload: &StatusPayload, inputs: &RenderInputs) -> String {
    strip_ansi(&render(payload, inputs, None))
}

#[test]
fn full_line_with_all_segments() {
    assert_eq!(
        plain(&base_payload(), &base_inputs()),
        "Session ⠦⠤⠤⠤⠤⠤ 3% 4h 46m | Weekly ⠿⠿⠷⠤⠤⠤ 45% 13h 26m | \
         Context ⠦⠤⠤⠤⠤⠤ 46.1k/1M | $1.85 | Fable 5 | [|] 12 tools 5m | +4 -1 | main"
    );
}

#[test]
fn hidden_segments_are_left_out() {
    let mut inputs = base_inputs();
    inputs.options.hidden = vec!["session".into(), "tools".into(), "lines".into()];
    assert_eq!(
        plain(&base_payload(), &inputs),
        "Weekly ⠿⠿⠷⠤⠤⠤ 45% 13h 26m | Context ⠦⠤⠤⠤⠤⠤ 46.1k/1M | $1.85 | Fable 5 | main"
    );
}

#[test]
fn auto_compact_window_caps_the_displayed_context_limit() {
    let mut inputs = base_inputs();
    inputs.auto_compact_window = Some(600_000);
    assert!(plain(&base_payload(), &inputs).contains("46.1k/600k"));
    inputs.auto_compact_window = Some(200_000);
    assert!(plain(&base_payload(), &inputs).contains("46.1k/200k"));
}

#[test]
fn context_bar_fills_against_the_effective_limit_not_the_model_window() {
    let half = parse(
        r#"{
          "context_window": {
            "total_input_tokens": 100000,
            "total_output_tokens": 0,
            "context_window_size": 1000000,
            "used_percentage": 5
          }
        }"#,
    );
    let mut inputs = base_inputs();
    inputs.auto_compact_window = Some(200_000);
    // 100k of 200k = 50% — bar half full even though it's 10% of the model window
    assert!(plain(&half, &inputs).contains("Context ⠿⠿⠿⠤⠤⠤ 100k/200k"));
}

#[test]
fn no_session_state_no_tools_segment() {
    let mut inputs = base_inputs();
    inputs.session_state = None;
    assert!(!plain(&base_payload(), &inputs).contains("tools"));
}

#[test]
fn rate_limits_accept_both_utilization_and_used_percentage_shapes() {
    let line = plain(&base_payload(), &base_inputs());
    assert!(line.contains("Session"));
    assert!(line.contains("Weekly"));
}

#[test]
fn missing_pieces_drop_their_segments_instead_of_crashing() {
    let empty_inputs = RenderInputs {
        now_ms: 0,
        session_state: None,
        branch: None,
        auto_compact_window: None,
        gh: None,
        options: RenderOptions::default(),
    };
    assert_eq!(plain(&parse("{}"), &empty_inputs), "");

    let minimal = parse(r#"{"model": {"display_name": "Claude Opus"}}"#);
    assert_eq!(plain(&minimal, &empty_inputs), "Opus");
}

#[test]
fn token_less_early_session_falls_back_to_payload_percentage() {
    let early = parse(r#"{"context_window": {"used_percentage": 7}}"#);
    let mut inputs = base_inputs();
    inputs.session_state = None;
    inputs.branch = None;
    assert!(plain(&early, &inputs).contains("Context"));
}

#[test]
fn warns_when_usage_nears_the_effective_limit() {
    let nearly = parse(
        r#"{
          "context_window": {
            "total_input_tokens": 520000,
            "total_output_tokens": 0,
            "context_window_size": 1000000,
            "used_percentage": 5
          }
        }"#,
    );
    let inputs = base_inputs();
    // 520k is 52% of 1M (no warning) but 87% of a 600k compact window
    assert!(!plain(&nearly, &inputs).contains('⚠'));
    let mut capped = inputs;
    capped.auto_compact_window = Some(600_000);
    assert!(plain(&nearly, &capped).contains("520k/600k ⚠"));
}

#[test]
fn cache_hits_and_misses_from_the_current_usage_breakdown() {
    let cached = parse(
        r#"{
          "context_window": {
            "total_input_tokens": 44000,
            "total_output_tokens": 2090,
            "context_window_size": 1000000,
            "used_percentage": 5,
            "current_usage": {
              "input_tokens": 500,
              "cache_creation_input_tokens": 1500,
              "cache_read_input_tokens": 18000
            }
          }
        }"#,
    );
    assert!(plain(&cached, &base_inputs()).contains("cache 18k hit / 2k miss"));
    // absent before the first API call -> no segment
    assert!(!plain(&base_payload(), &base_inputs()).contains("cache"));
}

#[test]
fn api_time_as_share_of_wall_clock() {
    let timed = parse(
        r#"{
          "cost": {
            "total_cost_usd": 1.846,
            "total_lines_added": 4,
            "total_lines_removed": 1,
            "total_duration_ms": 100000,
            "total_api_duration_ms": 38000
          }
        }"#,
    );
    assert!(plain(&timed, &base_inputs()).contains("api 38%"));
    assert!(!plain(&base_payload(), &base_inputs()).contains("api"));
}

#[test]
fn effort_marker_rides_on_the_model_segment() {
    let xhigh =
        parse(r#"{"model": {"display_name": "Claude Fable 5"}, "effort": {"level": "xhigh"}}"#);
    assert!(plain(&xhigh, &base_inputs()).contains("Fable 5 ⚡xhigh"));
    let high =
        parse(r#"{"model": {"display_name": "Claude Fable 5"}, "effort": {"level": "high"}}"#);
    assert!(plain(&high, &base_inputs()).contains("Fable 5 ⚡high"));
    assert!(!plain(&base_payload(), &base_inputs()).contains('⚡'));
}

#[test]
fn wraps_to_multiple_rows_at_segment_boundaries_when_width_is_tight() {
    let inputs = base_inputs();
    let plain_line = plain(&base_payload(), &inputs);
    let lines: Vec<&str> = plain_line.split(" | ").collect();
    let everything = render(&base_payload(), &inputs, None);
    assert!(!everything.contains('\n'));

    let wrapped_full = render(&base_payload(), &inputs, Some(60));
    let wrapped_stripped = strip_ansi(&wrapped_full);
    let wrapped: Vec<&str> = wrapped_stripped.split('\n').collect();
    assert!(wrapped.len() > 1);
    for row in &wrapped {
        assert!(row.chars().count() <= 60);
    }
    // nothing lost or reordered: rejoining the rows gives the unwrapped line
    assert_eq!(wrapped.join(" | "), lines.join(" | "));
}

#[test]
fn zero_lines_changed_is_hidden() {
    let no_lines = parse(r#"{"cost": {"total_cost_usd": 0.5}}"#);
    assert!(!plain(&no_lines, &base_inputs()).contains("+0"));
}

#[test]
fn wrap_segments_single_line_when_everything_fits_or_width_unknown() {
    let segs = vec!["aaa".to_string(), "bbb".to_string()];
    assert_eq!(
        wrap_segments(&segs, Some(20))
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>(),
        vec!["aaa | bbb"]
    );
    assert_eq!(
        wrap_segments(&segs, None)
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>(),
        vec!["aaa | bbb"]
    );
}

#[test]
fn wrap_segments_starts_a_new_row_instead_of_overflowing() {
    let segs = vec!["aaaa".to_string(), "bbbb".to_string(), "cccc".to_string()];
    let joined = strip_ansi(&wrap_segments(&segs, Some(12)).join("\n"));
    assert_eq!(joined, "aaaa | bbbb\ncccc");
}

#[test]
fn wrap_segments_a_segment_wider_than_the_terminal_still_gets_its_own_row() {
    let segs = vec!["aaa".to_string(), "b".repeat(30), "ccc".to_string()];
    assert_eq!(wrap_segments(&segs, Some(10)).len(), 3);
}

#[test]
fn wrap_segments_separator_width_is_counted() {
    let segs = vec!["aaaa".to_string(), "bbbb".to_string()];
    // "aaaa | bbbb" is 11 visible chars: fits at 11, wraps at 10
    assert_eq!(wrap_segments(&segs, Some(11)).len(), 1);
    assert_eq!(wrap_segments(&segs, Some(10)).len(), 2);
}

#[test]
fn wrap_segments_empty_input_renders_nothing() {
    let segs: Vec<String> = vec![];
    assert_eq!(wrap_segments(&segs, Some(80)), Vec::<String>::new());
}

fn gh_inputs(base: &str, ci: Option<statusline::gh::Ci>) -> RenderInputs {
    let mut inputs = base_inputs();
    inputs.branch = Some("feat/x".to_string());
    inputs.gh = Some(statusline::gh::GhInfo {
        pr_number: Some(730),
        base: Some(base.into()),
        base_sha: Some("1adc44e1c5".into()),
        base_pr: Some(729),
        ci,
        ..Default::default()
    });
    inputs
}

fn ci(
    state: statusline::gh::CiState,
    failed: &[(&str, Option<&str>)],
    pending: usize,
) -> statusline::gh::Ci {
    statusline::gh::Ci {
        state,
        pending,
        failed: failed
            .iter()
            .map(|(n, l)| statusline::gh::FailedJob {
                name: n.to_string(),
                link: l.map(String::from),
            })
            .collect(),
    }
}

/** The rows under the main line, without colors. */
fn pr_rows(payload: &StatusPayload, inputs: &RenderInputs) -> Vec<String> {
    strip_ansi(&render(payload, inputs, None))
        .lines()
        .skip(1)
        .map(String::from)
        .collect()
}

#[test]
fn a_stacked_pr_is_one_row_its_state_then_the_branch_it_is_stacked_on() {
    let mut inputs = gh_inputs(
        "develop",
        Some(ci(statusline::gh::CiState::Passing, &[], 0)),
    );
    inputs.gh.as_mut().unwrap().is_draft = Some(true);
    inputs.gh.as_mut().unwrap().review_decision = Some("APPROVED".into());
    let out = strip_ansi(&render(&base_payload(), &inputs, None));
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        !lines[0].contains("feat/x"),
        "the branch is not said twice: {out}"
    );
    assert_eq!(
        &lines[1..],
        ["feat/x (#730) ✓ → develop (#729) | draft | ✓ CI passing"]
    );
    assert!(!out.contains("github.com"), "urls only inside links: {out}");
}

#[test]
fn the_review_marker_is_the_stored_prs_own_verdict() {
    use statusline::gh::{Review, ReviewState};
    let marker = |decision: Option<&str>, reviews: &[ReviewState]| {
        let mut inputs = gh_inputs("main", None);
        let gh = inputs.gh.as_mut().unwrap();
        gh.review_decision = decision.map(String::from);
        gh.reviews = reviews
            .iter()
            .map(|&state| Review {
                author: "a".into(),
                state,
            })
            .collect();
        pr_rows(&base_payload(), &inputs).remove(0)
    };
    assert_eq!(marker(Some("CHANGES_REQUESTED"), &[]), "feat/x (#730) ✗");
    assert_eq!(marker(Some("APPROVED"), &[]), "feat/x (#730) ✓");
    assert_eq!(
        marker(Some("REVIEW_REQUIRED"), &[ReviewState::Approved]),
        "feat/x (#730)",
        "one approval short of the rule"
    );
    assert_eq!(
        marker(None, &[ReviewState::Approved]),
        "feat/x (#730) ✓",
        "no rule: the reviews say"
    );
}

#[test]
fn a_pr_to_the_default_branch_or_a_base_without_a_pr_has_no_parent_number() {
    let mut inputs = gh_inputs("main", None);
    inputs.gh.as_mut().unwrap().is_draft = Some(false);
    assert_eq!(
        pr_rows(&base_payload(), &inputs),
        vec!["feat/x (#730) | ready"]
    );
    let mut inputs = gh_inputs("develop", None);
    inputs.gh.as_mut().unwrap().base_pr = None;
    assert_eq!(
        pr_rows(&base_payload(), &inputs),
        vec!["feat/x (#730) → develop"]
    );
}

#[test]
fn ticket_branches_show_and_link_their_ticket_and_pr_numbers_link_their_pr() {
    let mut inputs = gh_inputs("marc/dn-543-parent", None);
    inputs.branch = Some("marc/dn-544-child".into());
    inputs.gh.as_mut().unwrap().url = Some("https://github.com/o/r/pull/730".into());
    inputs.options.user = Some("marc".into());
    inputs.options.linear_workspace = Some("acme".into());
    assert_eq!(
        pr_rows(&base_payload(), &inputs),
        vec!["DN-544-child (#730) → DN-543-parent (#729)"]
    );
    let out = render(&base_payload(), &inputs, None);
    for url in [
        "https://linear.app/acme/issue/DN-544",
        "https://github.com/o/r/pull/730",
        "https://linear.app/acme/issue/DN-543",
        "https://github.com/o/r/pull/729",
    ] {
        assert!(
            out.contains(&format!("\x1b]8;;{url}\x07")),
            "{url} in {out:?}"
        );
    }
}

#[test]
fn red_ci_links_the_checks_and_lists_failed_jobs_linked_capping_the_rest() {
    use statusline::gh::CiState::Failing;
    let failed = [
        ("lint", Some("https://x/1")),
        ("e2e", None),
        ("unit", Some("https://x/3")),
        ("types", None),
        ("docs", None),
    ];
    let mut inputs = gh_inputs("main", Some(ci(Failing, &failed, 0)));
    inputs.gh.as_mut().unwrap().url = Some("https://github.com/o/r/pull/730".into());
    assert_eq!(
        pr_rows(&base_payload(), &inputs),
        vec![
            "feat/x (#730) | ✗ CI failed: 5",
            "  ✗ lint",
            "  ✗ e2e",
            "  ✗ unit",
            "  +2 more"
        ]
    );
    let out = render(&base_payload(), &inputs, None);
    for url in [
        "https://github.com/o/r/pull/730/checks",
        "https://x/1",
        "https://x/3",
    ] {
        assert!(
            out.contains(&format!("\x1b]8;;{url}\x07")),
            "{url} in {out:?}"
        );
    }
}

#[test]
fn a_failed_jobs_name_and_link_cannot_write_escape_sequences() {
    use statusline::gh::CiState::Failing;
    let failed = [(
        "lint\x1b]52;c;aGk=\x07\x1b[2J",
        Some("https://x/1\x07\x1b]0;title\x07"),
    )];
    let inputs = gh_inputs("main", Some(ci(Failing, &failed, 0)));
    let out = render(&base_payload(), &inputs, None);
    for sequence in ["\x1b]52;", "\x1b[2J", "\x1b]0;"] {
        assert!(!out.contains(sequence), "{sequence:?} in {out:?}");
    }
    assert!(
        out.contains("\x1b]8;;https://x/1]0;title\x07"),
        "the link stays one link: {out:?}"
    );
}

#[test]
fn render_leaves_a_margin_below_columns() {
    let out = strip_ansi(&render(&base_payload(), &base_inputs(), Some(100)));
    assert!(out.lines().all(|l| l.chars().count() <= 96), "{out}");
}
