//! File-system events as a hint about what changed.
//!
//! A [`Watch`] listens (FSEvents on macOS) under a set of roots and collects
//! the paths that were touched. [`Watch::take`] hands them over, once per
//! refresh: the walk then re-reads only the directories named, and the store
//! stats only the files named, instead of statting the whole tree.
//!
//! Events are a hint, never the source of truth. Whenever they cannot be
//! trusted [`take`](Watch::take) says so by returning `None` -- "sweep
//! everything, as without a watch" -- which it does when:
//!
//! - it is the first call, or the watcher could not be created;
//! - a root did not exist when asked about (it is watched from the call after
//!   it appears, and that call sweeps);
//! - notify reported an error, or that events were dropped (a rescan);
//! - an event arrived with no path to act on;
//! - [`SWEEP_EVERY`] has passed since the last sweep, a backstop for anything
//!   an event stream can lose.
//!
//! Nothing here is public beyond [`Watch`]; the notify types stay inside.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// How long events are trusted before everything is looked at again anyway.
pub const SWEEP_EVERY: Duration = Duration::from_secs(300);

/// Called, on the watcher's own thread, whenever something worth a look has
/// happened: for a process that would rather be told than ask on a timer. It
/// can be called many times a second while a transcript is being written, so
/// it should do no more than note it.
pub type Bell = fn();

/// What the watcher's thread has collected since the last [`Watch::take`].
#[derive(Debug, Default)]
struct Inbox {
    /// Paths touched, and the directory holding each: an entry added to or
    /// removed from a directory changes the directory.
    dirty: HashSet<PathBuf>,
    /// Something happened that the paths cannot be trusted over.
    rescan: bool,
    /// Each watched root as the caller names it and as the file system reports
    /// it (`/var` is `/private/var` to FSEvents; a root may be a link).
    roots: Vec<(PathBuf, PathBuf)>,
}

impl Inbox {
    /// Note what `result` says. Whether it is anything to look at.
    fn record(&mut self, result: notify::Result<notify::Event>) -> bool {
        let Ok(event) = result else {
            self.rescan = true;
            return true;
        };
        if event.need_rescan() {
            self.rescan = true;
        }
        // Looking at a file is not changing it.
        if matches!(event.kind, EventKind::Access(_)) {
            return self.rescan;
        }
        if event.paths.is_empty() {
            self.rescan = true;
        }
        for path in event.paths {
            let path = self.named(path);
            if let Some(parent) = path.parent() {
                self.dirty.insert(parent.to_path_buf());
            }
            self.dirty.insert(path);
        }
        true
    }

    /// `path` as the caller names it: under the root's own name, not the one
    /// the file system reports.
    fn named(&self, path: PathBuf) -> PathBuf {
        for (given, real) in &self.roots {
            if let Ok(rest) = path.strip_prefix(real) {
                return if rest.as_os_str().is_empty() {
                    given.clone()
                } else {
                    given.join(rest)
                };
            }
        }
        path
    }
}

fn locked(inbox: &Mutex<Inbox>) -> MutexGuard<'_, Inbox> {
    // A poisoned inbox is still a list of paths; losing track is the worse
    // failure.
    inbox
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Collects what changed under some roots. See the module docs.
pub struct Watch {
    /// `None` when none could be created: every [`take`](Self::take) sweeps.
    watcher: Option<RecommendedWatcher>,
    inbox: Arc<Mutex<Inbox>>,
    /// The roots being watched, as named.
    watching: HashSet<PathBuf>,
    last_sweep: Option<Instant>,
    sweep_every: Duration,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("available", &self.watcher.is_some())
            .field("watching", &self.watching)
            .finish_non_exhaustive()
    }
}

impl Default for Watch {
    fn default() -> Self {
        Self::new()
    }
}

impl Watch {
    /// A watch with nothing watched yet. If the platform's watcher cannot be
    /// created it is a watch that always asks for a sweep.
    pub fn new() -> Self {
        Self::with_bell(None)
    }

    /// [`new`](Self::new), ringing `bell` whenever something is collected.
    pub fn with_bell(bell: Option<Bell>) -> Self {
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let sink = Arc::clone(&inbox);
        let watcher = notify::recommended_watcher(move |result| {
            let worth_a_look = locked(&sink).record(result);
            if let Some(bell) = bell.filter(|_| worth_a_look) {
                bell();
            }
        })
        .ok();
        Self {
            watcher,
            inbox,
            watching: HashSet::new(),
            last_sweep: None,
            sweep_every: SWEEP_EVERY,
        }
    }

    /// What changed under `roots` since the last call: the paths touched (and
    /// the directory of each), or `None` when everything must be looked at.
    pub fn take(&mut self, roots: &[PathBuf]) -> Option<HashSet<PathBuf>> {
        let mut sweep = self
            .last_sweep
            .is_none_or(|at| at.elapsed() >= self.sweep_every);
        for root in roots {
            if self.watching.contains(root) {
                continue;
            }
            // Not there (yet): nothing to watch, nothing to trust.
            sweep = true;
            let Ok(real) = root.canonicalize() else {
                continue;
            };
            let Some(watcher) = self.watcher.as_mut() else {
                continue;
            };
            // Named before it is watched: no event can come back unmapped.
            locked(&self.inbox).roots.push((root.clone(), real.clone()));
            if watcher.watch(&real, RecursiveMode::Recursive).is_ok() {
                self.watching.insert(root.clone());
            } else {
                locked(&self.inbox).roots.retain(|(given, _)| given != root);
            }
        }
        let mut inbox = locked(&self.inbox);
        let dirty = std::mem::take(&mut inbox.dirty);
        sweep |= std::mem::take(&mut inbox.rescan)
            || self.watcher.is_none()
            || roots.iter().any(|root| !self.watching.contains(root));
        drop(inbox);
        if sweep {
            self.last_sweep = Some(Instant::now());
            return None;
        }
        Some(dirty)
    }

    /// A watch whose watcher could not be created, for tests.
    #[cfg(test)]
    pub(crate) fn unavailable() -> Self {
        Self {
            watcher: None,
            ..Self::new()
        }
    }

    /// Whatever was gathered cannot be trusted: the next [`take`](Self::take)
    /// sweeps.
    pub fn distrust(&mut self) {
        self.last_sweep = None;
    }

    /// Whether `root` is being watched, for tests.
    #[cfg(test)]
    fn is_watching(&self, root: &std::path::Path) -> bool {
        self.watching.contains(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Poll `ready` until it holds, for up to ten seconds: FSEvents delivers
    /// after a delay, so a test waits on a condition, never a fixed time.
    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn the_first_take_sweeps_and_a_change_is_then_reported_by_path() {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().to_path_buf();
        fs::create_dir_all(root.join("p")).unwrap();
        fs::write(root.join("p/a.jsonl"), "").unwrap();
        let mut watch = Watch::new();
        assert!(
            watch.take(std::slice::from_ref(&root)).is_none(),
            "the first call sweeps"
        );
        assert!(watch.is_watching(&root));
        let target = root.join("p/a.jsonl");
        let mut seen = None;
        // Appended once, then waited for: a quiet watch reports nothing, a
        // stray late event from the set-up may arrive first.
        fs::write(&target, "x").unwrap();
        wait_for("the append to be reported", || {
            match watch.take(std::slice::from_ref(&root)) {
                Some(dirty) if dirty.contains(&target) => {
                    seen = Some(dirty);
                    true
                }
                Some(_) => false,
                None => panic!("a sweep was asked for with nothing wrong"),
            }
        });
        let dirty = seen.unwrap();
        assert!(
            dirty.contains(&root.join("p")),
            "the directory of the file is named too: {dirty:?}"
        );
    }

    #[test]
    fn a_rescan_an_error_or_no_watcher_sweeps() {
        let guard = tempfile::tempdir().unwrap();
        let roots = [guard.path().to_path_buf()];
        let mut watch = Watch::new();
        assert!(watch.take(&roots).is_none());
        wait_for("a quiet watch", || watch.take(&roots).is_some());

        locked(&watch.inbox).record(Err(notify::Error::generic("lost")));
        assert!(watch.take(&roots).is_none(), "an error sweeps");
        wait_for("a quiet watch", || watch.take(&roots).is_some());

        locked(&watch.inbox).record(Ok(notify::Event::new(EventKind::Other)
            .set_flag(notify::event::Flag::Rescan)
            .add_path(roots[0].clone())));
        assert!(watch.take(&roots).is_none(), "a rescan sweeps");
        wait_for("a quiet watch", || watch.take(&roots).is_some());

        locked(&watch.inbox).record(Ok(notify::Event::new(EventKind::Other)));
        assert!(watch.take(&roots).is_none(), "an event with no path sweeps");
        wait_for("a quiet watch", || watch.take(&roots).is_some());

        watch.distrust();
        assert!(watch.take(&roots).is_none(), "distrusted: sweeps");
        wait_for("a quiet watch", || watch.take(&roots).is_some());

        watch.sweep_every = Duration::ZERO;
        assert!(watch.take(&roots).is_none(), "the backstop sweeps");

        let mut unavailable = Watch::new();
        unavailable.watcher = None;
        assert!(unavailable.take(&roots).is_none());
        assert!(
            unavailable.take(&roots).is_none(),
            "without a watcher every call sweeps"
        );
    }

    #[test]
    fn a_change_rings_the_bell() {
        static RUNG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn ring() {
            RUNG.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let guard = tempfile::tempdir().unwrap();
        let roots = [guard.path().to_path_buf()];
        let mut watch = Watch::with_bell(Some(ring));
        assert!(watch.take(&roots).is_none());
        let before = RUNG.load(std::sync::atomic::Ordering::SeqCst);
        fs::write(roots[0].join("a.jsonl"), "x").unwrap();
        wait_for("the bell", || {
            RUNG.load(std::sync::atomic::Ordering::SeqCst) > before
        });
        wait_for("the change to be collected", || {
            watch
                .take(&roots)
                .is_some_and(|dirty| dirty.contains(&roots[0].join("a.jsonl")))
        });
        // A look is not a change; an error is worth one.
        let mut inbox = Inbox::default();
        assert!(
            !inbox.record(Ok(notify::Event::new(EventKind::Access(
                notify::event::AccessKind::Any
            ))
            .add_path(roots[0].join("a.jsonl"))))
        );
        assert!(inbox.record(Err(notify::Error::generic("lost"))));
    }

    #[test]
    fn a_root_that_appears_later_is_watched_from_then_on() {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().join("later");
        let roots = [root.clone()];
        let mut watch = Watch::new();
        assert!(watch.take(&roots).is_none());
        assert!(watch.take(&roots).is_none(), "missing: sweeps every time");
        fs::create_dir_all(&root).unwrap();
        assert!(
            watch.take(&roots).is_none(),
            "the call that finds it sweeps"
        );
        assert!(watch.is_watching(&root));
        wait_for("a quiet watch", || watch.take(&roots).is_some());
    }

    #[test]
    fn events_are_named_under_the_root_as_given_not_as_the_file_system_reports_it() {
        let guard = tempfile::tempdir().unwrap();
        let real = guard.path().canonicalize().unwrap();
        let link = tempfile::tempdir().unwrap();
        let given = link.path().join("root");
        std::os::unix::fs::symlink(&real, &given).unwrap();
        let roots = [given.clone()];
        let mut watch = Watch::new();
        assert!(watch.take(&roots).is_none());
        fs::write(real.join("f.jsonl"), "x").unwrap();
        wait_for("the new file under the link's name", || {
            watch
                .take(&roots)
                .is_some_and(|dirty| dirty.contains(&given.join("f.jsonl")))
        });
    }
}
