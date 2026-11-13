use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_millis(500);

/// Branch is resolved fresh on every render, pinned to the session's
/// directory. No cross-session cache: claude-pulse cached the branch in a file
/// shared by all sessions, so concurrent sessions in different repos showed
/// each other's (stale) branches.
///
/// On a branch -- nearly always -- it is read off the repository's `HEAD`
/// file, as `git branch --show-current` would print it, without starting
/// `git`. Anything else (a detached `HEAD`, whose short sha `git` abbreviates
/// as the repository needs; a `HEAD` file this does not know) asks `git`.
pub fn current_branch(dir: Option<&str>) -> Option<String> {
    let dir = dir?;
    let found = discover(Path::new(dir));
    if let Some(branch) = found.as_ref().and_then(|found| branch_in(&found.git_dir)) {
        return Some(branch);
    }
    // In no repository, as `git` would find none either: nothing to ask.
    if found.is_none() && outside_any_repository(Path::new(dir)) {
        return None;
    }
    if let Some(branch) = git(dir, &["branch", "--show-current"]) {
        return Some(branch);
    }
    // detached HEAD
    let sha = git(dir, &["rev-parse", "--short", "HEAD"])?;
    Some(format!("@{sha}"))
}

/// The repository's shared git directory (the same for every worktree of it),
/// as an absolute path with symlinks resolved, as
/// `git rev-parse --path-format=absolute --git-common-dir` prints it.
pub fn common_dir(dir: &str) -> Option<String> {
    if let Some(found) = discover(Path::new(dir)) {
        return Some(found.common_dir.to_string_lossy().into_owned());
    }
    if outside_any_repository(Path::new(dir)) {
        return None;
    }
    git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
}

/// `owner/name` of the GitHub repository `dir`'s PRs are in: see
/// [`github_remote`].
pub fn github_repo(dir: &str) -> Option<String> {
    github_remote(dir).map(|remote| remote.repo)
}

/// Where a work tree's PRs are on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRemote {
    /// `owner/name` of the repository PRs are opened in: the remote
    /// `gh repo set-default` chose (a fork's upstream), else `origin`.
    pub repo: String,
    /// The owner of `origin`, where branches are pushed, when that is a fork
    /// of `repo`; `None` when it is `repo` itself.
    pub head_owner: Option<String>,
}

/// Where `dir`'s PRs are on GitHub; `None` outside a repository, or when
/// neither the `gh repo set-default` remote nor `origin` is on github.com.
/// Read from the repository's `config` file, else asked of `git`.
pub fn github_remote(dir: &str) -> Option<GitHubRemote> {
    let remotes = match discover(Path::new(dir)) {
        Some(found) => remotes(&std::fs::read_to_string(found.common_dir.join("config")).ok()?),
        None => vec![Remote {
            name: "origin".to_owned(),
            url: git(dir, &["config", "--get", "remote.origin.url"])?,
            resolved: None,
        }],
    };
    let origin = remotes
        .iter()
        .find(|r| r.name == "origin")
        .and_then(|r| github_name(&r.url));
    // `gh repo set-default` marks its remote `gh-resolved = base` (older
    // versions wrote the repository's name instead).
    let chosen = remotes.iter().find_map(|r| match r.resolved.as_deref()? {
        "base" => github_name(&r.url),
        name if name.contains('/') => Some(name.to_owned()),
        _ => None,
    });
    let repo = chosen.or_else(|| origin.clone())?;
    let owner_of = |name: &str| name.split('/').next().unwrap_or_default().to_owned();
    let head_owner = origin
        .map(|origin| owner_of(&origin))
        .filter(|owner| !owner.eq_ignore_ascii_case(&owner_of(&repo)));
    Some(GitHubRemote { repo, head_owner })
}

/// A `[remote "name"]` section of a git `config` file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Remote {
    name: String,
    url: String,
    /// Its `gh-resolved`, if `gh` set one.
    resolved: Option<String>,
}

/// The remotes with a `url` in the text of a git `config` file.
fn remotes(config: &str) -> Vec<Remote> {
    let mut found: Vec<Remote> = Vec::new();
    let mut current: Option<Remote> = None;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            found.extend(current.take().filter(|r| !r.url.is_empty()));
            let header = line.replace(' ', "");
            current = header
                .strip_prefix("[remote\"")
                .and_then(|rest| rest.strip_suffix("\"]"))
                .map(|name| Remote {
                    name: name.to_owned(),
                    url: String::new(),
                    resolved: None,
                });
        } else if let Some(remote) = current.as_mut()
            && let Some((key, value)) = line.split_once('=')
        {
            let value = value.trim().trim_matches('"').to_owned();
            match key.trim().to_ascii_lowercase().as_str() {
                "url" if remote.url.is_empty() => remote.url = value,
                "gh-resolved" => remote.resolved = Some(value),
                _ => {}
            }
        }
    }
    found.extend(current.filter(|r| !r.url.is_empty()));
    found
}

/// `owner/name` of a GitHub remote URL in any of its forms:
/// `git@github.com:o/r.git`, `ssh://git@github.com/o/r`,
/// `https://github.com/o/r.git`. Over SSH the host may be an alias that
/// `~/.ssh/config` points at github.com (`git@github.com-work:o/r`).
pub fn github_name(url: &str) -> Option<String> {
    let (host, path, ssh) = if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let ssh = scheme.eq_ignore_ascii_case("ssh") || scheme.eq_ignore_ascii_case("git+ssh");
        (authority, path, ssh)
    } else {
        // scp-like: `[user@]host:path`.
        let (host, path) = url.split_once(':')?;
        if host.contains('/') {
            return None;
        }
        (host, path, true)
    };
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?;
    let on_github = host.eq_ignore_ascii_case("github.com")
        || host.eq_ignore_ascii_case("www.github.com")
        || (ssh && ssh_host_name(host).is_some_and(|name| name.eq_ignore_ascii_case("github.com")));
    if !on_github {
        return None;
    }
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

/// The `HostName` `~/.ssh/config` gives the SSH host `alias`, if it names it.
fn ssh_host_name(alias: &str) -> Option<String> {
    static CONFIG: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        std::fs::read_to_string(dirs::home_dir()?.join(".ssh").join("config")).ok()
    });
    host_name_in(config.as_deref()?, alias)
}

/// The `HostName` of the first `Host` block of an `ssh_config` text that
/// names `alias` outright (patterns are not matched).
fn host_name_in(config: &str, alias: &str) -> Option<String> {
    let mut in_block = false;
    for line in config.lines().map(str::trim) {
        let mut words = line
            .split(|c: char| c.is_whitespace() || c == '=')
            .filter(|w| !w.is_empty());
        let Some(keyword) = words.next() else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("host") || keyword.eq_ignore_ascii_case("match") {
            in_block = keyword.eq_ignore_ascii_case("host")
                && words.any(|name| name.eq_ignore_ascii_case(alias));
        } else if in_block && keyword.eq_ignore_ascii_case("hostname") {
            return words.next().map(str::to_owned);
        }
    }
    None
}

/// The commit `origin/<branch>` points to here: what was last pushed to (or
/// fetched from) it. Read from the ref files; `None` when there is none.
pub fn remote_sha(dir: &str, branch: &str) -> Option<String> {
    let common = discover(Path::new(dir))?.common_dir;
    let name = format!("refs/remotes/origin/{branch}");
    if let Ok(text) = std::fs::read_to_string(common.join(&name)) {
        let sha = text.trim();
        return (!sha.is_empty()).then(|| sha.to_owned());
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (sha, reference) = line.split_once(' ')?;
        (reference == name).then(|| sha.to_owned())
    })
}

/// When `origin/<branch>` here last moved (its ref file was written), in
/// epoch milliseconds; `None` when it is packed or there is none.
pub fn remote_moved_ms(dir: &str, branch: &str) -> Option<i64> {
    let common = discover(Path::new(dir))?.common_dir;
    let modified = std::fs::metadata(common.join(format!("refs/remotes/origin/{branch}")))
        .ok()?
        .modified()
        .ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// A repository as found from a directory in its work tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    /// The work tree's own git directory: `.git`, or a linked worktree's
    /// `.git/worktrees/<name>`.
    git_dir: PathBuf,
    /// The repository's shared one, canonical.
    common_dir: PathBuf,
}

/// The repository whose work tree holds `dir`, found as `git` finds it: the
/// nearest `.git` directory or `gitdir:` file at or above `dir`. `None` for a
/// directory that does not exist, is in no work tree, or holds something this
/// does not read the same way `git` would (left to `git` itself).
fn discover(dir: &Path) -> Option<Found> {
    if redirected() {
        return None;
    }
    let start = dir.canonicalize().ok()?;
    if !start.is_dir() {
        return None;
    }
    let mut at = start.as_path();
    loop {
        let dot_git = at.join(".git");
        if let Ok(metadata) = std::fs::metadata(&dot_git) {
            let git_dir = if metadata.is_dir() {
                dot_git
            } else {
                linked_git_dir(&dot_git, at)?
            };
            return found_at(git_dir);
        }
        // A bare repository (or a git directory), by the files `git` looks
        // for: it answers from one, but that is rare enough to leave to it.
        if at.join("HEAD").is_file() && at.join("objects").is_dir() && at.join("refs").is_dir() {
            return None;
        }
        at = at.parent()?;
    }
}

/// The git directory a `.git` file in `work_tree` points to (`gitdir: <path>`,
/// relative to the work tree or absolute).
fn linked_git_dir(dot_git: &Path, work_tree: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dot_git).ok()?;
    let target = text
        .strip_prefix("gitdir: ")?
        .trim_end_matches(['\n', '\r']);
    (!target.is_empty()).then(|| work_tree.join(target))
}

/// `git_dir` and its common dir, if it looks like a git directory.
fn found_at(git_dir: PathBuf) -> Option<Found> {
    if !git_dir.join("HEAD").is_file() {
        return None;
    }
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => git_dir.join(text.trim_end_matches(['\n', '\r'])),
        Err(_) => git_dir.clone(),
    };
    Some(Found {
        common_dir: common.canonicalize().ok()?,
        git_dir,
    })
}

/// The branch `git_dir`'s `HEAD` is on, when it is a plain symbolic ref to a
/// branch. `None` for anything else: detached, a reftable stub, unreadable.
fn branch_in(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let branch = head
        .strip_prefix("ref: refs/heads/")?
        .trim_end_matches(['\n', '\r']);
    // Reftable repositories keep a placeholder `HEAD` naming `.invalid`.
    (!branch.is_empty() && branch != ".invalid" && !branch.contains(char::is_whitespace))
        .then(|| branch.to_owned())
}

/// Whether the environment tells `git` to look elsewhere, or less far, for a
/// repository; then only `git` itself knows what it finds.
fn redirected() -> bool {
    [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some())
}

/// Whether `git` would find no repository from `dir` either: it exists, and
/// neither it nor any directory above it has a `.git` or looks like a git
/// directory itself, and `git` is not pointed elsewhere by the environment.
/// Then there is no need to ask.
fn outside_any_repository(dir: &Path) -> bool {
    if redirected() {
        return false;
    }
    let Ok(dir) = dir.canonicalize() else {
        return false;
    };
    dir.is_dir()
        && dir.ancestors().all(|at| {
            std::fs::symlink_metadata(at.join(".git")).is_err() && !at.join("HEAD").is_file()
        })
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    let ran = crate::process::run(command, GIT_TIMEOUT).ok()?;
    let out = ran.stdout.trim();
    (ran.success && !out.is_empty()).then(|| out.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn github_remotes_in_every_form_name_their_repository() {
        for url in [
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/",
        ] {
            assert_eq!(super::github_name(url).as_deref(), Some("o/r"), "{url}");
        }
        assert_eq!(super::github_name("git@gitlab.com:o/r.git"), None);
        assert_eq!(super::github_name("https://github.com/o"), None);
        assert_eq!(
            super::github_name("https://notgithub.com/o/r"),
            None,
            "the host is github.com, not one ending in it"
        );
        assert_eq!(
            super::github_name("https://github.com.evil.example/o/r"),
            None
        );
    }

    #[test]
    fn an_ssh_alias_is_github_when_ssh_config_says_so() {
        let config = "Host *\n  AddKeysToAgent yes\nHost github.com-work work\n  HostName github.com\n  User git\nHost box\n  HostName example.com\n";
        assert_eq!(
            super::host_name_in(config, "github.com-work").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            super::host_name_in(config, "work").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            super::host_name_in(config, "box").as_deref(),
            Some("example.com")
        );
        assert_eq!(super::host_name_in(config, "other"), None);
    }

    #[test]
    fn the_repository_is_gh_s_default_remote_else_origin() {
        let fork = "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:me/r.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[remote \"upstream\"]\n\turl = https://github.com/org/r.git\n\tgh-resolved = base\n";
        let remotes = super::remotes(fork);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[1].resolved.as_deref(), Some("base"));
        let dir = Scratch::new("fork");
        std::fs::create_dir_all(dir.0.join(".git/refs")).unwrap();
        std::fs::write(dir.0.join(".git/HEAD"), "ref: refs/heads/feat\n").unwrap();
        std::fs::write(dir.0.join(".git/config"), fork).unwrap();
        assert_eq!(
            super::github_remote(dir.0.to_str().unwrap()),
            Some(super::GitHubRemote {
                repo: "org/r".to_owned(),
                head_owner: Some("me".to_owned()),
            })
        );
        let plain = "[remote \"upstream\"]\n\turl = git@github.com:u/r.git\n[remote \"origin\"]\n\turl = git@github.com:o/r.git\n";
        std::fs::write(dir.0.join(".git/config"), plain).unwrap();
        assert_eq!(
            super::github_remote(dir.0.to_str().unwrap()),
            Some(super::GitHubRemote {
                repo: "o/r".to_owned(),
                head_owner: None,
            }),
            "origin's, not another remote's, unless gh chose one"
        );
    }

    use super::*;

    /// A scratch directory of its own, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("sl-core-git-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// `git` in `dir` with no user or system config, as a test fixture.
    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo(at: &Path) {
        std::fs::create_dir_all(at).unwrap();
        run(at, &["init", "-q"]);
        run(at, &["commit", "-q", "--allow-empty", "-m", "first"]);
    }

    /// What the previous implementation, `git` alone, answered.
    fn by_git(dir: &str) -> (Option<String>, Option<String>) {
        let branch = git(dir, &["branch", "--show-current"])
            .or_else(|| git(dir, &["rev-parse", "--short", "HEAD"]).map(|sha| format!("@{sha}")));
        (
            branch,
            git(
                dir,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
        )
    }

    /// The same answers as `git`, for `dir`; and, when on a branch, read
    /// without asking it.
    fn agrees(dir: &Path, expect_branch: Option<&str>) {
        let text = dir.to_str().unwrap();
        let (branch, common) = by_git(text);
        assert_eq!(
            branch.as_deref(),
            expect_branch,
            "{text}: git's own answer, as the case means it"
        );
        assert_eq!(current_branch(Some(text)), branch, "{text}: branch");
        assert_eq!(common_dir(text), common, "{text}: common dir");
        if let Some(found) = discover(dir) {
            if expect_branch.is_some_and(|branch| !branch.starts_with('@')) {
                assert_eq!(
                    branch_in(&found.git_dir),
                    branch,
                    "{text}: read off HEAD, no git"
                );
            }
            assert_eq!(
                Some(found.common_dir.to_string_lossy().into_owned()),
                common,
                "{text}: common dir without git"
            );
        }
    }

    #[test]
    fn a_branch_is_read_off_head_as_git_prints_it() {
        let s = Scratch::new("branch");
        repo(&s.path("repo"));
        agrees(&s.path("repo"), Some("main"));
        run(&s.path("repo"), &["checkout", "-q", "-b", "feat/deep/name"]);
        agrees(&s.path("repo"), Some("feat/deep/name"));
        std::fs::create_dir_all(s.path("repo/a/b")).unwrap();
        agrees(&s.path("repo/a/b"), Some("feat/deep/name"));
    }

    #[test]
    fn a_symlinked_path_resolves_as_git_resolves_it() {
        let s = Scratch::new("symlink");
        repo(&s.path("repo"));
        std::fs::create_dir_all(s.path("repo/sub")).unwrap();
        std::os::unix::fs::symlink(s.path("repo"), s.path("link")).unwrap();
        agrees(&s.path("link/sub"), Some("main"));
    }

    #[test]
    fn a_detached_head_is_git_s_short_sha() {
        let s = Scratch::new("detached");
        repo(&s.path("repo"));
        run(&s.path("repo"), &["checkout", "-q", "--detach"]);
        let (branch, _) = by_git(s.path("repo").to_str().unwrap());
        let branch = branch.unwrap();
        assert!(branch.starts_with('@') && branch.len() >= 8, "{branch}");
        agrees(&s.path("repo"), Some(&branch));
    }

    #[test]
    fn an_unborn_branch_is_named() {
        let s = Scratch::new("unborn");
        std::fs::create_dir_all(s.path("repo")).unwrap();
        run(&s.path("repo"), &["init", "-q", "-b", "trunk"]);
        agrees(&s.path("repo"), Some("trunk"));
    }

    #[test]
    fn a_linked_worktree_has_its_own_branch_and_the_shared_common_dir() {
        let s = Scratch::new("worktree");
        repo(&s.path("repo"));
        run(
            &s.path("repo"),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wt/one",
                s.path("wt").to_str().unwrap(),
            ],
        );
        agrees(&s.path("wt"), Some("wt/one"));
        agrees(&s.path("repo"), Some("main"));
        let found = discover(&s.path("wt")).unwrap();
        assert_eq!(
            found.common_dir,
            s.path("repo/.git"),
            "every worktree shares the main one's"
        );
        // A worktree linked by a relative path (`--relative-paths`).
        run(
            &s.path("repo"),
            &[
                "worktree",
                "add",
                "-q",
                "--relative-paths",
                "-b",
                "wt/two",
                s.path("rel").to_str().unwrap(),
            ],
        );
        assert!(
            std::fs::read_to_string(s.path("rel/.git"))
                .unwrap()
                .starts_with("gitdir: ../")
        );
        agrees(&s.path("rel"), Some("wt/two"));
    }

    #[test]
    fn outside_a_repository_there_is_nothing() {
        let s = Scratch::new("none");
        std::fs::create_dir_all(s.path("plain/dir")).unwrap();
        agrees(&s.path("plain/dir"), None);
        assert!(
            outside_any_repository(&s.path("plain/dir")),
            "answered without git"
        );
        // A directory that does not exist is not looked for above it.
        repo(&s.path("repo"));
        agrees(&s.path("repo/missing"), None);
        assert_eq!(current_branch(None), None);
    }

    #[test]
    fn a_bare_repository_or_a_git_dir_is_left_to_git() {
        let s = Scratch::new("bare");
        repo(&s.path("repo"));
        run(
            &s.path("repo"),
            &[
                "clone",
                "-q",
                "--bare",
                ".",
                s.path("bare.git").to_str().unwrap(),
            ],
        );
        assert_eq!(discover(&s.path("bare.git")), None);
        assert!(!outside_any_repository(&s.path("bare.git")));
        agrees(&s.path("bare.git"), Some("main"));
        assert!(!outside_any_repository(&s.path("repo/.git/refs")));
        let text = s.path("repo/.git/refs");
        assert_eq!(
            current_branch(text.to_str()),
            by_git(text.to_str().unwrap()).0,
            "inside .git: as git says"
        );
    }

    #[test]
    fn a_work_tree_file_named_head_is_not_a_bare_repository() {
        let s = Scratch::new("head-file");
        repo(&s.path("repo"));
        std::fs::create_dir_all(s.path("repo/docs/objects")).unwrap();
        std::fs::write(s.path("repo/docs/HEAD"), "not git's").unwrap();
        std::fs::create_dir_all(s.path("repo/data/objects")).unwrap();
        for dir in ["repo/docs", "repo/data/objects"] {
            assert!(
                discover(&s.path(dir)).is_some(),
                "{dir}: found the work tree above it"
            );
            agrees(&s.path(dir), Some("main"));
        }
    }

    /// Set in the child [`git_s_environment_is_left_to_git`] starts.
    const PROBE: &str = "SL_CORE_GIT_REDIRECT_PROBE";

    /// With `GIT_DIR` (or any of its kind) set, `git` decides where the
    /// repository is, so nothing is read here and `git` is asked. The
    /// environment cannot be changed in-process without `unsafe`, so the test
    /// runs itself again in a child with it set.
    #[test]
    fn git_s_environment_is_left_to_git() {
        if let Some(repo_dir) = std::env::var_os(PROBE) {
            let repo_dir = PathBuf::from(repo_dir);
            assert!(redirected(), "GIT_DIR is set");
            assert_eq!(discover(&repo_dir), None, "not looked for");
            assert!(!outside_any_repository(Path::new("/")), "and not ruled out");
            return;
        }
        let s = Scratch::new("redirect");
        repo(&s.path("repo"));
        assert!(discover(&s.path("repo")).is_some());
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git::tests::git_s_environment_is_left_to_git",
                "--nocapture",
            ])
            .env(PROBE, s.path("repo"))
            .env("GIT_DIR", s.path("repo/.git"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "the child: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn head_files_this_does_not_know_are_not_read() {
        let s = Scratch::new("head");
        let git_dir = s.path(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let head = |text: &str| {
            std::fs::write(git_dir.join("HEAD"), text).unwrap();
            branch_in(&git_dir)
        };
        assert_eq!(head("ref: refs/heads/main\n").as_deref(), Some("main"));
        assert_eq!(head("ref: refs/heads/a/b\r\n").as_deref(), Some("a/b"));
        assert_eq!(head("ref: refs/heads/.invalid\n"), None, "a reftable stub");
        assert_eq!(
            head("ref: refs/remotes/origin/main\n"),
            None,
            "not a branch"
        );
        assert_eq!(
            head("0123456789abcdef0123456789abcdef01234567\n"),
            None,
            "detached"
        );
        assert_eq!(head("ref: refs/heads/\n"), None);
        assert_eq!(head(""), None);
    }
}
