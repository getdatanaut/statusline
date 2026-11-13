//! HTTPS requests with `/usr/bin/curl`: at a fixed path, so a bare `PATH`
//! does not matter, and without a TLS stack compiled in. Shared by GitHub
//! ([`crate::gh::api`]) and the tray's Linear lookups.
//!
//! A secret header (a token, an API key) is handed to curl on stdin, never
//! on its command line, where any process could read it.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

const CURL: &str = "/usr/bin/curl";

static OFFLINE: AtomicBool = AtomicBool::new(false);

/// Never touch the network again in this process: for the tray's self-test,
/// whose stubbed `PATH` cannot catch `/usr/bin/curl`.
pub fn go_offline() {
    OFFLINE.store(true, Ordering::SeqCst);
}

/// Whether [`go_offline`] was called.
pub fn is_offline() -> bool {
    OFFLINE.load(Ordering::SeqCst)
}

/// What came back from one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
    /// Where a redirect points, which is not followed.
    pub location: Option<String>,
}

impl Response {
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `HTTP <status>: <message>`, the message from the JSON body's
    /// `message` (GitHub) or first `errors[].message` (GraphQL), else the
    /// start of the body.
    #[must_use]
    pub fn error_text(&self) -> String {
        let value = serde_json::from_str::<Value>(&self.body).ok();
        let message = value
            .as_ref()
            .and_then(|v| {
                v.get("message")
                    .or_else(|| v.pointer("/errors/0/message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| self.body.chars().take(200).collect());
        format!("HTTP {}: {message}", self.status)
    }
}

/// One request: `method` to `url`, with `body` as JSON if given, `secret`
/// (a whole header line, `Authorization: …`) sent through stdin, and
/// `headers` on the command line. Redirects are not followed. curl gives up
/// after `secs`.
///
/// # Errors
///
/// Offline, or curl failing (no connection, a timeout): its message.
pub fn send(
    method: &str,
    url: &str,
    body: Option<&str>,
    secret: Option<&str>,
    headers: &[&str],
    secs: u64,
) -> Result<Response, String> {
    if is_offline() {
        return Err("offline".to_owned());
    }
    let mut command = Command::new(CURL);
    command.args([
        "--silent",
        "--show-error",
        "--max-time",
        &secs.to_string(),
        "--request",
        method,
        // The secret header, if any, read from stdin.
        "--header",
        "@-",
        "--write-out",
        "\n%{http_code}\n%{redirect_url}",
    ]);
    for header in headers {
        command.args(["--header", header]);
    }
    if let Some(body) = body {
        command.args([
            "--header",
            "Content-Type: application/json",
            "--data-raw",
            body,
        ]);
    }
    command.arg(url);
    let stdin = secret.map_or_else(String::new, |line| format!("{line}\n"));
    let ran =
        crate::process::run_with_input(command, stdin.as_bytes(), Duration::from_secs(secs + 5))
            .map_err(|error| format!("curl: {error:?}"))?;
    if !ran.success {
        return Err(ran.stderr.trim().to_owned());
    }
    parse_output(&ran.stdout)
}

/// The body, then the two lines `--write-out` added.
fn parse_output(stdout: &str) -> Result<Response, String> {
    let mut parts = stdout.rsplitn(3, '\n');
    let (Some(location), Some(code), Some(body)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err("no status from curl".to_owned());
    };
    let status = code
        .trim()
        .parse()
        .map_err(|_not_a_number| format!("no status from curl: {code:?}"))?;
    Ok(Response {
        status,
        body: body.to_owned(),
        location: Some(location.trim())
            .filter(|l| !l.is_empty())
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curls_output_splits_into_body_status_and_redirect() {
        let response = parse_output("{\"a\":1}\n200\n").unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "{\"a\":1}");
        assert_eq!(response.location, None);
        let moved = parse_output("\n302\nhttps://x.test/log").unwrap();
        assert_eq!(moved.location.as_deref(), Some("https://x.test/log"));
        assert!(parse_output("nothing").is_err());
    }

    #[test]
    fn an_error_names_its_status_and_message() {
        let github = Response {
            status: 404,
            body: "{\"message\":\"Not Found\"}".to_owned(),
            location: None,
        };
        assert_eq!(github.error_text(), "HTTP 404: Not Found");
        let graphql = Response {
            status: 400,
            body: "{\"errors\":[{\"message\":\"Bad query\"}]}".to_owned(),
            location: None,
        };
        assert_eq!(graphql.error_text(), "HTTP 400: Bad query");
        let html = Response {
            status: 502,
            body: "<html>bad gateway</html>".to_owned(),
            location: None,
        };
        assert_eq!(html.error_text(), "HTTP 502: <html>bad gateway</html>");
    }
}
