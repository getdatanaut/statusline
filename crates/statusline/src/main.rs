#![forbid(unsafe_code)]
#![expect(clippy::print_stdout, reason = "the status line is what this prints")]
use statusline::gh;
use statusline::git::current_branch;
use statusline::context::capture_context;
use statusline::limits::capture_rate_limits;
use statusline::render::{render, RenderInputs, RenderOptions};
use statusline::settings::resolve_auto_compact_window;
use statusline::state::{bump_session_state, default_state_dir, read_session_state};
use statusline::types::{HookPayload, StatusPayload};
use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    if run().is_err() {
        // A broken statusline must never spam the UI with stack traces.
        println!("statusline error");
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--gh-refresh") {
        // Detached child spawned by a render; see `gh` module docs.
        if let (Some(dir), Some(branch)) = (args.get(2), args.get(3)) {
            gh::run_refresh(&gh::default_cache_dir(), dir, branch, now_ms());
        }
        return Ok(());
    }

    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let now_ms = now_ms();

    if std::env::args().any(|arg| arg == "--hook") {
        if let Ok(hook) = serde_json::from_str::<HookPayload>(&raw)
            && let Some(session_id) = hook.session_id
        {
            bump_session_state(&default_state_dir(), &session_id, hook.tool_name.as_deref(), now_ms);
        }
        return Ok(()); // hooks must stay silent
    }

    let payload: StatusPayload = serde_json::from_str(&raw).unwrap_or_default();
    let dir = payload
        .workspace
        .as_ref()
        .and_then(|w| w.current_dir.clone())
        .or_else(|| payload.cwd.clone());
    let columns: Option<usize> = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).filter(|&c| c > 0);

    let session_state = payload
        .session_id
        .as_ref()
        .and_then(|id| read_session_state(&default_state_dir(), id));
    let branch = payload
        .worktree
        .as_ref()
        .and_then(|w| w.branch.clone())
        .or_else(|| current_branch(dir.as_deref()));
    let project_dir = payload
        .workspace
        .as_ref()
        .and_then(|w| w.project_dir.clone())
        .or_else(|| dir.clone());
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let auto_compact_window = resolve_auto_compact_window(project_dir.as_deref(), &home);

    let gh = match (dir.as_deref(), branch.as_deref()) {
        (Some(dir), Some(branch)) => gh::load_and_refresh(&gh::default_cache_dir(), dir, branch, now_ms),
        _ => None,
    };

    let options = RenderOptions {
        hidden: flag(&args, "--hide").map(|names| names.split(',').map(|s| s.trim().to_string()).collect()).unwrap_or_default(),
        user: std::env::var("USER").ok(),
        linear_workspace: flag(&args, "--linear-workspace").map(String::from),
    };

    let inputs = RenderInputs { now_ms, session_state, branch, auto_compact_window, gh, options };
    println!("{}", render(&payload, &inputs, columns));

    // Last, and after the line has been printed: the tray's rate-limit capture
    // is a side effect of the status line running, never a precondition of it
    // printing. `println!` writes through a `LineWriter`, so the line is
    // already flushed by the time this touches the filesystem, and everything
    // in `capture_rate_limits` is best-effort and silent (see its module docs).
    // Not reached on the `--hook` path above, which returns early and must
    // stay silent -- and which is handed a different payload anyway.
    capture_rate_limits(&sl_core::paths::cache_dir(), &raw, now_ms);
    // And each session's context use, for the ring on its row in the tray.
    capture_context(&sl_core::paths::cache_dir(), &payload, auto_compact_window, now_ms);
    Ok(())
}

/// The value after `name`: `--hide cache,api` gives `cache,api`.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].as_str())
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}
