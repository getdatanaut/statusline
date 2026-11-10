//! The background refresh thread: the only place in this binary that touches
//! `sl_usage`.
//!
//! It loops forever -- incremental cache refresh, build a [`Snapshot`], publish
//! it into the shared mutex, sleep -- and never touches AppKit. The main thread
//! reads the mutex from the timer tick and never touches `sl_usage`. Nothing is
//! shared between the two but the `Arc<Mutex<Snapshot>>`, and a `Snapshot` is
//! plain owned data, so there is no borrow of the cache to outlive its owner.

use sl_usage::cache::{RefreshMode, UsageTotals};
use sl_usage::{PricingMap, Source};

use crate::limits::CodexLimits;
use crate::snapshot::{Snapshots, build_snapshots, now_ms};

/// The most time between two refreshes. Events say when a transcript changed;
/// this is for what they cannot say -- the day turning, a session block
/// ending -- and keeps the footer's "Updated" recent.
const USAGE_BACKSTOP: u64 = 60;

/// What the refresh keeps between runs: Claude Code's store, Codex's when a
/// Codex home exists, and Codex's last-seen limits per rollout.
pub struct Stores {
    pub claude: UsageTotals,
    pub codex: Option<UsageTotals>,
    codex_limits: CodexLimits,
}

fn ring() {
    crate::sessions::USAGE_BELL.ring();
}

impl Stores {
    /// The stores under the default directory, on one connection. Codex's is
    /// opened only when a Codex home exists.
    pub fn open_default() -> Self {
        let mut stores = Self {
            claude: UsageTotals::empty_for(&sl_usage::default_store_dir(), Source::Claude)
                .ringing(ring),
            codex: None,
            codex_limits: CodexLimits::default(),
        };
        stores.open_codex_if(crate::view_prefs::codex_available());
        stores
    }

    /// Open Codex's store when `installed` and it is not open yet: Codex can
    /// be installed while the tray runs, and everything else asks whether it
    /// is each time.
    fn open_codex_if(&mut self, installed: bool) {
        if installed && self.codex.is_none() {
            self.codex = Some(UsageTotals::empty_beside(&self.claude, Source::Codex).ringing(ring));
        }
    }
}

/// Refresh the stores and fold them into [`Snapshots`] as of now.
///
/// The one place that combines cache-refreshing with `build_snapshots`, shared
/// by the background loop below and `main`'s `--dump-snapshot` path so neither
/// has its own copy of "refresh the cache, fold". `rebuild` selects
/// [`RefreshMode::Rebuild`] (`--rebuild`'s full re-parse, ignoring all cached
/// state) over the normal incremental refresh.
///
/// `stores` is the caller's to keep: the loop reuses one for the life of the
/// process, so a refresh that finds nothing new costs next to nothing: the stores listen for file-system events and `stat()` only what they name. It holds
/// rollups, not records -- the records stay in the store.
pub fn load_and_build(stores: &mut Stores, pricing: &PricingMap, rebuild: bool) -> Snapshots {
    let start = std::time::Instant::now();
    let now = now_ms();
    let mode = if rebuild {
        RefreshMode::Rebuild
    } else {
        RefreshMode::Incremental
    };
    stores.open_codex_if(crate::view_prefs::codex_available());
    stores.claude.refresh(pricing, mode);
    // Re-read every refresh: the status-line hook rewrites
    // `limits.json` on every Claude Code turn, and a capture loaded once at
    // launch would freeze the menu bar title at whatever the limits were when
    // the tray started -- exactly the failure the staleness marker exists to
    // make visible.
    let capture = crate::limits::load_capture();
    let codex_limits = match &mut stores.codex {
        Some(codex) => {
            codex.refresh(pricing, mode);
            // The rollouts the refresh just found: no second walk.
            stores.codex_limits.load(codex.discovered())
        }
        None => None,
    };
    let fold = std::time::Instant::now();
    let snapshots = build_snapshots(
        &stores.claude,
        stores.codex.as_ref(),
        capture.as_ref(),
        codex_limits,
        now,
    );
    crate::trace(&format!(
        "refresh {:?}, fold {:?}",
        fold.duration_since(start),
        fold.elapsed()
    ));
    snapshots
}

/// Spawn the refresh loop. The returned handle is dropped by the caller: this
/// thread is meant to run for the life of the process and the app exits via
/// `terminate:`, so there is no graceful-shutdown handshake to honour.
///
/// `rebuild_first` is `--rebuild`'s effect on the normal (non-`--dump-snapshot`)
/// startup path: only the loop's first iteration forces a full rebuild. Every
/// iteration after that goes back to the incremental refresh -- a forced full
/// rebuild on every refresh would be wasteful and is not what `--rebuild` means.
pub fn spawn(shared: Shared, rebuild_first: bool) {
    let spawned = std::thread::Builder::new().name("sl-tray-refresh".to_owned()).spawn(move || {
        // Built once: `PricingMap::new()` parses the embedded pricing catalogs,
        // which is wasted work to repeat on every refresh.
        let pricing = PricingMap::new();
        let mut stores = Stores::open_default();
        let mut rebuild = rebuild_first;
        let mut last_io = crate::diskio::sample();
        loop {
            crate::sessions::USAGE_BELL.answer();
            let snapshots = load_and_build(&mut stores, &pricing, rebuild);
            let snapshot = &snapshots.claude;
            rebuild = false;
            // Under `SL_TRAY_DEBUG=1` only. The menu's own "Updated Ns ago ·
            // N records" row says the same thing, but reading it requires a
            // menu bar and a hand on the mouse -- this is how a headless run
            // proves the refresh actually landed something to paint.
            crate::trace(&format!(
                "published snapshot: {} records, {} models, today ${:.2}, block {}, limits {}",
                snapshot.record_count,
                snapshot.models.len(),
                snapshot.today.cost,
                if snapshot.current_block.is_some() { "active" } else { "none" },
                match snapshot.rate_limits.and_then(|limits| limits.five_hour) {
                    Some(window) => format!("{:.0}%", window.used_percent),
                    None => "none".to_owned(),
                }
            ));
            // What this refresh cost, measured: the store's own account of
            // what it read and wrote, and the process's disk counters since
            // the last refresh.
            if let Some(codex) = &snapshots.codex {
                crate::trace(&format!("codex: {} records, today ${:.2}, limits {}", codex.record_count, codex.today.cost, codex.codex_limits.is_some()));
            }
            let stats = stores.claude.last_refresh();
            let io = crate::diskio::sample();
            crate::trace(&format!(
                "refresh: {} files / {} read, {} rows in, {} out, {} partitions, {} buckets, {} txn{}{}{} | disk since last: {} read, {} written (process total {} / {})",
                stats.files_read,
                crate::diskio::fmt_bytes(stats.bytes_read),
                stats.rows_written,
                stats.rows_removed,
                stats.partitions,
                stats.buckets,
                stats.transactions,
                if stats.rebuilt { ", rebuilt" } else { "" },
                if stats.repriced { ", repriced" } else { "" },
                stats.error.as_deref().map(|error| format!(", error: {error}")).unwrap_or_default(),
                crate::diskio::fmt_bytes(io.read.saturating_sub(last_io.read)),
                crate::diskio::fmt_bytes(io.written.saturating_sub(last_io.written)),
                crate::diskio::fmt_bytes(io.read),
                crate::diskio::fmt_bytes(io.written),
            ));
            last_io = io;
            crate::latest::set(&shared, snapshots);
            // Until a transcript changes -- then no sooner than Settings'
            // Usage interval, since a working session appends many times a
            // second -- or the backstop.
            crate::sessions::nap(|| crate::sessions::USAGE_BELL.nap_secs(crate::view_prefs::load_poll().usage_secs, USAGE_BACKSTOP));
        }
    });

    if let Err(error) = spawned {
        // Without the refresh thread the tray would sit on the empty snapshot
        // forever. Say so rather than showing stale zeroes silently.
        crate::log(&format!("could not spawn the refresh thread: {error}"));
    }
}

/// The snapshots, as the refresh thread last published them.
pub type Shared = crate::latest::Latest<Snapshots>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_installed_after_the_tray_started_gets_its_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut stores = Stores {
            claude: UsageTotals::empty_for(dir.path(), Source::Claude),
            codex: None,
            codex_limits: CodexLimits::default(),
        };
        stores.open_codex_if(false);
        assert!(stores.codex.is_none(), "no Codex home: no store");
        stores.open_codex_if(true);
        assert!(stores.codex.is_some(), "a Codex home since: its store");
    }
}
