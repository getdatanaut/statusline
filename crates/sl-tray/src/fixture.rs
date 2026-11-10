//! Made-up tray state for the self-test and `--dump-views`: usage, limits,
//! open and closed sessions and a PR stack, standing in for what the refresh
//! and sessions threads would publish, so the tray can be driven with
//! nothing running, nothing read from disk and the clock fixed.
//!
//! `SL_TRAY_FIXTURE` names one: `full` (every state the menu and the popover
//! can show a session in), `stale` (as `full`, with a limits capture from
//! hours ago), `nolimits` (no capture at all) or `empty` (nothing open), or a
//! path to a `--dump-snapshot` JSON to use as the usage instead.

use crate::cache_ttl::CacheInfo;
use crate::sessions::{Activity, OpenSession};
use crate::snapshot::{CurrentBlock, DayModel, LatestSession, LimitWindow, LimitsSnapshot, ModelUsage, SessionStats, Snapshot, Snapshots, Totals};
use crate::stacks::StackView;
use crate::sticky::Remembered;
use crate::view_prefs::Source;

/// The environment variable naming the fixture.
pub(crate) const ENV: &str = "SL_TRAY_FIXTURE";

/// The instant every fixture is set at, the previews' own.
pub(crate) const NOW: i64 = 1_788_200_000_000;

const MIN: i64 = 60_000;

/// Everything the background threads would otherwise publish.
pub(crate) struct Fixture {
    pub snapshots: Snapshots,
    pub open: Vec<OpenSession>,
    pub closed: Vec<Remembered>,
    pub stacks: Vec<StackView>,
}

/// The fixture named by [`ENV`], if one is.
pub(crate) fn from_env() -> Option<Result<Fixture, String>> {
    let spec = std::env::var(ENV).ok().filter(|spec| !spec.is_empty())?;
    Some(load(&spec))
}

/// The fixture `spec` names; see the module docs.
pub(crate) fn load(spec: &str) -> Result<Fixture, String> {
    let mut fixture = full();
    match spec {
        "full" => {}
        "stale" => {
            for snapshot in snapshots_mut(&mut fixture.snapshots) {
                if let Some(limits) = snapshot.rate_limits.as_mut() {
                    limits.captured_at_ms = NOW - 6 * 60 * MIN;
                }
            }
        }
        "nolimits" => {
            for snapshot in snapshots_mut(&mut fixture.snapshots) {
                snapshot.rate_limits = None;
                snapshot.codex_limits = None;
            }
        }
        "empty" => {
            fixture.open.clear();
            fixture.closed.clear();
            fixture.stacks.clear();
        }
        path if std::path::Path::new(path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("json")) => {
            let json = std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
            let snapshot: Snapshot = serde_json::from_str(&json).map_err(|error| format!("{path}: {error}"))?;
            fixture.snapshots = Snapshots { claude: snapshot.clone(), codex: Some(snapshot.clone()), all: Some(snapshot) };
        }
        other => return Err(format!("no fixture called {other:?}: full, stale, nolimits, empty or a snapshot .json")),
    }
    Ok(fixture)
}

fn snapshots_mut(snapshots: &mut Snapshots) -> impl Iterator<Item = &mut Snapshot> {
    std::iter::once(&mut snapshots.claude).chain(snapshots.codex.as_mut()).chain(snapshots.all.as_mut())
}

/// Every state at once.
fn full() -> Fixture {
    let (mut open, _) = crate::controller::preview_samples(NOW);
    // The previews' PRs have no URL, which a real one always has.
    for gh in open.iter_mut().filter_map(|session| session.gh.as_mut()) {
        if let Some(number) = gh.pr_number {
            gh.url.get_or_insert_with(|| format!("https://github.com/o/r/pull/{number}"));
        }
    }
    open.extend(extra_sessions());
    Fixture { snapshots: snapshots(), open, closed: closed(), stacks: vec![crate::controller::preview_stack()] }
}

/// The states the previews' samples leave out: compacted, changes
/// requested, approved, `/rc` on, a Codex thread with a context reading.
fn extra_sessions() -> Vec<OpenSession> {
    use sl_core::gh::{Ci, CiState, GhInfo, Review, ReviewState};
    let session = |id: &str, branch: &str, gh: Option<GhInfo>| OpenSession {
        session_id: format!("{id}-id"),
        name: id.into(),
        project: "statusline".into(),
        cwd: format!("/x/{id}"),
        activity: Activity::Idle,
        started_ms: NOW - 120 * MIN,
        branch: Some(branch.into()),
        gh,
        cache: Some(CacheInfo { last_activity_ms: NOW - 20 * MIN, ttl_ms: Some(60 * MIN), compacted: false }),
        remote_control: None,
        pid: Some(4242),
        context: Some(sl_core::types::ContextCapture { percent: 88.0, used_tokens: Some(176_000), limit_tokens: Some(200_000), captured_at_ms: NOW - MIN }),
        linear: None,
        provider: Source::Claude,
        last_activity_ms: None,
    };
    let pr = |number: i64, decision: &str, state: ReviewState| GhInfo {
        pr_number: Some(number),
        is_draft: Some(false),
        url: Some(format!("https://github.com/o/r/pull/{number}")),
        ci: Some(Ci { state: CiState::Passing, failed: Vec::new(), pending: 0 }),
        review_decision: Some(decision.into()),
        reviews: vec![Review { author: "reviewer".into(), state }],
        requested: vec!["team-a".into()],
        ..GhInfo::default()
    };
    vec![
        OpenSession {
            cache: Some(CacheInfo { last_activity_ms: NOW - 3 * MIN, ttl_ms: None, compacted: true }),
            ..session("compacted-run", "feat/compacted", None)
        },
        session("changes-wanted", "feat/changes-wanted", Some(pr(801, "CHANGES_REQUESTED", ReviewState::ChangesRequested))),
        session("approved-pr", "feat/approved", Some(pr(802, "APPROVED", ReviewState::Approved))),
        OpenSession { remote_control: Some("rc-1234".into()), activity: Activity::Busy, ..session("remote-on", "feat/remote", None) },
    ]
}

/// Two closed sessions from the previews, and a Codex one.
fn closed() -> Vec<Remembered> {
    let mut closed = crate::controller::preview_closed(NOW);
    closed.push(Remembered {
        session_id: "x9".into(),
        name: "codex refactor".into(),
        project: "statusline".into(),
        cwd: "/x/x9".into(),
        branch: Some("feat/codex-refactor".into()),
        pr_number: None,
        base_pr: None,
        last_seen_ms: NOW - 5 * 60 * MIN,
        provider: Source::Codex,
    });
    closed
}

/// Usage for each provider: a month of history, a block in progress, both
/// tools' limits.
fn snapshots() -> Snapshots {
    let claude = snapshot(1.0, Some(claude_limits()), None);
    let codex = snapshot(0.25, None, Some(codex_limits()));
    let all = snapshot(1.25, Some(claude_limits()), Some(codex_limits()));
    Snapshots { claude, codex: Some(codex), all: Some(all) }
}

fn claude_limits() -> LimitsSnapshot {
    let window = |used_percent: f64, resets_in_min: i64| Some(LimitWindow { used_percent, resets_at_ms: Some(NOW + resets_in_min * MIN), window_minutes: None });
    LimitsSnapshot { captured_at_ms: NOW - 2 * MIN, five_hour: window(37.0, 140), seven_day: window(52.0, 3 * 24 * 60) }
}

fn codex_limits() -> LimitsSnapshot {
    LimitsSnapshot {
        captured_at_ms: NOW - 50 * MIN,
        five_hour: None,
        seven_day: Some(LimitWindow { used_percent: 9.0, resets_at_ms: Some(NOW + 5 * 24 * 60 * MIN), window_minutes: Some(10_080) }),
    }
}

/// A snapshot whose numbers are `scale` times a fixed pattern.
fn snapshot(scale: f64, rate_limits: Option<LimitsSnapshot>, codex_limits: Option<LimitsSnapshot>) -> Snapshot {
    let mut snapshot = Snapshot::empty(NOW);
    snapshot.generated_at_ms = NOW - 29_000;
    snapshot.record_count = sl_core::num::trunc_usize(433_498.0 * scale);
    let totals = |cost: f64, tokens: u64| Totals { cost: cost * scale, tokens: sl_core::num::trunc_u64(tokens as f64 * scale), input_tokens: 1_000, output_tokens: 2_000 };
    snapshot.today = totals(48.20, 61_000_000);
    snapshot.prev_day = totals(71.10, 90_000_000);
    snapshot.week = totals(312.40, 400_000_000);
    snapshot.prev_week = totals(280.00, 350_000_000);
    snapshot.month = totals(1_204.75, 1_500_000_000);
    snapshot.total_cost = 9_876.54 * scale;
    let model = |name: &str, cost: f64| ModelUsage { model: name.into(), cost: cost * scale, tokens: sl_core::num::trunc_u64(cost * 1_000_000.0), output_tokens: sl_core::num::trunc_u64(cost * 10_000.0), missing_pricing: false };
    snapshot.models = vec![model("claude-opus-5-5", 7_000.0), model("claude-sonnet-5-5", 2_500.0), model("claude-haiku-5-5", 376.54)];
    snapshot.current_block = Some(CurrentBlock { cost_so_far: 21.50 * scale, cost_per_hour: Some(12.45 * scale), end_time_ms: NOW + 140 * MIN, started_at_ms: NOW - 160 * MIN, tokens: 30_000_000 });
    let session = |id: &str, project: &str, cost: f64, ago_min: i64| LatestSession {
        session_id: id.into(),
        project: project.into(),
        cost: cost * scale,
        total_tokens: sl_core::num::trunc_u64(cost * 900_000.0),
        messages: sl_core::num::trunc_usize(cost * 3.0),
        first_seen_ms: NOW - (ago_min + 90) * MIN,
        last_activity_ms: NOW - ago_min * MIN,
    };
    snapshot.sessions = SessionStats {
        count: 214,
        avg_cost: 4.6 * scale,
        avg_tokens: 5_100_000,
        output_input_ratio: Some(0.42),
        cost_per_output_mtok: Some(31.5),
        most_efficient: None,
        latest: vec![session("s1", "Developer-statusline", 12.30, 3), session("s2", "Developer-datanaut", 48.10, 70), session("s3", "Developer-datanaut", 3.05, 400)],
        top: vec![session("s2", "Developer-datanaut", 48.10, 70), session("s1", "Developer-statusline", 12.30, 3), session("s3", "Developer-datanaut", 3.05, 400)],
    };
    snapshot.token_breakdown = vec![("cache read".into(), 9_000_000_000), ("cache write".into(), 600_000_000), ("output".into(), 40_000_000), ("input".into(), 2_000_000)];
    // A month of days, quieter at weekends, with the models in a fixed mix.
    let days = snapshot.history.len();
    for (i, day) in snapshot.history.iter_mut().enumerate() {
        let cost = (20.0 + ((i * 37) % 60) as f64 + if i % 7 >= 5 { -15.0 } else { 0.0 }) * scale;
        let recent = i + 30 >= days;
        let cost = if recent { cost } else { cost / 3.0 };
        day.cost = cost;
        day.tokens = sl_core::num::trunc_u64(cost * 1_200_000.0);
        day.messages = sl_core::num::trunc_u64(cost * 4.0);
        day.models = vec![
            DayModel { model: "claude-opus-5-5".into(), cost: cost * 0.7, tokens: sl_core::num::trunc_u64(cost * 800_000.0) },
            DayModel { model: "claude-sonnet-5-5".into(), cost: cost * 0.3, tokens: sl_core::num::trunc_u64(cost * 400_000.0) },
        ];
    }
    snapshot.rate_limits = rate_limits;
    snapshot.codex_limits = codex_limits;
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_loads_and_only_the_empty_one_has_no_sessions() {
        for name in ["full", "stale", "nolimits", "empty"] {
            let fixture = load(name).unwrap();
            assert_eq!(fixture.open.is_empty(), name == "empty", "{name}");
        }
        assert!(load("nope").is_err());
        assert!(load("stale").unwrap().snapshots.claude.rate_limits.unwrap().captured_at_ms < NOW - 60 * MIN);
        assert!(load("nolimits").unwrap().snapshots.claude.rate_limits.is_none());
    }

    #[test]
    fn the_full_fixture_has_every_state_the_plan_lists() {
        use sl_core::gh::CiState;
        let fixture = load("full").unwrap();
        let open = &fixture.open;
        assert!(open.iter().any(|s| s.activity == Activity::Busy), "working");
        assert!(open.iter().any(|s| s.cache.is_some_and(|c| c.compacted)), "compacted");
        assert!(open.iter().any(|s| s.cache.is_some_and(|c| c.remaining_ms(NOW).is_some_and(|left| left < 0))), "cold");
        let gh = |f: &dyn Fn(&sl_core::gh::GhInfo) -> bool| open.iter().any(|s| s.gh.as_ref().is_some_and(f));
        assert!(gh(&|g| g.ci.as_ref().is_some_and(|ci| ci.state == CiState::Failing)), "failed CI");
        assert!(gh(&|g| g.is_draft == Some(true)), "draft");
        assert!(gh(&|g| g.review_decision.as_deref() == Some("CHANGES_REQUESTED")), "changes requested");
        assert!(gh(&|g| g.review_decision.as_deref() == Some("APPROVED")), "approved");
        assert!(open.iter().any(|s| s.gh.is_none()), "no PR");
        assert!(open.iter().any(|s| s.linear.is_some()), "Linear");
        assert!(open.iter().any(|s| s.remote_control.is_some()), "/rc");
        assert!(open.iter().any(|s| s.provider == Source::Codex), "Codex");
        assert!(!fixture.stacks.is_empty() && !fixture.closed.is_empty());
    }
}
