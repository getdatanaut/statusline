//! Running a program with a time limit.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What a finished program left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// It exited with status 0.
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Why a program left nothing.
#[derive(Debug)]
pub enum RunError {
    /// It could not be started.
    Spawn(std::io::Error),
    /// It was still running after the time limit, and was killed.
    TimedOut,
}

/// Run `command` (stdin closed, stdout and stderr captured), killing it after
/// `timeout`. Both pipes are drained while it runs, so a program with a lot to
/// say is not stalled on a full pipe until the limit.
///
/// # Errors
///
/// [`RunError::Spawn`] if it cannot be started, [`RunError::TimedOut`] if it
/// outlived `timeout` (or could no longer be waited for) and was killed. A
/// program that ran and failed is an `Ok` whose `success` is false.
pub fn run(mut command: Command, timeout: Duration) -> Result<Ran, RunError> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(RunError::Spawn)?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            // Whatever was read before an error is still worth having.
            if let Some(mut pipe) = pipe {
                pipe.read_to_end(&mut buf).ok();
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let start = Instant::now();
    // Short waits first: most of what runs here is done in a few
    // milliseconds, and a status line is waiting on it.
    let mut nap = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if start.elapsed() < timeout => {
                std::thread::sleep(nap);
                nap = (nap * 2).min(Duration::from_millis(25));
            }
            // Timed out, or it can no longer be waited for.
            Ok(None) | Err(_) => {
                child.kill().ok();
                child.wait().ok();
                break None;
            }
        }
    };
    // The pipes close once the child is gone (unless it left a grandchild
    // holding them, which the time limit does not cover either).
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let status = status.ok_or(RunError::TimedOut)?;
    Ok(Ran { success: status.success(), stdout, stderr })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn output_and_status_come_back() {
        let ran = run(sh("echo out; echo err >&2; exit 3"), Duration::from_secs(5)).unwrap();
        assert_eq!(ran, Ran { success: false, stdout: "out\n".to_owned(), stderr: "err\n".to_owned() });
        assert!(run(sh("true"), Duration::from_secs(5)).unwrap().success);
    }

    #[test]
    fn more_than_a_pipe_holds_does_not_stall() {
        // 1 MB on each pipe: far past the 64 KB a pipe buffers.
        let start = Instant::now();
        let ran = run(sh("head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2"), Duration::from_secs(5)).unwrap();
        assert!(ran.success);
        assert_eq!((ran.stdout.len(), ran.stderr.len()), (1 << 20, 1 << 20));
        assert!(start.elapsed() < Duration::from_secs(4), "not held until the limit: {:?}", start.elapsed());
    }

    #[test]
    fn a_program_past_its_limit_is_killed() {
        let start = Instant::now();
        assert!(matches!(run(sh("sleep 5"), Duration::from_millis(100)), Err(RunError::TimedOut)));
        assert!(start.elapsed() < Duration::from_secs(2), "killed, not waited for: {:?}", start.elapsed());
    }

    #[test]
    fn a_program_that_cannot_start_says_so() {
        assert!(matches!(run(Command::new("/nonexistent/program"), Duration::from_secs(1)), Err(RunError::Spawn(_))));
    }
}
