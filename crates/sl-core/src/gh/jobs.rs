//! A GitHub Actions job, named by its link (`github.com/o/r/actions/runs/R/job/J`):
//! the log of its failed steps, and re-running it.

use std::fmt::Write as _;

use serde_json::Value;

use super::api::{self, Failure};

/// A job's log or JSON is fetched within this long.
const SECS: u64 = 60;

/// The repository (`owner/name`) and job id a check's link points to.
fn job_of(link: &str) -> Option<(String, u64)> {
    let (_, job) = super::run_and_job_ids(link)?;
    let repo = crate::git::github_name(link.split("/actions/").next()?)?;
    Some((repo, job))
}

const NOT_A_JOB: &str = "This check is not a GitHub Actions job.";

/// The log of the job's failed steps, or the whole log when the steps cannot
/// be told apart in it.
///
/// # Errors
///
/// A link that is not a job's, or GitHub not giving the job or its log.
pub fn failed_log(link: &str) -> Result<String, String> {
    let (repo, job) = job_of(link).ok_or_else(|| NOT_A_JOB.to_owned())?;
    let fail = |f: Failure| f.to_string();
    let info = api::rest("GET", &format!("repos/{repo}/actions/jobs/{job}"), SECS)
        .and_then(api::Response::ok)
        .map_err(fail)?;
    // The log is a redirect to storage, followed without the token.
    let response = api::rest(
        "GET",
        &format!("repos/{repo}/actions/jobs/{job}/logs"),
        SECS,
    )
    .map_err(fail)?;
    let log = match response.location.clone() {
        Some(url) if (300..400).contains(&response.status) => {
            api::download(&url, SECS).map_err(fail)?
        }
        _ => response.ok().map_err(fail)?,
    };
    let steps = serde_json::from_str::<Value>(&info)
        .map(|v| failed_steps(&v))
        .unwrap_or_default();
    Ok(failed_part(&log, &steps).unwrap_or(log))
}

/// Ask GitHub to run the job again.
///
/// # Errors
///
/// A link that is not a job's, or GitHub refusing, with its reason (a run
/// still in progress, say).
pub fn rerun(link: &str) -> Result<String, String> {
    let (repo, job) = job_of(link).ok_or_else(|| NOT_A_JOB.to_owned())?;
    api::rest(
        "POST",
        &format!("repos/{repo}/actions/jobs/{job}/rerun"),
        SECS,
    )
    .and_then(api::Response::ok)
    .map(|_| format!("Re-run requested for job {job}."))
    .map_err(|f| f.to_string())
}

/// A failed step: its name, and the seconds (since the epoch) it started and
/// finished in. GitHub gives a step's times with an offset
/// (`2020-01-20T09:42:40.000-08:00`) and stamps log lines in UTC, so they are
/// compared as instants, not as text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    name: String,
    from: i64,
    to: i64,
}

/// The second an ISO 8601 time falls in, since the epoch.
fn epoch_second(time: &str) -> Option<i64> {
    Some(crate::format::parse_iso8601_to_epoch_ms(time)?.div_euclid(1000))
}

fn failed_steps(job: &Value) -> Vec<Step> {
    job.get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("conclusion").and_then(Value::as_str) == Some("failure"))
        .filter_map(|s| {
            let second = |key| epoch_second(s.get(key)?.as_str()?);
            Some(Step {
                name: s.get("name")?.as_str()?.to_owned(),
                from: second("started_at")?,
                to: second("completed_at")?,
            })
        })
        .collect()
}

/// A log line's timestamp, to the second, and the line without it.
fn stamped(line: &str) -> Option<(i64, &str)> {
    let (stamp, rest) = line.split_once(' ')?;
    if !(stamp.ends_with('Z') && stamp.as_bytes().get(10) == Some(&b'T')) {
        return None;
    }
    Some((epoch_second(stamp)?, rest))
}

/// The lines of `log` written while a failed step ran, under each step's
/// name. `None` when no line falls in any, so the caller shows it all.
fn failed_part(log: &str, steps: &[Step]) -> Option<String> {
    let mut out = String::new();
    for step in steps {
        let mut inside = false;
        let mut lines = Vec::new();
        for line in log.lines() {
            let text = match stamped(line) {
                Some((second, text)) => {
                    inside = step.from <= second && second <= step.to;
                    text
                }
                // A line without a stamp goes on the one before.
                None => line,
            };
            if inside {
                lines.push(text);
            }
        }
        if !lines.is_empty() {
            writeln!(out, "── {} ──\n{}", step.name, lines.join("\n")).ok();
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_link_names_its_repository_and_job() {
        assert_eq!(
            job_of("https://github.com/o/r/actions/runs/1/job/2?pr=3"),
            Some(("o/r".to_owned(), 2))
        );
        assert_eq!(job_of("https://github.com/o/r/runs/2"), None);
    }

    #[test]
    fn only_the_failed_steps_lines_are_kept_under_their_names() {
        let job: Value = serde_json::from_str(
            r#"{"steps":[
                {"name":"Set up","conclusion":"success","started_at":"2026-01-01T10:00:00Z","completed_at":"2026-01-01T10:00:05Z"},
                {"name":"Test","conclusion":"failure","started_at":"2026-01-01T10:00:06Z","completed_at":"2026-01-01T10:00:09Z"},
                {"name":"Post","conclusion":"success","started_at":"2026-01-01T10:00:10Z","completed_at":"2026-01-01T10:00:11Z"}]}"#,
        )
        .unwrap();
        let steps = failed_steps(&job);
        assert_eq!(steps.len(), 1);
        let log = "2026-01-01T10:00:01.1234567Z setting up\n\
                   2026-01-01T10:00:07.0000000Z running tests\n\
                   continued without a stamp\n\
                   2026-01-01T10:00:09.9000000Z ##[error]1 failed\n\
                   2026-01-01T10:00:10.5000000Z cleaning up\n";
        assert_eq!(
            failed_part(log, &steps).unwrap(),
            "── Test ──\nrunning tests\ncontinued without a stamp\n##[error]1 failed\n"
        );
        assert_eq!(failed_part("no stamps here", &steps), None);
    }

    #[test]
    fn a_step_timed_with_an_offset_finds_its_utc_stamped_lines() {
        let job: Value = serde_json::from_str(
            r#"{"steps":[{"name":"Test","conclusion":"failure",
                "started_at":"2026-01-01T02:00:06.000-08:00","completed_at":"2026-01-01T02:00:09.000-08:00"}]}"#,
        )
        .unwrap();
        let log = "2026-01-01T10:00:01.0000000Z before\n2026-01-01T10:00:07.0000000Z failing\n";
        assert_eq!(
            failed_part(log, &failed_steps(&job)).unwrap(),
            "── Test ──\nfailing\n"
        );
    }
}
