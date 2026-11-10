//! A heads-up when an idle session's prompt cache is about to expire.
//!
//! Sent through `UNUserNotificationCenter`. A plain accessory binary has no
//! app bundle, which that API needs, so `build.rs` embeds an `Info.plist` that
//! gives the process a bundle identifier. There is no fallback: if the app is
//! not allowed to notify, nothing is shown.

use std::collections::{HashMap, HashSet};

use sl_core::gh::{GhInfo, ReviewState};

use crate::cache_ttl::CacheInfo;
use crate::view_prefs::NotifyPrefs;

const ONE_MINUTE_MS: i64 = 60_000;

/// How far ahead of expiry to warn: `long_mins` for a long (1h) cache; for a
/// five-minute cache that would be the moment of the message itself, so one
/// minute.
fn lead_ms(ttl_ms: i64, long_mins: u64) -> i64 {
    if ttl_ms >= 30 * ONE_MINUTE_MS {
        i64::try_from(long_mins).unwrap_or(i64::MAX) * ONE_MINUTE_MS
    } else {
        ONE_MINUTE_MS
    }
}

/// A session as the notifier needs to see it.
pub struct Watched<'a> {
    pub session_id: &'a str,
    pub name: &'a str,
    pub busy: bool,
    pub cache: Option<CacheInfo>,
}

/// Decides which warnings are due, each at most once per message.
#[derive(Default)]
pub struct Notifier {
    sent: HashSet<(String, i64)>,
}

impl Notifier {
    /// `(title, body)` for each idle session whose cache expires within its
    /// lead time and which has not been warned about since its last message.
    pub fn due(
        &mut self,
        sessions: &[Watched],
        now_ms: i64,
        long_lead_mins: u64,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for session in sessions {
            let Some(cache) = session.cache else { continue };
            let Some(remaining) = cache.remaining_ms(now_ms) else {
                continue;
            };
            let lead = cache.ttl_ms.map_or(0, |ttl| lead_ms(ttl, long_lead_mins));
            if session.busy || remaining <= 0 || remaining > lead {
                continue;
            }
            if self
                .sent
                .insert((session.session_id.to_owned(), cache.last_activity_ms))
            {
                let minutes = (remaining + ONE_MINUTE_MS - 1) / ONE_MINUTE_MS;
                out.push((
                    "Prompt cache expiring".to_owned(),
                    format!("{} — about {minutes} min left. The next message after that pays full price for the context.", session.name),
                ));
            }
        }
        // Forget sessions that have moved on, so the set cannot grow forever.
        self.sent.retain(|(id, at)| {
            sessions
                .iter()
                .any(|s| s.session_id == id && s.cache.is_some_and(|c| c.last_activity_ms == *at))
        });
        out
    }
}

/// A pull request as the watcher needs to see it.
pub struct PrWatch<'a> {
    pub project: &'a str,
    pub info: &'a GhInfo,
}

/// What was last seen of one PR.
struct PrSeen {
    reviews: HashMap<String, ReviewState>,
    /// When each reviewer last reviewed; `None` until an answer said, as
    /// answers stored before it was kept do not.
    latest: Option<HashMap<String, String>>,
    failed: HashSet<String>,
    /// CI has failed and not yet been seen green since. A re-run goes
    /// Failing, Pending (nothing failed yet), Passing, so `failed` alone
    /// forgets the failure at the Pending step.
    was_failing: bool,
    /// When it was seen.
    at_ms: i64,
}

/// How long a PR no pass has seen is still remembered. A pass can miss one
/// whose session is still open -- its branch unreadable mid-rebase, the store
/// busy -- and taking it back as a fresh baseline would swallow whatever
/// happened in between.
const PR_FORGET_MS: i64 = 10 * ONE_MINUTE_MS;

/// Whether the PR's checks have all settled green.
fn passing_now(info: &GhInfo) -> bool {
    info.ci
        .as_ref()
        .is_some_and(|ci| ci.state == sl_core::gh::CiState::Passing)
}

/// Announces a review that arrives or a CI job that starts failing on the PRs
/// of open sessions. The first sighting of a PR only records its state, so
/// starting the tray does not announce everything that already happened.
#[derive(Default)]
pub struct PrWatcher {
    /// By the PR's URL (its project's name where it has none) and number:
    /// two repositories can share a directory name and PR numbers.
    seen: HashMap<(String, i64), PrSeen>,
}

fn review_phrase(state: ReviewState) -> Option<&'static str> {
    match state {
        ReviewState::Approved => Some("approved"),
        ReviewState::ChangesRequested => Some("requested changes on"),
        ReviewState::Commented => Some("commented on"),
        ReviewState::Dismissed | ReviewState::Other => None,
    }
}

impl PrWatcher {
    /// `(title, body)` for each review given since the last sighting and
    /// each CI job that was not failing then and is now, of the
    /// kinds `wanted` asks for. Every sighting is still recorded, so switching
    /// a kind back on does not replay what happened while it was off.
    pub fn due(
        &mut self,
        prs: &[PrWatch],
        wanted: NotifyPrefs,
        now_ms: i64,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut live = HashSet::new();
        for pr in prs {
            let Some(number) = pr.info.pr_number else {
                continue;
            };
            let key = (
                pr.info.url.clone().unwrap_or_else(|| pr.project.to_owned()),
                number,
            );
            if !live.insert(key.clone()) {
                continue; // several sessions on one PR
            }
            let reviews: HashMap<String, ReviewState> = pr
                .info
                .reviews
                .iter()
                .map(|r| (r.author.clone(), r.state))
                .collect();
            // An answer with reviews and no times predates them being kept.
            let latest: Option<HashMap<String, String>> =
                (!pr.info.latest_reviews.is_empty() || pr.info.reviews.is_empty()).then(|| {
                    pr.info
                        .latest_reviews
                        .iter()
                        .map(|r| (r.author.clone(), r.submitted_at.clone()))
                        .collect()
                });
            let failed: HashSet<String> = pr
                .info
                .ci
                .iter()
                .flat_map(|ci| ci.failed.iter().map(|job| job.name.clone()))
                .collect();
            if let Some(before) = self.seen.get(&key) {
                // A reviewer who has reviewed again since, with that review
                // as given (a comment after an approval is a comment); without
                // times on both sightings, or for them, one whose standing
                // changed.
                let mut authors: Vec<(&str, ReviewState)> = match (&before.latest, &latest) {
                    (Some(was), Some(_)) => pr
                        .info
                        .latest_reviews
                        .iter()
                        .filter(|r| match was.get(&r.author) {
                            Some(at) => *at != r.submitted_at,
                            None => before.reviews.get(&r.author) != reviews.get(&r.author),
                        })
                        .map(|r| (r.author.as_str(), r.state))
                        .collect(),
                    _ => reviews
                        .iter()
                        .filter(|(a, s)| before.reviews.get(*a) != Some(*s))
                        .map(|(a, s)| (a.as_str(), *s))
                        .collect(),
                };
                authors.sort_by_key(|(author, _)| *author);
                for (author, state) in authors {
                    let wanted_state = match state {
                        ReviewState::ChangesRequested => wanted.review_changes,
                        ReviewState::Approved => wanted.review_approved,
                        ReviewState::Commented => wanted.review_commented,
                        ReviewState::Dismissed | ReviewState::Other => false,
                    };
                    if let Some(phrase) = review_phrase(state).filter(|_| wanted_state) {
                        out.push((
                            format!("{} #{number}", pr.project),
                            format!("{author} {phrase} your PR."),
                        ));
                    }
                }
                let mut fresh: Vec<_> = failed.difference(&before.failed).collect();
                fresh.sort();
                for job in fresh.into_iter().filter(|_| wanted.ci_failures) {
                    out.push((
                        format!("{} #{number}: CI failed", pr.project),
                        format!("{job} failed."),
                    ));
                }
                // Everything that was failing is not, and the checks have
                // settled green: no pending run is "recovered" yet.
                let passing = passing_now(pr.info);
                if wanted.ci_recovered && before.was_failing && failed.is_empty() && passing {
                    out.push((
                        format!("{} #{number}: CI passes", pr.project),
                        "The failing checks are green again.".to_owned(),
                    ));
                }
            }
            let was_failing = !failed.is_empty()
                || (self.seen.get(&key).is_some_and(|before| before.was_failing)
                    && !passing_now(pr.info));
            // An answer without times leaves the last ones standing.
            let latest =
                latest.or_else(|| self.seen.get(&key).and_then(|before| before.latest.clone()));
            self.seen.insert(
                key,
                PrSeen {
                    reviews,
                    latest,
                    failed,
                    was_failing,
                    at_ms: now_ms,
                },
            );
        }
        // A PR unseen for a while (its session closed) is forgotten, so
        // reopening it is a fresh baseline.
        self.seen
            .retain(|key, seen| live.contains(key) || now_ms - seen.at_ms < PR_FORGET_MS);
        out
    }
}

/// Announces a PR in a shown stack that starts needing a rebase -- typically
/// once a PR below it merges. As with [`PrWatcher`], the first sighting of a
/// PR only records it, so starting the tray announces nothing already true,
/// and a PR is announced once per time it starts needing one.
#[derive(Default)]
pub struct StackWatcher {
    /// Every PR in a shown stack, and whether it needed a rebase when last
    /// seen.
    seen: HashMap<StackPr, bool>,
    /// Likewise whether it was merged.
    merged: HashMap<StackPr, bool>,
}

/// A PR in a stack: its repository, its stack's name and its number. Two
/// repositories can each have a `Stack #1` with a `#212` in it.
type StackPr = (Option<std::path::PathBuf>, String, i64);

impl StackWatcher {
    /// `(title, body)` for each PR that needs a rebase now and did not when
    /// last seen, and each that has been merged since; a kind that is off
    /// announces nothing, though every PR is still recorded, so switching it
    /// on does not replay the past.
    pub fn due(
        &mut self,
        stacks: &[crate::stacks::StackView],
        rebase_wanted: bool,
        merged_wanted: bool,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut live = HashSet::new();
        for stack in stacks {
            let name = stack
                .number
                .map_or_else(|| "PR chain".to_owned(), |n| format!("Stack #{n}"));
            for entry in &stack.entries {
                let key = (stack.repo.clone(), name.clone(), entry.number);
                live.insert(key.clone());
                // Not yet settled (GitHub still computing it): no news either
                // way, so what was last seen stands.
                if !entry.checked {
                    continue;
                }
                let needs = entry.state == crate::stacks::EntryState::NeedsRebase;
                if rebase_wanted && needs && self.seen.get(&key) == Some(&false) {
                    out.push((
                        format!("{name}: #{} needs a rebase", entry.number),
                        entry.branch.clone().unwrap_or_else(|| {
                            "It conflicts with or is behind its base.".to_owned()
                        }),
                    ));
                }
                let merged = entry.state == crate::stacks::EntryState::Merged;
                if merged_wanted && merged && self.merged.get(&key) == Some(&false) {
                    out.push((
                        format!("{name}: #{} merged", entry.number),
                        entry
                            .branch
                            .clone()
                            .unwrap_or_else(|| "It was merged.".to_owned()),
                    ));
                }
                self.seen.insert(key.clone(), needs);
                self.merged.insert(key, merged);
            }
        }
        self.seen.retain(|key, _| live.contains(key));
        self.merged.retain(|key, _| live.contains(key));
        out
    }
}

/// Show a notification, off the calling thread: the permission prompt can sit
/// on screen for a long time and nothing should wait on it.
pub fn send(title: &str, body: &str) {
    if crate::effects::intercept(crate::effects::Effect::Notify {
        title: title.to_owned(),
        body: body.to_owned(),
    }) {
        return;
    }
    let (title, body) = (title.to_owned(), body.to_owned());
    std::thread::Builder::new()
        .name("sl-tray-notify".to_owned())
        .spawn(move || {
            crate::trace(&format!("notification: {}", send_blocking(&title, &body)));
        })
        .ok();
}

/// Notifications sent so far by this process.
static SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A notification's identifier, never one sent before: a request with an
/// identifier already shown replaces that notification, and several sent in
/// one pass share their millisecond (all of them do under `SL_TRAY_NOW`).
fn request_id(now_ms: i64) -> String {
    let n = SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("sl-tray-{now_ms}-{n}")
}

/// Deliver through `UNUserNotificationCenter`. A user who said no to
/// notifications gets none. Returns what happened, for `--test-notification`.
pub fn send_blocking(title: &str, body: &str) -> String {
    match sl_macos::notify::send(&request_id(crate::snapshot::now_ms()), title, body) {
        Ok(()) => "delivered".to_owned(),
        Err(sl_macos::notify::Error::Denied) => {
            "notifications are turned off for this app in System Settings".to_owned()
        }
        Err(sl_macos::notify::Error::NoIdentity) => {
            "not delivered: the process has no bundle identifier".to_owned()
        }
        Err(sl_macos::notify::Error::Failed(why)) => format!("not delivered: {why}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every notification kind as it is before anything is chosen.
    fn defaults() -> NotifyPrefs {
        NotifyPrefs::from_array(crate::view_prefs::NOTIFY_KINDS.map(|(_, _, on)| on))
    }

    fn watched(id: &str, busy: bool, last: i64, ttl: i64) -> Watched<'_> {
        Watched {
            session_id: id,
            name: id,
            busy,
            cache: Some(CacheInfo {
                last_activity_ms: last,
                ttl_ms: Some(ttl),
                compacted: false,
            }),
        }
    }

    const HOUR: i64 = 60 * ONE_MINUTE_MS;

    fn stack(states: &[(i64, crate::stacks::EntryState)]) -> Vec<crate::stacks::StackView> {
        stack_checked(
            &states
                .iter()
                .map(|(n, s)| (*n, *s, true))
                .collect::<Vec<_>>(),
        )
    }

    /// As [`stack`], each PR also saying whether its state is settled.
    fn stack_checked(
        states: &[(i64, crate::stacks::EntryState, bool)],
    ) -> Vec<crate::stacks::StackView> {
        let entries = states
            .iter()
            .map(|(number, state, checked)| crate::stacks::StackEntry {
                number: *number,
                branch: Some(format!("b{number}")),
                state: *state,
                session: crate::stacks::EntrySession::None,
                checked: *checked,
            })
            .collect();
        vec![crate::stacks::StackView {
            number: Some(746),
            repo: None,
            repo_url: None,
            entries,
        }]
    }

    #[test]
    fn a_stack_pr_is_announced_once_when_it_starts_needing_a_rebase() {
        use crate::stacks::EntryState::{Merged, NeedsRebase, Open};
        let mut watcher = StackWatcher::default();
        assert!(
            watcher
                .due(
                    &stack(&[(752, Open), (753, Open), (747, NeedsRebase)]),
                    true,
                    false
                )
                .is_empty(),
            "what is true at launch is not news"
        );
        let due = watcher.due(
            &stack(&[(752, Merged), (753, NeedsRebase), (747, NeedsRebase)]),
            true,
            false,
        );
        assert_eq!(
            due,
            [(
                "Stack #746: #753 needs a rebase".to_owned(),
                "b753".to_owned()
            )],
            "the one that changed, once"
        );
        assert!(
            watcher
                .due(&stack(&[(752, Merged), (753, NeedsRebase)]), true, false)
                .is_empty(),
            "not again while it stays so"
        );
        watcher.due(&stack(&[(752, Merged), (753, Open)]), true, false);
        assert_eq!(
            watcher
                .due(&stack(&[(752, Merged), (753, NeedsRebase)]), true, false)
                .len(),
            1,
            "rebased, then behind again: news again"
        );
    }

    #[test]
    fn github_still_computing_is_no_news_either_way() {
        use crate::stacks::EntryState::{NeedsRebase, Open};
        let mut watcher = StackWatcher::default();
        watcher.due(&stack(&[(753, NeedsRebase)]), true, false);
        // GitHub recomputes after a push to the base: the PR keeps its last
        // known state in the menu, unsettled.
        assert!(
            watcher
                .due(&stack_checked(&[(753, NeedsRebase, false)]), true, false)
                .is_empty()
        );
        assert!(
            watcher
                .due(&stack_checked(&[(753, Open, false)]), true, false)
                .is_empty(),
            "even shown as open while unsettled"
        );
        assert!(
            watcher
                .due(&stack(&[(753, NeedsRebase)]), true, false)
                .is_empty(),
            "it never stopped needing one"
        );

        watcher.due(&stack(&[(753, Open)]), true, false);
        assert_eq!(
            watcher
                .due(&stack(&[(753, NeedsRebase)]), true, false)
                .len(),
            1,
            "a settled clean state between does make it news"
        );

        let mut fresh = StackWatcher::default();
        assert!(
            fresh
                .due(&stack_checked(&[(800, Open, false)]), true, false)
                .is_empty()
        );
        assert!(
            fresh
                .due(&stack(&[(800, NeedsRebase)]), true, false)
                .is_empty(),
            "first seen unsettled: its first settled state is a baseline, not news"
        );
    }

    #[test]
    fn same_named_stacks_in_two_repositories_are_watched_apart() {
        use crate::stacks::EntryState::{NeedsRebase, Open};
        let in_repo = |repo: &str, state| {
            let mut stacks = stack(&[(212, state)]);
            stacks[0].repo = Some(repo.into());
            stacks.remove(0)
        };
        let mut watcher = StackWatcher::default();
        let both = [in_repo("/a/.git", NeedsRebase), in_repo("/b/.git", Open)];
        watcher.due(&both, true, false);
        for _ in 0..3 {
            assert!(
                watcher.due(&both, true, false).is_empty(),
                "A's #212 has needed one all along; B's never has"
            );
        }
    }

    #[test]
    fn same_numbered_prs_of_same_named_projects_are_watched_apart() {
        let mut watcher = PrWatcher::default();
        let at = |repo: &str, reviews: &[(&str, &str)]| GhInfo {
            url: Some(format!("https://github.com/{repo}/pull/7")),
            ..pr_info(reviews, &[])
        };
        let (a, b) = (at("a/app", &[]), at("b/app", &[]));
        let watch = |a, b| {
            [
                PrWatch {
                    project: "app",
                    info: a,
                },
                PrWatch {
                    project: "app",
                    info: b,
                },
            ]
        };
        watcher.due(&watch(&a, &b), defaults(), 0);
        let reviewed = at("b/app", &[("marc", "APPROVED")]);
        assert_eq!(
            watcher.due(&watch(&a, &reviewed), defaults(), 0).len(),
            1,
            "B's review is not lost behind A's PR"
        );
    }

    #[test]
    fn a_stack_rebase_while_switched_off_is_recorded_not_replayed() {
        use crate::stacks::EntryState::{NeedsRebase, Open};
        let mut watcher = StackWatcher::default();
        watcher.due(&stack(&[(753, Open)]), false, false);
        assert!(
            watcher
                .due(&stack(&[(753, NeedsRebase)]), false, false)
                .is_empty()
        );
        assert!(
            watcher
                .due(&stack(&[(753, NeedsRebase)]), true, false)
                .is_empty(),
            "switched on later: no replay"
        );
    }

    #[test]
    fn warns_once_inside_the_lead_time_for_idle_sessions_only() {
        let mut notifier = Notifier::default();
        let s = [watched("a", false, 0, HOUR)];
        assert!(
            notifier.due(&s, 54 * ONE_MINUTE_MS, 5).is_empty(),
            "6 min left is too early"
        );
        let due = notifier.due(&s, 56 * ONE_MINUTE_MS, 5);
        assert_eq!(due.len(), 1);
        assert!(due[0].1.contains("about 4 min left"));
        assert!(
            notifier.due(&s, 57 * ONE_MINUTE_MS, 5).is_empty(),
            "once per message"
        );
        assert!(
            notifier.due(&s, 61 * ONE_MINUTE_MS, 5).is_empty(),
            "already expired"
        );
        assert!(
            Notifier::default()
                .due(&[watched("b", true, 0, HOUR)], 56 * ONE_MINUTE_MS, 5)
                .is_empty(),
            "busy sessions keep it warm"
        );
    }

    #[test]
    fn a_new_message_earns_a_new_warning_and_short_caches_warn_at_one_minute() {
        let mut notifier = Notifier::default();
        assert_eq!(
            notifier
                .due(&[watched("a", false, 0, HOUR)], 56 * ONE_MINUTE_MS, 5)
                .len(),
            1
        );
        let later = 3 * HOUR;
        assert_eq!(
            notifier
                .due(
                    &[watched("a", false, later, HOUR)],
                    later + 56 * ONE_MINUTE_MS,
                    5
                )
                .len(),
            1
        );
        let five = 5 * ONE_MINUTE_MS;
        let mut notifier = Notifier::default();
        assert!(
            notifier
                .due(&[watched("c", false, 0, five)], 3 * ONE_MINUTE_MS, 5)
                .is_empty()
        );
        assert_eq!(
            notifier
                .due(
                    &[watched("c", false, 0, five)],
                    4 * ONE_MINUTE_MS + 10_000,
                    5
                )
                .len(),
            1
        );
    }

    fn pr_info(reviews: &[(&str, &str)], failed: &[&str]) -> GhInfo {
        use sl_core::gh::{Ci, CiState, FailedJob, Review, ReviewState};
        GhInfo {
            pr_number: Some(7),
            // As `gh`'s output is parsed: a pending review is no review at all.
            reviews: reviews
                .iter()
                .filter_map(|(a, s)| {
                    Some(Review {
                        author: (*a).into(),
                        state: ReviewState::from_api(s)?,
                    })
                })
                .collect(),
            ci: Some(Ci {
                state: if failed.is_empty() {
                    CiState::Passing
                } else {
                    CiState::Failing
                },
                failed: failed
                    .iter()
                    .map(|n| FailedJob {
                        name: (*n).into(),
                        link: None,
                    })
                    .collect(),
                pending: 0,
            }),
            ..GhInfo::default()
        }
    }

    #[test]
    fn a_new_review_or_failing_job_is_announced_once_after_a_silent_first_sighting() {
        let mut watcher = PrWatcher::default();
        let due = |w: &mut PrWatcher, info: &GhInfo| {
            w.due(
                &[PrWatch {
                    project: "sl",
                    info,
                }],
                defaults(),
                0,
            )
        };
        assert!(
            due(&mut watcher, &pr_info(&[("marc", "COMMENTED")], &["lint"])).is_empty(),
            "baseline"
        );
        assert!(
            due(&mut watcher, &pr_info(&[("marc", "COMMENTED")], &["lint"])).is_empty(),
            "nothing changed"
        );

        let news = due(
            &mut watcher,
            &pr_info(
                &[("marc", "APPROVED"), ("claude", "COMMENTED")],
                &["lint", "test"],
            ),
        );
        let bodies: Vec<_> = news.iter().map(|(_, b)| b.as_str()).collect();
        assert_eq!(
            bodies,
            [
                "claude commented on your PR.",
                "marc approved your PR.",
                "test failed."
            ]
        );
        assert_eq!(news[0].0, "sl #7");
        assert_eq!(news[2].0, "sl #7: CI failed");
        assert!(
            due(
                &mut watcher,
                &pr_info(
                    &[("marc", "APPROVED"), ("claude", "COMMENTED")],
                    &["lint", "test"]
                )
            )
            .is_empty()
        );

        // Fixed, then broken again: that is news.
        assert!(
            due(
                &mut watcher,
                &pr_info(&[("marc", "APPROVED"), ("claude", "COMMENTED")], &[])
            )
            .is_empty()
        );
        assert_eq!(
            due(
                &mut watcher,
                &pr_info(&[("marc", "APPROVED"), ("claude", "COMMENTED")], &["lint"])
            )
            .len(),
            1
        );
    }

    /// As [`pr_info`], with each reviewer's latest review and its time.
    fn timed(standing: &[(&str, &str)], latest: &[(&str, &str, &str)]) -> GhInfo {
        GhInfo {
            latest_reviews: latest
                .iter()
                .map(|(author, state, at)| sl_core::gh::LatestReview {
                    author: (*author).into(),
                    state: ReviewState::from_api(state).unwrap(),
                    submitted_at: (*at).into(),
                })
                .collect(),
            ..pr_info(standing, &[])
        }
    }

    #[test]
    fn a_reviewer_reviewing_again_is_news_each_time_as_the_review_was_given() {
        let mut watcher = PrWatcher::default();
        let mut due = |info: &GhInfo| {
            watcher
                .due(
                    &[PrWatch {
                        project: "sl",
                        info,
                    }],
                    defaults(),
                    0,
                )
                .into_iter()
                .map(|(_, body)| body)
                .collect::<Vec<_>>()
        };
        let changes = [("marc", "CHANGES_REQUESTED")];
        assert!(due(&timed(&changes, &[("marc", "CHANGES_REQUESTED", "t1")])).is_empty());
        assert_eq!(
            due(&timed(&changes, &[("marc", "CHANGES_REQUESTED", "t2")])),
            ["marc requested changes on your PR."],
            "changes requested again, after a fix"
        );
        assert!(due(&timed(&changes, &[("marc", "CHANGES_REQUESTED", "t2")])).is_empty());
        let approved = [("marc", "APPROVED")];
        assert_eq!(
            due(&timed(&approved, &[("marc", "APPROVED", "t3")])),
            ["marc approved your PR."]
        );
        assert_eq!(
            due(&timed(&approved, &[("marc", "COMMENTED", "t4")])),
            ["marc commented on your PR."],
            "a comment after the approval, which still stands"
        );
    }

    #[test]
    fn an_answer_without_review_times_neither_announces_nor_forgets_them() {
        let mut watcher = PrWatcher::default();
        let mut due = |info: &GhInfo| {
            watcher.due(
                &[PrWatch {
                    project: "sl",
                    info,
                }],
                defaults(),
                0,
            )
        };
        let changes = [("marc", "CHANGES_REQUESTED")];
        due(&timed(&changes, &[("marc", "CHANGES_REQUESTED", "t1")]));
        assert!(
            due(&pr_info(&changes, &[])).is_empty(),
            "stored before times"
        );
        assert!(
            due(&timed(&changes, &[("marc", "CHANGES_REQUESTED", "t1")])).is_empty(),
            "the same review, not replayed"
        );
        assert_eq!(
            due(&timed(&changes, &[("marc", "CHANGES_REQUESTED", "t2")])).len(),
            1
        );

        // A first review that arrives in an answer without times is news
        // once, not again when its time is first seen.
        let mut watcher = PrWatcher::default();
        let mut due = |info: &GhInfo| {
            watcher.due(
                &[PrWatch {
                    project: "sl",
                    info,
                }],
                defaults(),
                0,
            )
        };
        due(&timed(&[], &[]));
        assert_eq!(due(&pr_info(&changes, &[])).len(), 1);
        assert!(due(&timed(&changes, &[("marc", "CHANGES_REQUESTED", "t1")])).is_empty());
    }

    #[test]
    fn a_closed_session_resets_its_pr_and_pending_reviews_are_ignored() {
        let mut watcher = PrWatcher::default();
        let info = pr_info(&[("marc", "PENDING")], &[]);
        assert!(
            watcher
                .due(
                    &[PrWatch {
                        project: "sl",
                        info: &info
                    }],
                    defaults(),
                    0
                )
                .is_empty()
        );
        assert!(watcher.due(&[], defaults(), PR_FORGET_MS).is_empty());
        let later = pr_info(&[("marc", "APPROVED")], &[]);
        assert!(
            watcher
                .due(
                    &[PrWatch {
                        project: "sl",
                        info: &later
                    }],
                    defaults(),
                    PR_FORGET_MS
                )
                .is_empty(),
            "back after a long gap is a new baseline"
        );
    }

    #[test]
    fn a_pr_missing_from_a_pass_or_two_keeps_its_baseline() {
        let mut watcher = PrWatcher::default();
        let mut due = |info: Option<&GhInfo>, now_ms: i64| {
            let prs: Vec<PrWatch> = info
                .into_iter()
                .map(|info| PrWatch {
                    project: "sl",
                    info,
                })
                .collect();
            watcher.due(&prs, defaults(), now_ms)
        };
        due(Some(&pr_info(&[], &[])), 0);
        // Mid-rebase, its branch cannot be read: no PR for a minute.
        assert!(due(None, 30_000).is_empty());
        assert!(due(None, 60_000).is_empty());
        let news = due(
            Some(&pr_info(&[("marc", "CHANGES_REQUESTED")], &["lint"])),
            90_000,
        );
        assert_eq!(news.len(), 2, "what came meanwhile is news: {news:?}");
    }

    fn prefs(on: &[usize]) -> NotifyPrefs {
        NotifyPrefs::from_array(std::array::from_fn(|i| on.contains(&i)))
    }

    #[test]
    fn each_review_outcome_is_its_own_switch() {
        let mut watcher = PrWatcher::default();
        let due = |w: &mut PrWatcher, info: &GhInfo, wanted: &NotifyPrefs| {
            w.due(
                &[PrWatch {
                    project: "sl",
                    info,
                }],
                *wanted,
                0,
            )
        };
        let only_changes = prefs(&[1]);
        assert!(
            due(&mut watcher, &pr_info(&[], &[]), &only_changes).is_empty(),
            "baseline"
        );
        let news = due(
            &mut watcher,
            &pr_info(
                &[
                    ("a", "APPROVED"),
                    ("b", "CHANGES_REQUESTED"),
                    ("c", "COMMENTED"),
                ],
                &[],
            ),
            &only_changes,
        );
        assert_eq!(
            news.iter().map(|(_, b)| b.as_str()).collect::<Vec<_>>(),
            ["b requested changes on your PR."]
        );
        // What was skipped while off is not replayed once it is on.
        assert!(
            due(
                &mut watcher,
                &pr_info(
                    &[
                        ("a", "APPROVED"),
                        ("b", "CHANGES_REQUESTED"),
                        ("c", "COMMENTED")
                    ],
                    &[]
                ),
                &prefs(&[1, 2, 3])
            )
            .is_empty()
        );
    }

    #[test]
    fn ci_recovering_is_announced_once_and_only_when_green() {
        let mut watcher = PrWatcher::default();
        let due = |w: &mut PrWatcher, info: &GhInfo| {
            w.due(
                &[PrWatch {
                    project: "sl",
                    info,
                }],
                prefs(&[5]),
                0,
            )
        };
        assert!(
            due(&mut watcher, &pr_info(&[], &["lint"])).is_empty(),
            "baseline"
        );
        let news = due(&mut watcher, &pr_info(&[], &[]));
        assert_eq!(news.len(), 1);
        assert_eq!(news[0].0, "sl #7: CI passes");
        assert!(due(&mut watcher, &pr_info(&[], &[])).is_empty(), "once");
        due(&mut watcher, &pr_info(&[], &["lint"]));
        let mut pending = pr_info(&[], &[]);
        pending.ci = Some(sl_core::gh::Ci {
            state: sl_core::gh::CiState::Pending,
            failed: vec![],
            pending: 1,
        });
        assert!(
            due(&mut watcher, &pending).is_empty(),
            "a rerun in progress has not recovered yet"
        );
        assert!(
            due(&mut watcher, &pending).is_empty(),
            "nor while it keeps running"
        );
        let news = due(&mut watcher, &pr_info(&[], &[]));
        assert_eq!(news.len(), 1, "Failing, Pending, Passing: recovered, once");
        assert!(due(&mut watcher, &pr_info(&[], &[])).is_empty());
    }

    #[test]
    fn a_merged_stack_pr_is_announced_once_and_only_when_wanted() {
        use crate::stacks::EntryState::{Merged, Open};
        let mut watcher = StackWatcher::default();
        assert!(
            watcher
                .due(&stack(&[(752, Open), (753, Open)]), false, true)
                .is_empty(),
            "baseline"
        );
        let due = watcher.due(&stack(&[(752, Merged), (753, Open)]), false, true);
        assert_eq!(due.len(), 1);
        assert!(due[0].0.ends_with("#752 merged"), "{due:?}");
        assert!(
            watcher
                .due(&stack(&[(752, Merged), (753, Open)]), false, true)
                .is_empty(),
            "once"
        );
        let mut off = StackWatcher::default();
        off.due(&stack(&[(752, Open)]), false, false);
        off.due(&stack(&[(752, Merged)]), false, false);
        assert!(
            off.due(&stack(&[(752, Merged)]), false, true).is_empty(),
            "switched on later: no replay"
        );
    }

    #[test]
    fn notifications_sent_in_the_same_millisecond_have_their_own_ids() {
        assert_ne!(request_id(5), request_id(5));
    }

    #[test]
    fn the_lead_time_is_the_setting_for_long_caches_and_a_minute_for_short_ones() {
        let hour = 60 * ONE_MINUTE_MS;
        let mut notifier = Notifier::default();
        let s = [watched("a", false, 0, hour)];
        assert!(
            notifier.due(&s, 49 * ONE_MINUTE_MS, 10).is_empty(),
            "11 min left, 10 min lead"
        );
        assert_eq!(notifier.due(&s, 51 * ONE_MINUTE_MS, 10).len(), 1);
        assert_eq!(
            lead_ms(5 * ONE_MINUTE_MS, 15),
            ONE_MINUTE_MS,
            "a five-minute cache is never warned early"
        );
        assert_eq!(lead_ms(hour, 15), 15 * ONE_MINUTE_MS);
    }
}
