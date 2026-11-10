//! The tray's settings: the layout (a menu, or a compact popover of cards),
//! how the usage meters are drawn (bars or rings), and which notifications
//! are sent. Chosen in the Settings window and kept across launches in the
//! user defaults, beside the "Auto caffeinate" switch.

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSString, NSUserDefaults};

/// The environment variable naming a defaults suite to keep the settings in
/// instead of the app's own, for the self-test and previews: they must never
/// read or change the settings of the tray in actual use.
pub(crate) const SUITE_ENV: &str = "SL_TRAY_DEFAULTS_SUITE";

/// The suite named by [`SUITE_ENV`], if any.
pub(crate) fn suite() -> Option<String> {
    #[cfg(test)]
    if let Some(name) = tests::SUITE.with(|suite| suite.borrow().clone()) {
        return Some(name);
    }
    std::env::var(SUITE_ENV)
        .ok()
        .filter(|name| !name.is_empty())
}

/// Where every setting is kept: the standard user defaults, or the suite
/// named by [`SUITE_ENV`]. A suite that cannot be opened ends the process
/// rather than falling back to the real settings.
#[expect(
    clippy::print_stderr,
    clippy::exit,
    reason = "never fall back to the real settings: say why and stop"
)]
pub(crate) fn defaults() -> Retained<NSUserDefaults> {
    let Some(name) = suite() else {
        return NSUserDefaults::standardUserDefaults();
    };
    if let Some(defaults) =
        NSUserDefaults::initWithSuiteName(NSUserDefaults::alloc(), Some(&NSString::from_str(&name)))
    {
        defaults
    } else {
        eprintln!("sl-tray: cannot open the defaults suite {name:?}");
        std::process::exit(2);
    }
}

/// Empty the suite named `name` and write it out at once, so nothing is left
/// to write to it after the process exits (its emptied file is then the
/// caller's to delete).
pub(crate) fn discard_suite(name: &str) {
    let defaults = defaults();
    defaults.removePersistentDomainForName(&NSString::from_str(name));
    defaults.synchronize();
}

/// A setting with a fixed set of values, stored by name: `ALL` (in the order
/// Settings offers them), `label` (as Settings shows it), `key` (as it is
/// stored) and `parse` (a stored name back; `None` for anything else).
macro_rules! stored_choice {
    ($ty:ident { $($variant:ident => $key:literal, $label:literal;)+ }) => {
        impl $ty {
            pub const ALL: [$ty; [$($key),+].len()] = [$($ty::$variant),+];

            pub fn label(self) -> &'static str {
                match self {
                    $($ty::$variant => $label),+
                }
            }

            pub(crate) fn key(self) -> &'static str {
                match self {
                    $($ty::$variant => $key),+
                }
            }

            pub(crate) fn parse(text: &str) -> Option<Self> {
                Self::ALL.into_iter().find(|value| value.key() == text)
            }
        }
    };
}

/// Which surface the menu bar icon opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// A native menu: sessions grouped by what needs attention, two lines
    /// each, details and actions in each session's submenu.
    #[default]
    Regular,
    /// A popover of one-line cards; a card unfolds in place to show its PR
    /// and the actions on it.
    Compact,
}

/// How the usage meters are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MeterStyle {
    /// A bar per window: usage, a fainter fill to where it is heading, and a
    /// tick where an even pace would be.
    #[default]
    Bars,
    /// A ring per window, the numbers beside it.
    Rings,
}

/// Whose usage a view shows. The meters and the menu bar glyph show Claude
/// Code's or Codex's (the footer's switch); the Stats window may show both
/// (its toolbar). Codex is offered only when a Codex home exists; without one
/// the tray is Claude Code's alone, whatever was saved. Sessions are listed
/// whichever is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Provider {
    #[default]
    Claude,
    Codex,
    /// Both, merged: spend summed, every window's meter, every session.
    All,
}

stored_choice!(Provider {
    Claude => "claude", "Claude Code";
    Codex => "codex", "Codex";
    All => "all", "All";
});

impl Provider {
    /// Whether a session or a snapshot of `source` is shown under this choice.
    pub fn shows(self, source: Source) -> bool {
        match self {
            Provider::All => true,
            Provider::Claude => source == Source::Claude,
            Provider::Codex => source == Source::Codex,
        }
    }
}

pub use sl_usage::Source;

const LAYOUT_KEY: &str = "layout";
const METERS_KEY: &str = "meterStyle";
const METER_PROVIDER_KEY: &str = "meterProvider";
const STATS_PROVIDER_KEY: &str = "statsProvider";

/// Whether a Codex home exists, and so whether Codex and All are on offer.
pub fn codex_available() -> bool {
    sl_usage::codex::codex_home().is_some()
}

/// Whether the session rows say whose they are: only when both Claude Code
/// and Codex are installed. With one tool, a logo on every row says nothing.
pub fn show_marks() -> bool {
    codex_available()
        && dirs::home_dir().is_some_and(|home| {
            sl_core::paths::claude_config_dirs(&home)
                .iter()
                .any(|dir| dir.is_dir())
        })
}

/// Whose limits the meters and the glyph show: Claude Code's, unless Codex is
/// installed and was chosen. Never All.
pub fn load_meter_provider() -> Provider {
    match read(METER_PROVIDER_KEY)
        .as_deref()
        .and_then(Provider::parse)
    {
        Some(Provider::Codex) if codex_available() => Provider::Codex,
        _ => Provider::Claude,
    }
}

pub fn save_meter_provider(provider: Provider) {
    write(METER_PROVIDER_KEY, provider.key());
}

/// Whose spend the Stats window shows: Claude Code's unless Codex is installed
/// and Codex or All was chosen.
pub fn load_stats_provider() -> Provider {
    if !codex_available() {
        return Provider::Claude;
    }
    read(STATS_PROVIDER_KEY)
        .as_deref()
        .and_then(Provider::parse)
        .unwrap_or_default()
}

pub fn save_stats_provider(provider: Provider) {
    write(STATS_PROVIDER_KEY, provider.key());
}

stored_choice!(Layout {
    Regular => "regular", "Regular";
    Compact => "compact", "Compact";
});

stored_choice!(MeterStyle {
    Bars => "bars", "Bars";
    Rings => "rings", "Rings";
});

fn read(key: &str) -> Option<String> {
    crate::view_prefs::defaults()
        .stringForKey(&NSString::from_str(key))
        .map(|value| value.to_string())
}

/// A string kept in the user defaults under `key`, for state other modules keep.
pub(crate) fn read_string(key: &str) -> Option<String> {
    read(key)
}

pub(crate) fn write_string(key: &str, value: &str) {
    write(key, value);
}

fn write(key: &str, value: &str) {
    sl_macos::system::defaults_set_string(&defaults(), key, value);
    crate::sessions::wake();
}

fn write_bool(key: &str, on: bool) {
    defaults().setBool_forKey(on, &NSString::from_str(key));
    crate::sessions::wake();
}

/// The saved layout; Regular when none was chosen or the value is unknown.
pub fn load_layout() -> Layout {
    read(LAYOUT_KEY)
        .as_deref()
        .and_then(Layout::parse)
        .unwrap_or_default()
}

pub fn save_layout(layout: Layout) {
    write(LAYOUT_KEY, layout.key());
}

/// The saved meter style; Bars when none was chosen or the value is unknown.
pub fn load_meters() -> MeterStyle {
    read(METERS_KEY)
        .as_deref()
        .and_then(MeterStyle::parse)
        .unwrap_or_default()
}

pub fn save_meters(style: MeterStyle) {
    write(METERS_KEY, style.key());
}

/// The on/off settings, each on until switched off in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    /// The spend line (`$21.50 so far · $12.45/hr · $60.10 projected`) under
    /// the meters.
    ShowSpend,
    /// A session with `/rc` on keeps the Mac awake, busy or idle.
    RcAwake,
    /// A session on a ticket's branch shows the ticket's state, and can open
    /// it in Linear.
    ShowLinear,
    /// The menu bar icon's badge: what the open sessions are doing.
    BarBadge,
}

impl Switch {
    fn key(self) -> &'static str {
        match self {
            Switch::ShowSpend => "showSpend",
            Switch::RcAwake => "rcKeepsAwake",
            Switch::ShowLinear => "showLinear",
            Switch::BarBadge => "barBadge",
        }
    }
}

/// Whether `switch` is on. Safe from any thread, as the other loaders are.
pub fn load_switch(switch: Switch) -> bool {
    read_bool(switch.key())
}

pub fn save_switch(switch: Switch, on: bool) {
    write_bool(switch.key(), on);
}

/// Which notifications are wanted; each as its [`NOTIFY_KINDS`] default until
/// switched in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per notification kind, as Settings lists them"
)]
pub struct NotifyPrefs {
    /// An idle session's prompt cache is about to expire.
    pub cache_expiry: bool,
    /// A reviewer requests changes on an open session's PR.
    pub review_changes: bool,
    /// A reviewer approves it.
    pub review_approved: bool,
    /// A reviewer comments on it.
    pub review_commented: bool,
    /// A CI job starts failing on an open session's PR.
    pub ci_failures: bool,
    /// The PR's failing CI is green again.
    pub ci_recovered: bool,
    /// A PR in a tracked stack is merged.
    pub pr_merged: bool,
    /// A PR in a stack starts needing a rebase, as one below it merges.
    pub stack_rebase: bool,
}

impl NotifyPrefs {
    /// The switches in [`NOTIFY_KINDS`] order.
    pub fn as_array(self) -> [bool; 8] {
        [
            self.cache_expiry,
            self.review_changes,
            self.review_approved,
            self.review_commented,
            self.ci_failures,
            self.ci_recovered,
            self.pr_merged,
            self.stack_rebase,
        ]
    }

    pub fn from_array(
        [
            cache_expiry,
            review_changes,
            review_approved,
            review_commented,
            ci_failures,
            ci_recovered,
            pr_merged,
            stack_rebase,
        ]: [bool; 8],
    ) -> Self {
        Self {
            cache_expiry,
            review_changes,
            review_approved,
            review_commented,
            ci_failures,
            ci_recovered,
            pr_merged,
            stack_rebase,
        }
    }
}

/// The notification kinds, in the order Settings lists them, with their
/// keys, labels, and whether they are on before anything is chosen.
pub const NOTIFY_KINDS: [(&str, &str, bool); 8] = [
    (
        "notifyCacheExpiry",
        "A session's prompt cache is about to expire",
        true,
    ),
    (
        "notifyReviewChanges",
        "A reviewer requests changes on a session's PR",
        true,
    ),
    (
        "notifyReviewApproved",
        "A reviewer approves a session's PR",
        true,
    ),
    (
        "notifyReviewCommented",
        "A reviewer comments on a session's PR",
        true,
    ),
    ("notifyCiFailures", "A CI job fails on a session's PR", true),
    ("notifyCiRecovered", "A failing CI is green again", false),
    ("notifyPrMerged", "A PR in a stack is merged", false),
    ("notifyStackRebase", "A PR in a stack needs a rebase", false),
];

/// Before the review switch was split in three, one `notifyReviews` covered
/// all of them: a choice stored under it stands for the three until each is
/// chosen on its own.
const LEGACY_REVIEWS_KEY: &str = "notifyReviews";

fn read_bool(key: &str) -> bool {
    read_bool_or(key, true)
}

/// The stored switch, or `unset` if none was ever stored, which is not the
/// same as a stored `false`.
fn read_bool_or(key: &str, unset: bool) -> bool {
    let defaults = crate::view_prefs::defaults();
    let key = NSString::from_str(key);
    defaults
        .objectForKey(&key)
        .map_or(unset, |_| defaults.boolForKey(&key))
}

/// The saved notification choices. Safe from any thread: `NSUserDefaults`
/// is, and the sessions thread reads these on every pass.
pub fn load_notify() -> NotifyPrefs {
    let legacy_reviews = read_bool_or(LEGACY_REVIEWS_KEY, true);
    let mut on = [false; 8];
    for (i, (key, _, unset)) in NOTIFY_KINDS.iter().enumerate() {
        on[i] = read_bool_or(
            key,
            if (1..=3).contains(&i) {
                legacy_reviews
            } else {
                *unset
            },
        );
    }
    NotifyPrefs::from_array(on)
}

/// Switch the notification kind at `index` in [`NOTIFY_KINDS`].
pub fn save_notify(index: usize, on: bool) {
    if let Some((key, _, _)) = NOTIFY_KINDS.get(index) {
        write_bool(key, on);
    }
}

/// The least time between two refreshes of the usage numbers while a session
/// is writing, in seconds: they are refreshed when a transcript changes, and
/// each refresh that finds new rows writes them and folds the snapshot again.
pub const USAGE_EVERY: [u64; 3] = [15, 30, 45];
const USAGE_DEFAULT: u64 = 15;
/// How long a PR's CI and reviews are reused before `gh` is asked again, in
/// seconds. A branch with no PR is asked about a fifth as often.
pub const PR_EVERY: [u64; 3] = [30, 60, 300];
const PR_DEFAULT: u64 = 60;

/// The key the old fixed interval used: a choice still offered carries over.
const USAGE_KEY: &str = "refreshUsageSecs";
const PR_KEY: &str = "refreshPrSecs";

/// The refresh settings, read again by the background threads' waits whenever
/// a setting is saved (see [`crate::sessions::nap`]), so a change in Settings
/// applies at once. (The sessions are read when their records change: there
/// is nothing to set. `refreshSessionsSecs` and `backOffWhenIdle` are no
/// longer read.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollPrefs {
    pub usage_secs: u64,
    pub pr_secs: u64,
}

/// `"15 s"`, `"1 min"`: a number of seconds as a segment's label.
pub fn secs_label(secs: u64) -> String {
    if secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// The stored interval if it is one of `choices`, else `default`.
fn read_choice(key: &str, choices: &[u64], default: u64) -> u64 {
    let defaults = crate::view_prefs::defaults();
    let key = NSString::from_str(key);
    let stored = defaults
        .objectForKey(&key)
        .map(|_| defaults.integerForKey(&key));
    stored
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| choices.contains(value))
        .unwrap_or(default)
}

fn save_choice(key: &str, value: u64) {
    defaults().setInteger_forKey(
        isize::try_from(value).unwrap_or(isize::MAX),
        &NSString::from_str(key),
    );
    crate::sessions::wake();
}

/// Safe from any thread, as the other loaders are.
pub fn load_poll() -> PollPrefs {
    PollPrefs {
        usage_secs: read_choice(USAGE_KEY, &USAGE_EVERY, USAGE_DEFAULT),
        pr_secs: read_choice(PR_KEY, &PR_EVERY, PR_DEFAULT),
    }
}

pub fn save_usage_every(secs: u64) {
    save_choice(USAGE_KEY, secs);
}

pub fn save_pr_every(secs: u64) {
    save_choice(PR_KEY, secs);
}

/// How the open sessions are ordered within a group; the groups themselves
/// (Needs you, Working, Idle, Cold) keep their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    /// By project, a busy session first, then the newest started.
    #[default]
    Project,
    /// The one touched most recently first.
    Recent,
    /// The one started most recently first.
    Started,
}

stored_choice!(SortOrder {
    Project => "project", "Project";
    Recent => "recent", "Recent";
    Started => "started", "Newest";
});

/// What to do with the Cold group, the sessions whose prompt cache has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdMode {
    /// Listed in full.
    Show,
    /// Behind a `Cold · 3` row that unfolds them. In the compact popover,
    /// left out of All; the Cold filter lists them.
    Fold,
    /// Not listed under All at all; in the compact popover the Cold filter
    /// still lists them.
    Hide,
}

stored_choice!(ColdMode {
    Fold => "fold", "Fold";
    Show => "show", "Show";
    Hide => "hide", "Hide";
});

impl ColdMode {
    /// What the layout does before a choice is made: the menu folds its Cold
    /// group, the popover lists its cards.
    pub fn unset(layout: Layout) -> Self {
        match layout {
            Layout::Regular => ColdMode::Fold,
            Layout::Compact => ColdMode::Show,
        }
    }
}

/// How much room the compact popover's cards take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    #[default]
    Comfortable,
    Tight,
}

stored_choice!(Density {
    Comfortable => "comfortable", "Comfortable";
    Tight => "tight", "Tight";
});

/// Which usage window the menu bar icon's meter shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum BarWindow {
    /// The 5-hour session window, or the weekly one on a plan with no
    /// session limit.
    #[default]
    Auto,
    /// The 5-hour session window only.
    Session,
    /// The 7-day window only.
    Weekly,
}

stored_choice!(BarWindow {
    Auto => "auto", "Auto";
    Session => "session", "Session";
    Weekly => "weekly", "Weekly";
});

/// How early a long (1 h) prompt cache's expiry is warned of, in minutes. A
/// short (5 min) cache is always warned of one minute before.
pub const CACHE_LEAD_MINS: [u64; 4] = [1, 5, 10, 15];
const CACHE_LEAD_DEFAULT: u64 = 5;
/// How little cache an idle session may have left before it "needs you", in
/// minutes.
pub const NEEDS_YOU_MINS: [u64; 3] = [5, 15, 30];
const NEEDS_YOU_DEFAULT: u64 = 15;
/// Where a context ring turns orange and then red, in percent of the window.
/// The status line's own `⚠` stays at 85, whatever is chosen here.
pub const RING_POINTS: [(u8, u8); 3] = [(60, 80), (70, 85), (80, 90)];
const RING_DEFAULT: (u8, u8) = (70, 85);
/// How many open sessions are listed; the rest are counted.
pub const MAX_SESSION_CHOICES: [u64; 3] = [4, 8, 16];
const MAX_SESSIONS_DEFAULT: u64 = 16;

const CACHE_LEAD_KEY: &str = "cacheLeadMins";
const NEEDS_YOU_KEY: &str = "needsYouMins";
const RING_KEY: &str = "ringNotePct";
const MAX_SESSIONS_KEY: &str = "maxSessions";
const SORT_KEY: &str = "sortOrder";
const COLD_KEY: &str = "coldMode";
const DENSITY_KEY: &str = "density";
const BAR_WINDOW_KEY: &str = "barWindow";

/// The settings that tune what the sessions show and when they are flagged,
/// read afresh on every paint and every sessions pass so a change applies at
/// once. `Default` is what the tray did before any of these were settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// Warn this long before a long cache expires, in minutes.
    pub cache_lead_mins: u64,
    /// An idle session with at most this much cache left needs you, in ms.
    pub needs_you_ms: i64,
    /// A context ring turns orange here, in percent.
    pub ring_note: u8,
    /// And red here.
    pub ring_warn: u8,
    pub max_sessions: usize,
    pub sort: SortOrder,
    /// `None` until chosen: see [`ColdMode::unset`].
    pub cold: Option<ColdMode>,
    pub density: Density,
    pub bar_window: BarWindow,
    /// The menu bar icon's badge: what the open sessions are doing.
    pub bar_badge: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            cache_lead_mins: CACHE_LEAD_DEFAULT,
            needs_you_ms: i64::try_from(NEEDS_YOU_DEFAULT).unwrap_or(i64::MAX) * 60_000,
            ring_note: RING_DEFAULT.0,
            ring_warn: RING_DEFAULT.1,
            max_sessions: usize::try_from(MAX_SESSIONS_DEFAULT).unwrap_or(usize::MAX),
            sort: SortOrder::default(),
            cold: None,
            density: Density::default(),
            bar_window: BarWindow::default(),
            bar_badge: true,
        }
    }
}

impl Tuning {
    /// The Cold group's treatment in `layout`.
    pub fn cold_mode(&self, layout: Layout) -> ColdMode {
        self.cold.unwrap_or_else(|| ColdMode::unset(layout))
    }
}

/// The saved tuning. Safe from any thread, as the other loaders are.
pub fn load_tuning() -> Tuning {
    let ring = read_choice(
        RING_KEY,
        &RING_POINTS.map(|(note, _)| u64::from(note)),
        u64::from(RING_DEFAULT.0),
    );
    let (ring_note, ring_warn) = RING_POINTS
        .into_iter()
        .find(|(note, _)| u64::from(*note) == ring)
        .unwrap_or(RING_DEFAULT);
    Tuning {
        cache_lead_mins: read_choice(CACHE_LEAD_KEY, &CACHE_LEAD_MINS, CACHE_LEAD_DEFAULT),
        needs_you_ms: i64::try_from(read_choice(
            NEEDS_YOU_KEY,
            &NEEDS_YOU_MINS,
            NEEDS_YOU_DEFAULT,
        ))
        .unwrap_or(i64::MAX)
            * 60_000,
        ring_note,
        ring_warn,
        max_sessions: usize::try_from(read_choice(
            MAX_SESSIONS_KEY,
            &MAX_SESSION_CHOICES,
            MAX_SESSIONS_DEFAULT,
        ))
        .unwrap_or(usize::MAX),
        sort: read(SORT_KEY)
            .as_deref()
            .and_then(SortOrder::parse)
            .unwrap_or_default(),
        cold: read(COLD_KEY).as_deref().and_then(ColdMode::parse),
        density: read(DENSITY_KEY)
            .as_deref()
            .and_then(Density::parse)
            .unwrap_or_default(),
        bar_window: read(BAR_WINDOW_KEY)
            .as_deref()
            .and_then(BarWindow::parse)
            .unwrap_or_default(),
        bar_badge: load_switch(Switch::BarBadge),
    }
}

pub fn save_cache_lead(mins: u64) {
    save_choice(CACHE_LEAD_KEY, mins);
}

pub fn save_needs_you(mins: u64) {
    save_choice(NEEDS_YOU_KEY, mins);
}

/// Save the ring points whose orange point is `note`.
pub fn save_ring_points(note: u8) {
    save_choice(RING_KEY, u64::from(note));
}

pub fn save_max_sessions(count: u64) {
    save_choice(MAX_SESSIONS_KEY, count);
}

pub fn save_sort(order: SortOrder) {
    write(SORT_KEY, order.key());
}

pub fn save_cold(mode: ColdMode) {
    write(COLD_KEY, mode.key());
}

pub fn save_density(density: Density) {
    write(DENSITY_KEY, density.key());
}

pub fn save_bar_window(window: BarWindow) {
    write(BAR_WINDOW_KEY, window.key());
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;

    use super::*;

    thread_local! {
        /// The suite this test thread's settings are kept in, instead of the
        /// environment's: tests cannot set the environment, and run at once.
        pub(crate) static SUITE: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// Keeps this thread's settings in a fresh suite of its own until dropped,
    /// then removes it.
    pub(crate) struct ScratchSuite(String);

    impl ScratchSuite {
        pub(crate) fn new(name: &str) -> Self {
            // The same every run (see `tests/self_test.rs`): emptied first.
            let suite = format!("com.statusline.unit.{name}");
            SUITE.with(|current| *current.borrow_mut() = Some(suite.clone()));
            defaults().removePersistentDomainForName(&NSString::from_str(&suite));
            Self(suite)
        }
    }

    impl Drop for ScratchSuite {
        fn drop(&mut self) {
            discard_suite(&self.0);
            SUITE.with(|current| *current.borrow_mut() = None);
        }
    }

    #[test]
    fn a_fresh_suite_reads_as_every_default() {
        let _suite = ScratchSuite::new("defaults");
        assert_eq!(load_layout(), Layout::Regular);
        assert_eq!(load_meters(), MeterStyle::Bars);
        assert!(
            [
                Switch::ShowSpend,
                Switch::RcAwake,
                Switch::ShowLinear,
                Switch::BarBadge
            ]
            .into_iter()
            .all(load_switch),
            "switches are on until switched off"
        );
        assert_eq!(load_notify().as_array(), NOTIFY_KINDS.map(|(_, _, on)| on));
        assert_eq!(
            load_poll(),
            PollPrefs {
                usage_secs: 15,
                pr_secs: 60
            }
        );
        assert_eq!(load_tuning(), Tuning::default());
    }

    #[test]
    fn every_setting_round_trips_through_the_suite() {
        let _suite = ScratchSuite::new("round-trip");
        for layout in Layout::ALL {
            save_layout(layout);
            assert_eq!(load_layout(), layout);
        }
        for style in MeterStyle::ALL {
            save_meters(style);
            assert_eq!(load_meters(), style);
        }
        for on in [false, true] {
            save_switch(Switch::ShowSpend, on);
            save_switch(Switch::RcAwake, !on);
            save_switch(Switch::ShowLinear, on);
            save_switch(Switch::BarBadge, on);
            let switches =
                [Switch::ShowSpend, Switch::RcAwake, Switch::ShowLinear].map(load_switch);
            assert_eq!((switches, load_tuning().bar_badge), ([on, !on, on], on));
        }
        for (index, _) in NOTIFY_KINDS.iter().enumerate() {
            let before = load_notify().as_array();
            save_notify(index, !before[index]);
            let mut expected = before;
            expected[index] = !before[index];
            assert_eq!(
                load_notify().as_array(),
                expected,
                "only kind {index} switched"
            );
        }
        save_notify(NOTIFY_KINDS.len(), true);
        save_usage_every(45);
        save_pr_every(30);
        assert_eq!(
            load_poll(),
            PollPrefs {
                usage_secs: 45,
                pr_secs: 30
            }
        );
        save_cache_lead(15);
        save_needs_you(30);
        save_ring_points(80);
        save_max_sessions(4);
        save_sort(SortOrder::Recent);
        save_cold(ColdMode::Hide);
        save_density(Density::Tight);
        save_bar_window(BarWindow::Weekly);
        let tuning = load_tuning();
        assert_eq!(
            (
                tuning.cache_lead_mins,
                tuning.needs_you_ms,
                tuning.ring_note,
                tuning.ring_warn,
                tuning.max_sessions,
                tuning.sort,
                tuning.cold,
                tuning.density,
                tuning.bar_window
            ),
            (
                15,
                30 * 60_000,
                80,
                90,
                4,
                SortOrder::Recent,
                Some(ColdMode::Hide),
                Density::Tight,
                BarWindow::Weekly
            )
        );
    }

    #[test]
    fn values_that_are_not_a_choice_read_as_the_default() {
        let _suite = ScratchSuite::new("bad-values");
        save_pr_every(17);
        save_usage_every(300);
        save_max_sessions(0);
        save_ring_points(75);
        write(LAYOUT_KEY, "sideways");
        write(SORT_KEY, "Recent");
        assert_eq!(
            load_poll(),
            PollPrefs {
                usage_secs: 15,
                pr_secs: 60
            },
            "the old 5 min is not a choice any more"
        );
        let tuning = load_tuning();
        assert_eq!(
            (
                tuning.max_sessions,
                tuning.ring_note,
                tuning.ring_warn,
                tuning.sort
            ),
            (16, 70, 85, SortOrder::Project)
        );
        assert_eq!(load_layout(), Layout::Regular);
    }

    #[test]
    fn the_old_single_review_switch_stands_for_the_three_until_each_is_chosen() {
        let _suite = ScratchSuite::new("legacy");
        defaults().setBool_forKey(false, &NSString::from_str(LEGACY_REVIEWS_KEY));
        let notify = load_notify();
        assert!(
            !notify.review_changes && !notify.review_approved && !notify.review_commented,
            "all three off, as the old switch was"
        );
        assert!(
            notify.cache_expiry && notify.ci_failures,
            "the others keep their defaults"
        );
        save_notify(2, true);
        let notify = load_notify();
        assert!(
            notify.review_approved && !notify.review_changes,
            "one chosen on its own wins over the old switch"
        );
    }

    #[test]
    fn choices_are_in_order_and_include_their_defaults() {
        for (choices, default) in [
            (&USAGE_EVERY[..], USAGE_DEFAULT),
            (&PR_EVERY[..], PR_DEFAULT),
            (&CACHE_LEAD_MINS[..], CACHE_LEAD_DEFAULT),
            (&NEEDS_YOU_MINS[..], NEEDS_YOU_DEFAULT),
            (&MAX_SESSION_CHOICES[..], MAX_SESSIONS_DEFAULT),
        ] {
            assert!(
                choices.windows(2).all(|pair| pair[0] < pair[1]),
                "{choices:?}"
            );
            assert!(choices.contains(&default), "{default}");
        }
    }

    #[test]
    fn intervals_are_labelled_in_the_largest_whole_unit() {
        assert_eq!(secs_label(15), "15 s");
        assert_eq!(secs_label(60), "1 min");
        assert_eq!(secs_label(300), "5 min");
    }

    #[test]
    fn stored_names_round_trip_and_unknown_ones_are_ignored() {
        for layout in Layout::ALL {
            assert_eq!(Layout::parse(layout.key()), Some(layout));
        }
        for style in MeterStyle::ALL {
            assert_eq!(MeterStyle::parse(style.key()), Some(style));
        }
        assert_eq!(Layout::parse("Compact"), None, "stored keys are lower case");
        assert_eq!(MeterStyle::parse(""), None);
    }

    #[test]
    fn the_ring_points_are_in_order_and_include_their_default() {
        assert!(
            RING_POINTS
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1)
        );
        assert!(RING_POINTS.iter().all(|(note, warn)| note < warn));
        assert!(RING_POINTS.contains(&RING_DEFAULT));
    }

    #[test]
    fn the_default_tuning_is_what_the_tray_did_before_there_were_settings() {
        let tuning = Tuning::default();
        assert_eq!(tuning.needs_you_ms, crate::sessions::NEEDS_YOU_CACHE_MS);
        assert_eq!(tuning.max_sessions, crate::sessions::MAX_SESSIONS);
        assert_eq!((tuning.ring_note, tuning.ring_warn), (70, 85));
        assert_eq!(
            tuning.cold_mode(Layout::Regular),
            ColdMode::Fold,
            "the menu folds Cold"
        );
        assert_eq!(
            tuning.cold_mode(Layout::Compact),
            ColdMode::Show,
            "the popover lists it"
        );
        assert_eq!(
            (
                tuning.sort,
                tuning.density,
                tuning.bar_window,
                tuning.bar_badge
            ),
            (
                SortOrder::Project,
                Density::Comfortable,
                BarWindow::Auto,
                true
            )
        );
    }

    #[test]
    fn stored_tuning_names_round_trip() {
        for order in SortOrder::ALL {
            assert_eq!(SortOrder::parse(order.key()), Some(order));
        }
        for mode in ColdMode::ALL {
            assert_eq!(ColdMode::parse(mode.key()), Some(mode));
        }
        for density in Density::ALL {
            assert_eq!(Density::parse(density.key()), Some(density));
        }
        for window in BarWindow::ALL {
            assert_eq!(BarWindow::parse(window.key()), Some(window));
        }
        assert_eq!(SortOrder::parse("Recent"), None);
    }

    #[test]
    fn the_notification_switches_round_trip_in_order() {
        let prefs = NotifyPrefs::from_array([true, false, true, false, true, false, true, false]);
        assert_eq!(
            prefs.as_array(),
            [true, false, true, false, true, false, true, false]
        );
        assert!(
            prefs.cache_expiry
                && !prefs.review_changes
                && prefs.review_approved
                && !prefs.stack_rebase
        );
        assert_eq!(NOTIFY_KINDS.len(), prefs.as_array().len());
    }
}
