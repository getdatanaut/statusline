//! Discovery of Claude Code's on-disk JSONL transcript files.
//!
//! Ported from <https://github.com/ccusage/ccusage/blob/8841f921/rust/adapters/claude/src/paths.rs>, minus the
//! `project_filter` plumbing (not needed by this crate) and adapted from
//! ccusage's internal `home::home_dir()`/`Result` machinery to `dirs::home_dir()`
//! and a plain `Result<_, String>`.

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::watch::Watch;

/// Resolve the Claude Code data directories that hold a `projects/` subdirectory.
///
/// `CLAUDE_CONFIG_DIR` (comma-separated) is *exclusive*: when set, the default
/// locations are never consulted. Each entry is normalized (if it points at a
/// `projects/` directory itself, its parent is used instead) and kept only if
/// `<path>/projects` is a directory. If the env var is set but none of its
/// entries qualify, this returns an error.
///
/// Otherwise both `$XDG_CONFIG_HOME/claude` (default `~/.config/claude`) and
/// `~/.claude` are checked, and both may be kept (they are not exclusive of
/// each other).
///
/// # Errors
///
/// When `CLAUDE_CONFIG_DIR` names no usable directory, or it is unset and
/// there is no home directory.
pub fn claude_paths() -> Result<Vec<PathBuf>, String> {
    claude_paths_with(|key| env::var(key).ok(), dirs::home_dir().as_deref())
}

/// [`claude_paths`] with the environment read through `lookup` and the home
/// directory given, so it can be tested without touching either.
///
/// # Errors
///
/// As [`claude_paths`].
pub fn claude_paths_with(lookup: impl Fn(&str) -> Option<String>, home: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(env_paths) = lookup("CLAUDE_CONFIG_DIR") {
        for raw in env_paths.split(',').map(str::trim).filter(|path| !path.is_empty()) {
            let path = normalize_claude_config_path(raw, home);
            if path.join("projects").is_dir() && seen.insert(path.clone()) {
                paths.push(path);
            }
        }
        if !paths.is_empty() {
            return Ok(paths);
        }
        return Err(format!(
            "No valid Claude data directories found in CLAUDE_CONFIG_DIR. Expected each path to be a Claude config directory containing 'projects/', or the 'projects/' directory itself: {env_paths}"
        ));
    }

    let home = home.ok_or_else(|| "home directory is not set".to_string())?;
    let xdg = lookup("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    for path in [xdg.join("claude"), home.join(".claude")] {
        if path.join("projects").is_dir() && seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn expand_home_path(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~"
        && let Some(home) = home
    {
        return home.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

fn normalize_claude_config_path(raw: &str, home: Option<&Path>) -> PathBuf {
    let path = expand_home_path(raw, home);
    if path.file_name().is_some_and(|name| name == "projects") && path.is_dir() {
        return path.parent().map(Path::to_path_buf).unwrap_or(path);
    }
    path
}

/// Recursively walk each `<path>/projects` for `*.jsonl` files (not following
/// symlinks) and return a sorted `Vec<PathBuf>`. Sorted by full path string
/// for determinism, matching ccusage.
pub fn usage_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    Walk::new(is_transcript).files(&transcript_roots(paths))
}

/// Where [`usage_files`] looks under each data directory.
pub fn transcript_roots(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|path| path.join("projects")).collect()
}

/// A Claude Code transcript, by name.
pub fn is_transcript(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
}

/// A directory tree walked again and again, cheaply: each directory's
/// listing is kept with the directory's mtime and read again only once that
/// changes -- an entry added, removed or renamed in it changes it. A quiet
/// walk is then a `stat` per directory, not an open, a read and a close.
///
/// A [watched](Walk::watched) walk skips even that `stat` for a directory no
/// file-system event named since the last walk: see [`crate::watch`]. It still
/// finds exactly what a fresh walk would; events only say where to look.
///
/// Symlinks are not followed below a root, as ccusage does not: a link is
/// neither a file nor a directory here.
#[derive(Debug)]
pub struct Walk {
    keep: fn(&Path) -> bool,
    dirs: HashMap<PathBuf, Listing>,
    /// A listing read this soon after its directory last changed is read
    /// again next time: on a file system with coarse mtimes another change
    /// in the same tick would not move it.
    settle: Duration,
    round: u64,
    /// How many directories were read, for tests.
    reads: usize,
    /// How many directories were `stat`ed, for tests.
    stats: usize,
    /// Listens for changes, when this walk is watched.
    watch: Option<Watch>,
    /// What the last [`files`](Self::files) was told changed: `None` when
    /// everything was looked at.
    changed: Option<HashSet<PathBuf>>,
}

#[derive(Debug)]
struct Listing {
    mtime: SystemTime,
    trusted: bool,
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    /// The last walk that saw it; one gone from the tree is dropped.
    round: u64,
}

impl Walk {
    /// A walk that finds the files `keep` accepts.
    pub fn new(keep: fn(&Path) -> bool) -> Self {
        Self { keep, dirs: HashMap::new(), settle: Duration::from_secs(2), round: 0, reads: 0, stats: 0, watch: None, changed: None }
    }

    /// A walk that finds the files `keep` accepts and listens for changes
    /// under its roots, so a quiet walk `stat`s nothing.
    pub fn watched(keep: fn(&Path) -> bool) -> Self {
        Self { watch: Some(Watch::new()), ..Self::new(keep) }
    }

    /// Watched, ringing `bell` whenever something under the roots changes.
    #[must_use]
    pub fn ringing(self, bell: crate::watch::Bell) -> Self {
        Self { watch: Some(Watch::with_bell(Some(bell))), ..self }
    }

    /// What the last [`files`](Self::files) was told had changed under its
    /// roots (the paths touched, and the directory of each), or `None` when
    /// it looked at everything: unwatched, first walk, an overflow, a backstop
    /// sweep. A consumer with its own per-file state may skip the files
    /// outside the set, and must otherwise check them all.
    pub fn changed(&self) -> Option<&HashSet<PathBuf>> {
        self.changed.as_ref()
    }

    /// The next walk looks at everything, whatever events were seen: for a
    /// consumer that could not act on what [`changed`](Self::changed) said.
    pub fn distrust(&mut self) {
        if let Some(watch) = &mut self.watch {
            watch.distrust();
        }
    }

    /// Every kept file under `roots`, sorted by path. A missing root is
    /// simply empty.
    pub fn files(&mut self, roots: &[PathBuf]) -> Vec<PathBuf> {
        self.round += 1;
        self.changed = self.watch.as_mut().and_then(|watch| watch.take(roots));
        let changed = self.changed.take();
        let mut files = Vec::new();
        for root in roots {
            self.visit(root, changed.as_ref(), &mut files);
        }
        self.changed = changed;
        let round = self.round;
        self.dirs.retain(|_, listing| listing.round == round);
        files.sort_by_cached_key(|path| path.to_string_lossy().into_owned());
        files
    }

    /// Read `dir`, whose mtime is `mtime`, into a listing.
    fn read(&mut self, dir: &Path, mtime: SystemTime) {
        let read_at = SystemTime::now();
        let Ok(entries) = fs::read_dir(dir) else {
            self.dirs.remove(dir);
            return;
        };
        self.reads += 1;
        let mut listing = Listing { mtime, trusted: read_at.duration_since(mtime).is_ok_and(|age| age >= self.settle), files: Vec::new(), dirs: Vec::new(), round: 0 };
        for entry in entries.filter_map(Result::ok) {
            // `DirEntry::file_type` reports the entry's own type without
            // following symlinks, so a symlink is neither `is_file()` nor
            // `is_dir()` here and is skipped -- matching ccusage's "do not
            // follow symlinks".
            let Ok(file_type) = entry.file_type() else { continue };
            let path = entry.path();
            if file_type.is_file() && (self.keep)(&path) {
                listing.files.push(path);
            } else if file_type.is_dir() {
                listing.dirs.push(path);
            }
        }
        self.dirs.insert(dir.to_path_buf(), listing);
    }

    fn visit(&mut self, dir: &Path, changed: Option<&HashSet<PathBuf>>, files: &mut Vec<PathBuf>) {
        let settled = self.dirs.get(dir).filter(|listing| listing.trusted).map(|listing| listing.mtime);
        // A settled listing that no event named is as it was: no `stat`.
        let untouched = settled.is_some() && changed.is_some_and(|changed| !changed.contains(dir));
        if !untouched {
            // Followed: a root may be a link; below it, only real
            // directories are visited.
            self.stats += 1;
            let Ok(mtime) = fs::metadata(dir).and_then(|meta| meta.modified()) else {
                self.dirs.remove(dir);
                return;
            };
            if settled != Some(mtime) {
                self.read(dir, mtime);
            }
        }
        let Some(listing) = self.dirs.get_mut(dir) else { return };
        listing.round = self.round;
        files.extend(listing.files.iter().cloned());
        // Taken out for the walk below and put back: no copy of the list.
        let subdirs = std::mem::take(&mut listing.dirs);
        for sub in &subdirs {
            self.visit(sub, changed, files);
        }
        if let Some(listing) = self.dirs.get_mut(dir) {
            listing.dirs = subdirs;
        }
    }
}

/// Derive the path-based fallback `(session_id, project_path)` pair from a
/// transcript path. Handles both `<sessionId>.jsonl` and
/// `<sessionId>/subagents/*.jsonl` layouts.
pub fn extract_session_parts(path: &Path) -> (String, String) {
    let parts = path.components().filter_map(|component| component.as_os_str().to_str()).collect::<Vec<_>>();
    let projects_index = parts.iter().position(|part| *part == "projects");
    let relative = projects_index.map_or(&parts[..], |index| &parts[index + 1..]);

    let file_session_id =
        relative.last().and_then(|file_name| file_name.strip_suffix(".jsonl")).filter(|session_id| !session_id.is_empty());
    if relative.len() == 2
        && let Some(session_id) = file_session_id
    {
        return (session_id.to_string(), relative[0].to_string());
    }

    if relative.len() >= 4 && relative.get(relative.len() - 2) == Some(&"subagents") {
        let session_id = relative[relative.len() - 3].to_string();
        let project_path = relative[..relative.len() - 3].join(std::path::MAIN_SEPARATOR_STR);
        return (session_id, if project_path.is_empty() { "Unknown Project".to_string() } else { project_path });
    }

    let session_id = relative.get(relative.len().saturating_sub(2)).copied().unwrap_or("unknown").to_string();
    let project_path =
        if relative.len() > 2 { relative[..relative.len() - 2].join(std::path::MAIN_SEPARATOR_STR) } else { "Unknown Project".to_string() };
    (session_id, project_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An environment of `vars`, for [`claude_paths_with`].
    fn env_of(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars.iter().map(|(key, value)| ((*key).to_owned(), value.display().to_string())).collect();
        move |key| vars.get(key).cloned()
    }

    #[test]
    fn claude_paths_discovery() {
        let tmp_guard = tempfile::tempdir().unwrap();
        let tmp = tmp_guard.path().to_path_buf();

        // CLAUDE_CONFIG_DIR set with a single valid entry is exclusive: default
        // locations (which we also seed with valid `projects/` dirs below) are
        // never consulted.
        let configured = tmp.join("configured");
        fs::create_dir_all(configured.join("projects")).unwrap();
        let xdg_claude = tmp.join("xdg-config").join("claude");
        fs::create_dir_all(xdg_claude.join("projects")).unwrap();
        let home_claude = tmp.join("home").join(".claude");
        fs::create_dir_all(home_claude.join("projects")).unwrap();

        let home = tmp.join("home");
        let xdg = tmp.join("xdg-config");
        {
            let paths = claude_paths_with(env_of(&[("CLAUDE_CONFIG_DIR", &configured), ("XDG_CONFIG_HOME", &xdg)]), Some(&home)).expect("configured dir is valid");
            assert_eq!(paths, vec![configured.clone()], "CLAUDE_CONFIG_DIR is exclusive");
        }

        // CLAUDE_CONFIG_DIR set with a `projects/` dir itself is normalized to
        // its parent.
        {
            let paths = claude_paths_with(env_of(&[("CLAUDE_CONFIG_DIR", &configured.join("projects"))]), Some(&home)).expect("projects dir normalizes to parent");
            assert_eq!(paths, vec![configured.clone()], "a projects/ dir means its parent");
        }

        // CLAUDE_CONFIG_DIR set but pointing nowhere valid is an error, even
        // though the XDG/home fallbacks below would otherwise be valid.
        {
            let missing = tmp.join("does-not-exist");
            assert!(claude_paths_with(env_of(&[("CLAUDE_CONFIG_DIR", &missing), ("XDG_CONFIG_HOME", &xdg)]), Some(&home)).is_err(), "a configured dir that is not there is an error");
        }

        // Without CLAUDE_CONFIG_DIR: XDG_CONFIG_HOME/claude and ~/.claude are
        // both kept when both have a `projects/` dir (they are not exclusive of
        // each other). `dirs::home_dir()` reads `$HOME` on this platform, so
        // both sides can be pointed at the fixture and asserted together.
        {
            let paths = claude_paths_with(env_of(&[("XDG_CONFIG_HOME", &xdg)]), Some(&home)).expect("xdg + home fallback resolves");
            assert_eq!(paths, vec![xdg_claude.clone(), home_claude.clone()], "both roots are kept, XDG first");
        }
        assert!(claude_paths_with(env_of(&[]), None).is_err(), "no home and no CLAUDE_CONFIG_DIR is an error");
        {
            let tilde = claude_paths_with(|key| (key == "CLAUDE_CONFIG_DIR").then(|| "~/.claude".to_owned()), Some(&home)).expect("~ expands to home");
            assert_eq!(tilde, vec![home_claude.clone()], "~/ is the given home");
        }

    }

    /// A small transcript tree, removed when the guard is dropped.
    fn walk_tree() -> (tempfile::TempDir, PathBuf) {
        let guard = tempfile::tempdir().unwrap();
        let tmp = guard.path().to_path_buf();
        for dir in ["p1/s1/subagents", "p2"] {
            fs::create_dir_all(tmp.join(dir)).unwrap();
        }
        for file in ["p1/s1.jsonl", "p1/s1/subagents/a.jsonl", "p1/notes.txt", "p2/s2.jsonl"] {
            fs::write(tmp.join(file), "").unwrap();
        }
        (guard, tmp)
    }

    /// Whatever changes in the tree, a kept walk finds what a fresh one does,
    /// and reads only the directories that changed.
    #[test]
    fn a_kept_walk_rereads_only_what_changed_and_agrees_with_a_fresh_one() {
        let (_guard, tmp) = walk_tree();
        let roots = [tmp.clone()];
        let mut walk = Walk::new(is_transcript);
        walk.settle = Duration::ZERO;
        let fresh = || Walk::new(is_transcript).files(&roots);
        assert_eq!(walk.files(&roots), fresh());
        assert_eq!(walk.reads, 5, "the root, p1, s1, subagents, p2");
        assert_eq!(walk.files(&roots), fresh());
        assert_eq!(walk.reads, 5, "nothing changed: nothing read");

        fs::write(tmp.join("p1/s1/subagents/b.jsonl"), "").unwrap();
        assert_eq!(walk.files(&roots), fresh(), "a new file in a deep directory");
        assert_eq!(walk.reads, 6, "only that directory read again");

        fs::remove_file(tmp.join("p2/s2.jsonl")).unwrap();
        fs::create_dir_all(tmp.join("p3")).unwrap();
        fs::write(tmp.join("p3/s3.jsonl"), "").unwrap();
        assert_eq!(walk.files(&roots), fresh(), "a file gone, a project new");
        fs::remove_dir_all(tmp.join("p1/s1")).unwrap();
        let files = walk.files(&roots);
        assert_eq!(files, fresh(), "a directory gone");
        assert!(!walk.dirs.contains_key(&tmp.join("p1/s1/subagents")), "its listing is dropped");
        let names: Vec<_> = files.iter().map(|f| f.strip_prefix(&tmp).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["p1/s1.jsonl", "p3/s3.jsonl"]);
    }

    /// A directory that changed moments before it was read may change again
    /// within its mtime's tick: it is read again until it has settled.
    #[test]
    fn a_directory_that_just_changed_is_read_again_until_it_settles() {
        let (_guard, tmp) = walk_tree();
        let roots = [tmp.clone()];
        let mut walk = Walk::new(is_transcript);
        walk.files(&roots);
        walk.files(&roots);
        assert_eq!(walk.reads, 10, "everything was just written: all read both times");
    }

    /// A root that is a link is followed; a link below it is not.
    #[test]
    fn a_linked_root_is_followed_but_no_link_below_it() {
        let (_guard, tmp) = walk_tree();
        let elsewhere_guard = tempfile::tempdir().unwrap();
        let elsewhere = elsewhere_guard.path().to_path_buf();
        fs::write(elsewhere.join("x.jsonl"), "").unwrap();
        std::os::unix::fs::symlink(&elsewhere, tmp.join("p2/linked")).unwrap();
        let root_link_guard = tempfile::tempdir().unwrap();
        let root_link = root_link_guard.path().join("root");
        std::os::unix::fs::symlink(&tmp, &root_link).unwrap();
        let files = Walk::new(is_transcript).files(std::slice::from_ref(&root_link));
        assert_eq!(files.len(), 3, "through the root link, not into p2/linked: {files:?}");
    }


    /// Poll `ready` until it holds, for up to ten seconds: FSEvents delivers
    /// after a delay, so a test waits on a condition, never a fixed time.
    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// A watched walk over `tmp` that trusts every listing at once, and has
    /// gone quiet: its last walk found no events to act on.
    fn quiet_watched_walk(tmp: &Path) -> Walk {
        let mut walk = Walk::watched(is_transcript);
        walk.settle = Duration::ZERO;
        let roots = [tmp.to_path_buf()];
        walk.files(&roots);
        wait_for("the walk to go quiet", || {
            walk.files(&roots);
            walk.changed().is_some_and(HashSet::is_empty)
        });
        walk
    }

    /// Events say where to look: a quiet watched walk reads and `stat`s no
    /// directory at all.
    #[test]
    fn a_quiet_watched_walk_looks_at_no_directory() {
        let (_guard, tmp) = walk_tree();
        let roots = [tmp.clone()];
        let mut walk = quiet_watched_walk(&tmp);
        let (reads, stats) = (walk.reads, walk.stats);
        for _ in 0..3 {
            assert_eq!(walk.files(&roots), Walk::new(is_transcript).files(&roots));
        }
        assert_eq!((walk.reads, walk.stats), (reads, stats), "nothing read, nothing statted");
    }

    /// Whatever changes in the tree, a watched walk finds what a fresh one
    /// does -- through events, not a sweep.
    #[test]
    fn a_watched_walk_follows_the_tree_by_events_and_agrees_with_a_fresh_one() {
        let (_guard, tmp) = walk_tree();
        let roots = [tmp.clone()];
        let mut walk = quiet_watched_walk(&tmp);
        let fresh = || Walk::new(is_transcript).files(&roots);
        let converges = |walk: &mut Walk, what: &str| {
            wait_for(what, || walk.files(&roots) == fresh());
            assert_eq!(walk.files(&roots), fresh(), "{what}");
        };

        fs::create_dir_all(tmp.join("p4/s4/subagents")).unwrap();
        fs::write(tmp.join("p4/s4/subagents/new.jsonl"), "").unwrap();
        converges(&mut walk, "a file in a new subdirectory");
        assert!(walk.files(&roots).contains(&tmp.join("p4/s4/subagents/new.jsonl")));

        fs::write(tmp.join("p2/s2.jsonl"), "appended").unwrap();
        wait_for("an append to be named", || {
            walk.files(&roots);
            walk.changed().is_some_and(|changed| changed.contains(&tmp.join("p2/s2.jsonl")))
        });

        fs::remove_dir_all(tmp.join("p1/s1")).unwrap();
        converges(&mut walk, "a directory removed");
        assert!(walk.changed().is_some(), "by events, not a sweep");
        assert!(!walk.dirs.contains_key(&tmp.join("p1/s1/subagents")), "its listing is dropped");
    }

    /// Without events to trust -- a rescan, an error, a watcher that could not
    /// be made -- a watched walk looks at everything and agrees with a fresh one.
    #[test]
    fn a_watched_walk_without_events_to_trust_looks_at_everything() {
        let (_guard, tmp) = walk_tree();
        let roots = [tmp.clone()];
        let fresh = || Walk::new(is_transcript).files(&roots);
        let mut walk = quiet_watched_walk(&tmp);

        // Changes the walk was never told of, then a forced sweep.
        fs::write(tmp.join("p2/hidden.jsonl"), "").unwrap();
        walk.distrust();
        let files = walk.files(&roots);
        assert!(walk.changed().is_none(), "a sweep names no paths");
        assert_eq!(files, fresh());
        assert!(files.contains(&tmp.join("p2/hidden.jsonl")));

        let mut broken = Walk::watched(is_transcript);
        broken.settle = Duration::ZERO;
        broken.watch = Some(crate::watch::Watch::unavailable());
        assert_eq!(broken.files(&roots), fresh());
        fs::write(tmp.join("p2/more.jsonl"), "").unwrap();
        assert_eq!(broken.files(&roots), fresh(), "no watcher: every walk is a sweep");
        assert!(broken.changed().is_none());
    }

    /// A root that is missing when the walk starts is found once it appears.
    #[test]
    fn a_watched_walk_finds_a_root_that_appears_later() {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().join("later");
        let roots = [root.clone()];
        let mut walk = Walk::watched(is_transcript);
        walk.settle = Duration::ZERO;
        assert!(walk.files(&roots).is_empty());
        fs::create_dir_all(root.join("p")).unwrap();
        fs::write(root.join("p/a.jsonl"), "").unwrap();
        assert_eq!(walk.files(&roots), vec![root.join("p/a.jsonl")], "the walk that finds the root sweeps");
        fs::write(root.join("p/b.jsonl"), "").unwrap();
        wait_for("the next file", || walk.files(&roots).len() == 2);
    }

    #[test]
    fn usage_files_walks_recursively_sorted_ignoring_non_jsonl() {
        let tmp_guard = tempfile::tempdir().unwrap();
        let tmp = tmp_guard.path().to_path_buf();
        let projects = tmp.join("projects");
        fs::create_dir_all(projects.join("proj-a")).unwrap();
        fs::create_dir_all(projects.join("proj-b").join("sub")).unwrap();
        fs::write(projects.join("proj-a").join("b.jsonl"), "").unwrap();
        fs::write(projects.join("proj-a").join("a.jsonl"), "").unwrap();
        fs::write(projects.join("proj-a").join("not-usage.txt"), "").unwrap();
        fs::write(projects.join("proj-b").join("sub").join("c.jsonl"), "").unwrap();

        let files = usage_files(std::slice::from_ref(&tmp));
        let names: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.windows(2).all(|w| w[0] <= w[1]), "expected sorted output: {names:?}");
        assert!(names.iter().all(|n| Path::new(n).extension().is_some_and(|ext| ext == "jsonl")), "only transcripts");

    }



    #[test]
    fn extract_session_parts_simple_layout() {
        let path = Path::new("/home/user/.claude/projects/my-project/abc123.jsonl");
        let (session_id, project_path) = extract_session_parts(path);
        assert_eq!(session_id, "abc123");
        assert_eq!(project_path, "my-project");
    }

    #[test]
    fn extract_session_parts_subagents_layout() {
        let path = Path::new("/home/user/.claude/projects/my-project/abc123/subagents/def456.jsonl");
        let (session_id, project_path) = extract_session_parts(path);
        assert_eq!(session_id, "abc123");
        assert_eq!(project_path, "my-project");
    }

    #[test]
    fn extract_session_parts_nested_project_subagents_layout() {
        let path = Path::new("/home/user/.claude/projects/team/my-project/abc123/subagents/def456.jsonl");
        let (session_id, project_path) = extract_session_parts(path);
        assert_eq!(session_id, "abc123");
        assert_eq!(project_path, format!("team{}my-project", std::path::MAIN_SEPARATOR));
    }
}
