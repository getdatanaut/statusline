//! What the session submenus do when clicked: fetch a review, read a job's
//! logs, re-run a job, open a PR.
//!
//! Every action runs a subprocess (`gh`, or `zed`), which can take
//! seconds, so each runs on its own thread and hands its result back through
//! an [`Outbox`]. The main thread drains the outbox on its timer tick and does
//! the AppKit part (clipboard, popup); nothing here touches AppKit.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sl_core::gh::run_and_job_ids;

/// What a finished action asks the main thread to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Put this text on the clipboard.
    Copy(String),
    /// Show this text in a popup with a Copy button. `markdown` bodies are
    /// rendered; anything else (logs) is shown as plain monospaced text.
    Popup {
        title: String,
        body: String,
        markdown: bool,
    },
}

pub type Outbox = Arc<Mutex<Vec<Outcome>>>;

/// How long a subprocess may run before it is killed.
const TIMEOUT: Duration = Duration::from_secs(60);
/// A popup shows at most this much of a log: its tail, where the failure is.
const MAX_LOG_BYTES: usize = 200_000;

fn push(outbox: &Outbox, outcome: Outcome) {
    outbox
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(outcome);
}

/// Take everything queued so far.
pub fn drain(outbox: &Outbox) -> Vec<Outcome> {
    std::mem::take(
        &mut *outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// `PATH` with the usual tool locations added: a tray started outside a shell
/// has a bare `PATH`, and `gh`, `linear` and `zed` live in Homebrew's.
fn search_path() -> String {
    let home = dirs::home_dir().unwrap_or_default();
    let mut dirs: Vec<String> = vec![
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
        home.join(".local/bin").display().to_string(),
        home.join(".cargo/bin").display().to_string(),
    ];
    if let Ok(existing) = std::env::var("PATH") {
        dirs.push(existing);
    }
    dirs.join(":")
}

/// Run `program args` in `cwd`, returning stdout, or the error text (stderr,
/// else stdout) if it fails, cannot start, or outlives [`TIMEOUT`].
fn run(program: &str, args: &[&str], cwd: &str) -> Result<String, String> {
    run_for(program, args, cwd, TIMEOUT)
}

/// As [`run`], killing the process after `timeout`.
pub(crate) fn run_for(
    program: &str,
    args: &[&str],
    cwd: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("PATH", search_path());
    match sl_core::process::run(command, timeout) {
        Ok(ran) if ran.success => Ok(ran.stdout),
        Ok(ran) if ran.stderr.trim().is_empty() => Err(ran.stdout),
        Ok(ran) => Err(ran.stderr),
        Err(sl_core::process::RunError::Spawn(error)) => {
            Err(format!("cannot run {program}: {error}"))
        }
        Err(sl_core::process::RunError::TimedOut) => {
            Err(format!("{program} timed out after {}s", timeout.as_secs()))
        }
    }
}

/// Actions started and not yet finished.
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// Whether an action is still running: its result is wanted on screen
/// promptly, so the main thread ticks fast until it is handed over.
pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed) > 0
}

/// Counts an action as running for as long as it lives.
struct Running;

impl Running {
    fn start() -> Self {
        RUNNING.fetch_add(1, Ordering::Relaxed);
        Running
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::Relaxed);
    }
}

fn spawn(work: impl FnOnce() + Send + 'static) {
    let running = Running::start();
    // A thread that cannot start is an action that does nothing, as a
    // subprocess that cannot start is; it drops `running` with it.
    std::thread::Builder::new()
        .name("sl-tray-action".to_owned())
        .spawn(move || {
            let _running = running;
            work();
        })
        .ok();
}

/// The PR's unresolved review feedback as markdown.
fn review_text(cwd: &str, pr: i64) -> Result<String, String> {
    crate::feedback::markdown(cwd, pr, crate::snapshot::now_ms())
}

pub fn copy_review(outbox: Outbox, cwd: String, pr: i64) {
    if crate::effects::intercept(crate::effects::Effect::CopyReview {
        cwd: cwd.clone(),
        pr,
    }) {
        return;
    }
    spawn(move || {
        push(
            &outbox,
            match review_text(&cwd, pr) {
                Ok(text) => Outcome::Copy(text),
                Err(error) => Outcome::Popup {
                    title: format!("Could not get review feedback for #{pr}"),
                    body: error,
                    markdown: false,
                },
            },
        );
    });
}

/// Where review feedback is written to be opened in Zed: a directory of the
/// user's own in their temporary directory (`$TMPDIR`, which macOS keeps
/// private to them), not a shared, predictable one in `/tmp`.
fn review_dir() -> PathBuf {
    std::env::temp_dir().join("statusline")
}

/// The file a PR's review feedback is written to: named for the session's
/// directory as well as the PR, as two repositories can share a number.
fn review_file(dir: &Path, cwd: &str, pr: i64) -> PathBuf {
    let name = Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    dir.join(format!("{name}-pr{pr}-review.md"))
}

/// Make `dir` if it is missing, as a directory only its owner may enter. One
/// that is a symlink or not a directory is refused, and so is one another
/// user owns: its mode cannot be set.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    if !std::fs::symlink_metadata(dir)?.is_dir() {
        return Err(std::io::Error::other("not a directory"));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Tells apart two writes of one file under way at once.
static WRITES: AtomicUsize = AtomicUsize::new(0);

/// Write `text` to `path`, readable by its owner alone. It goes to a new file
/// beside `path` that is then renamed over it, so a symlink at `path` is
/// replaced rather than written through.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(name);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        std::fs::remove_file(&tmp).ok();
    }
    written
}

/// The review feedback, written to a file in [`review_dir`] and opened in
/// Zed. Each run rewrites the file, so it always holds the latest feedback.
pub fn open_review_in_zed(outbox: Outbox, cwd: String, pr: i64) {
    if crate::effects::intercept(crate::effects::Effect::OpenReviewInZed {
        cwd: cwd.clone(),
        pr,
    }) {
        return;
    }
    spawn(move || {
        let dir = review_dir();
        let path = review_file(&dir, &cwd, pr);
        let written = review_text(&cwd, pr).and_then(|text| {
            private_dir(&dir)
                .and_then(|()| write_private(&path, &text))
                .map_err(|error| format!("{}: {error}", path.display()))
        });
        match written {
            Ok(()) => open_in_zed(&path.to_string_lossy()),
            Err(error) => push(
                &outbox,
                Outcome::Popup {
                    title: format!("Could not get review feedback for #{pr}"),
                    body: error,
                    markdown: false,
                },
            ),
        }
    });
}

/// The failed steps' logs of one job. A check from another app has no job id
/// and no logs `gh` can fetch.
pub fn show_logs(outbox: Outbox, cwd: String, name: String, link: Option<String>) {
    if crate::effects::intercept(crate::effects::Effect::ShowLogs {
        cwd: cwd.clone(),
        job: name.clone(),
        link: link.clone(),
    }) {
        return;
    }
    spawn(move || {
        let title = format!("Logs · {name}");
        let body = match link.as_deref().and_then(run_and_job_ids) {
            None => "This check is not a GitHub Actions job, so there are no logs to fetch here."
                .to_owned(),
            Some((_, job)) => match run(
                "gh",
                &["run", "view", "--job", &job.to_string(), "--log-failed"],
                &cwd,
            ) {
                Ok(log) if log.trim().is_empty() => {
                    "The job has no failed-step log yet.".to_owned()
                }
                Ok(log) => tail(&log, MAX_LOG_BYTES),
                Err(error) => error,
            },
        };
        push(
            &outbox,
            Outcome::Popup {
                title,
                body,
                markdown: false,
            },
        );
    });
}

/// Re-run one job (`gh run rerun --job`), and report whether GitHub took it.
pub fn retry_job(outbox: Outbox, cwd: String, name: String, link: Option<String>) {
    if crate::effects::intercept(crate::effects::Effect::RetryJob {
        cwd: cwd.clone(),
        job: name.clone(),
        link: link.clone(),
    }) {
        return;
    }
    spawn(move || {
        let title = format!("Re-run · {name}");
        let body = match link.as_deref().and_then(run_and_job_ids) {
            None => "This check is not a GitHub Actions job, so it cannot be re-run from here."
                .to_owned(),
            Some((_, job)) => match run("gh", &["run", "rerun", "--job", &job.to_string()], &cwd) {
                Ok(out) if out.trim().is_empty() => format!("Re-run requested for job {job}."),
                Ok(out) => out,
                // `gh` explains itself, e.g. that the run is still in progress.
                Err(error) => error,
            },
        };
        push(
            &outbox,
            Outcome::Popup {
                title,
                body,
                markdown: false,
            },
        );
    });
}

/// Open a directory or file in Zed. A session's directory is already the PR branch's
/// git worktree, so this opens that worktree. Falls back to `open -a Zed` when
/// the `zed` command is not on the path.
pub fn open_in_zed(path: &str) {
    if crate::effects::intercept(crate::effects::Effect::OpenInZed(path.to_owned())) {
        return;
    }
    let mut zed = Command::new("zed");
    zed.arg(path).env("PATH", search_path());
    if launch(zed).is_err() {
        let mut open = Command::new("open");
        open.args(["-a", "Zed", path])
            .env("PATH", search_path());
        launch(open).ok();
    }
}

/// Open a URL in the default browser.
pub fn open_url(url: &str) {
    if crate::effects::intercept(crate::effects::Effect::OpenUrl(url.to_owned())) {
        return;
    }
    let mut open = Command::new("open");
    open.arg(url);
    launch(open).ok();
}

/// Start `command` with no input or output and leave it running, waited for
/// on a thread of its own so it does not stay a zombie once it exits. An
/// error only when it cannot be started.
fn launch(mut command: Command) -> std::io::Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // A thread that cannot start leaves the child unwaited, as before.
    std::thread::Builder::new()
        .name("sl-tray-reap".to_owned())
        .spawn(move || {
            child.wait().ok();
        })
        .ok();
    Ok(())
}

/// The last `max` bytes of `text`, cut at a line boundary and marked.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let rest = text.get(start..).unwrap_or(text);
    let rest = rest.split_once('\n').map_or(rest, |(_, after)| after);
    format!("… (earlier output omitted)\n{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_logs_pass_through_and_long_ones_keep_the_tail_from_a_line_start() {
        assert_eq!(tail("a\nb", 100), "a\nb");
        let long = "first line\nsecond line\nthird line\n";
        let cut = tail(long, 20);
        assert!(cut.starts_with("… (earlier output omitted)\n"));
        assert!(cut.ends_with("third line\n"));
        assert!(!cut.contains("first"));
    }

    #[test]
    fn an_action_counts_as_running_until_its_thread_is_done() {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (done, finished) = std::sync::mpsc::channel::<()>();
        spawn(move || {
            wait.recv().ok();
            done.send(()).ok();
        });
        assert!(running());
        go.send(()).unwrap();
        finished.recv().unwrap();
        let start = std::time::Instant::now();
        while running() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!running(), "the count drops when the thread ends");
    }

    #[test]
    fn the_outbox_hands_over_everything_once() {
        let outbox: Outbox = Arc::default();
        push(&outbox, Outcome::Copy("x".into()));
        push(&outbox, Outcome::Copy("y".into()));
        assert_eq!(
            drain(&outbox),
            vec![Outcome::Copy("x".into()), Outcome::Copy("y".into())]
        );
        assert!(drain(&outbox).is_empty());
    }

    #[test]
    fn a_launched_program_is_waited_for_and_a_missing_one_is_an_error() {
        let mut true_ = Command::new("true");
        true_.current_dir("/tmp");
        assert!(launch(true_).is_ok(), "true starts");
        assert!(
            launch(Command::new("definitely-not-a-program-xyz")).is_err(),
            "a missing program cannot start"
        );
    }

    #[test]
    fn review_files_go_in_a_private_directory_and_replace_a_planted_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("statusline");
        private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700, "made private");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700, "an open one is closed again");

        let path = review_file(&dir, "/x/repo", 12);
        assert_eq!(path, dir.join("repo-pr12-review.md"));
        let target = root.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        write_private(&path, "feedback").unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "keep",
            "not written through"
        );
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "feedback");
        assert_eq!(mode(&path), 0o600, "readable by its owner alone");
        write_private(&path, "newer").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "newer",
            "rewritten"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary file left"
        );

        let link = root.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(
            private_dir(&link).is_err(),
            "a symlinked directory is refused"
        );
    }

    #[test]
    fn run_reports_stdout_failures_and_missing_programs() {
        assert_eq!(run("echo", &["hi"], "/tmp").unwrap().trim(), "hi");
        assert!(
            run("sh", &["-c", "echo boom >&2; exit 3"], "/tmp")
                .unwrap_err()
                .contains("boom")
        );
        assert!(
            run("definitely-not-a-program-xyz", &[], "/tmp")
                .unwrap_err()
                .contains("cannot run")
        );
    }
}
