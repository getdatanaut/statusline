//! The branch segment, end to end: the binary, given a payload whose cwd is a
//! scratch repository. A branch is read off the repository without running
//! `git` (none is on the `PATH`); a detached `HEAD` still asks `git` for its
//! short sha. Default branches and detached heads never start a `gh` fetch,
//! so nothing outlives the test.

#![expect(clippy::unwrap_used, reason = "fixtures, which fail the test loudly")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use statusline::ansi::strip_ansi;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("statusline-branch-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for sub in ["repo", "home", "cache", "bin"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
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
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The rendered line, run with `path` as the whole `PATH`.
fn render(scratch: &Scratch, path: &str) -> String {
    render_with(scratch, path, "")
}

/// [`render`] with `extra` (`,"key":value` pairs) added to the payload.
fn render_with(scratch: &Scratch, path: &str, extra: &str) -> String {
    let payload = format!(
        r#"{{"model":{{"display_name":"M"}},"workspace":{{"current_dir":{:?}}}{extra}}}"#,
        scratch.0.join("repo").to_str().unwrap()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_statusline"))
        .env_clear()
        .env("HOME", scratch.0.join("home"))
        .env("XDG_CACHE_HOME", scratch.0.join("cache"))
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), payload.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "the status line runs");
    strip_ansi(&String::from_utf8(out.stdout).unwrap())
}

#[test]
fn a_branch_is_shown_without_running_git() {
    let scratch = Scratch::new("branch");
    let repo = scratch.0.join("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
    // A `PATH` with nothing on it: no `git` to run.
    let line = render(&scratch, scratch.0.join("bin").to_str().unwrap());
    assert!(line.contains("main"), "the branch, read off HEAD: {line:?}");
}

#[test]
fn the_checked_out_branch_wins_over_the_one_the_worktree_was_made_on() {
    let scratch = Scratch::new("worktree");
    let repo = scratch.0.join("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
    let line = render_with(
        &scratch,
        scratch.0.join("bin").to_str().unwrap(),
        r#","worktree":{"name":"w","branch":"worktree-w"}"#,
    );
    assert!(line.ends_with("| main\n"), "HEAD's branch: {line:?}");
    assert!(!line.contains("worktree-w"), "{line:?}");
}

#[test]
fn a_detached_head_shows_git_s_short_sha() {
    let scratch = Scratch::new("detached");
    let repo = scratch.0.join("repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
    git(&repo, &["checkout", "-q", "--detach"]);
    let sha = git(&repo, &["rev-parse", "--short", "HEAD"]);
    let line = render(&scratch, "/usr/bin:/bin");
    assert!(line.contains(&format!("@{sha}")), "@{sha} in {line:?}");
}
