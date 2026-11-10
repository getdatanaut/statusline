# statusline

A fast status line for Claude Code, and a macOS menu bar tray for Claude Code
and Codex usage and sessions, written in Rust.

```
Session ⠦⠤⠤⠤⠤⠤ 3% 4h 34m | Weekly ⠿⠿⠷⠤⠤⠤ 45% 13h 14m | Context ⠶⠤⠤⠤⠤⠤ 46.1k/600k | cache 18k hit / 2k miss | $1.85 | Fable 5 | [-] 12 tools 5m | +4 -1
feat/child (#730) ✓ → feat/parent (#729) | draft | ✓ CI passing
```

On narrow terminals the line wraps at segment boundaries into multiple status
rows, sized to the `COLUMNS` env var Claude Code provides (v2.1.153+).

Segments (each drops out silently when its data is unavailable):

- **Session / Weekly** — rate-limit usage bars with time until reset, from the
  `rate_limits` field of the statusline payload. Handles both the documented
  (`used_percentage`, epoch) and observed (`utilization`, ISO 8601) shapes.
- **Context** — tokens used vs the *effective* context limit:
  `min(model window, autoCompactWindow)`. If you cap compaction at 600k in
  settings, the bar and limit reflect 600k, not the model's 1M window.
  `autoCompactWindow` is resolved like Claude Code resolves settings:
  `.claude/settings.local.json` → `.claude/settings.json` → `~/.claude/settings.json`.
  A red `⚠` appears at ≥85% of the effective limit — auto-compact imminent.
- **Cache** — `cache 18k hit / 2k miss`: tokens of the last API call's input
  served from prompt cache vs paid for in full (misses include cache writes).
  The hit count is green at ≥75% of the total, yellow ≥40%, red below. Many
  misses mean you're paying full price for context that should be cached.
- **Cost** — `$` total for the session.
- **Model** — display name, `Claude ` prefix stripped, with the effort level
  (`⚡xhigh` highlighted, others dim) when the model supports it.
- **API share** — `api 38%`: API time as a share of session wall-clock, i.e.
  whether a slow session is the model or the tools/you.
- **Tools** — `[|] 12 tools 5m`: tool-use count and elapsed time for *this
  session only*. Fed by a PostToolUse hook; state lives in one file per
  `session_id` under `~/.cache/statusline/sessions/` (pruned after 7 days).
- **Lines** — `+added -removed` for the session, hidden when zero.
- **Branch** — resolved fresh every render from the session cwd's repository
  (read off `.git/HEAD`, worktrees included, without starting `git`), so it
  can't go stale or leak between concurrent sessions. Shows `@<sha>` when
  detached (asking `git` for the short sha); prefers the worktree branch for
  `--worktree` sessions. Your own `$USER/` prefix is dropped and, past 40
  characters, the end. A Linear ticket opening the name is uppercased
  (`jakub/dn-479-ontology-m4` is `DN-479-ontology-m4` to anyone but jakub)
  and, with `--linear-workspace <slug>`, linked to the ticket.
- **PR** — a branch with an open PR moves to a row of its own: the branch,
  `(#730)` linked to the PR, the review state (`✓` approved, `✗` changes
  requested), `draft`/`ready`, and CI (linked to the PR's checks): passing,
  running, or failed with up to three failed jobs on rows below, each linked
  to its job. When the PR targets something other than `main`/`master`, `→`
  after the PR names that branch and *its own* open PR (the parent in a
  stack).
  Data from `gh`, cached under `~/.cache/statusline/gh/` and refreshed in the
  background, so it appears one refresh late. Links are OSC 8: Cmd+click in
  terminals that support them. Claude Code only passes them on in terminals
  it recognizes; elsewhere (Zed's terminal, for one) set `FORCE_HYPERLINK=1`.

  ```
  feat/child (#730) ✓ → feat/parent (#729) | ✗ CI failed: 1
    ✗ lint
  ```

## Why not claude-pulse?

This replaces [claude-pulse](https://github.com/NoobyGains/claude-pulse), which
kept all hook state in a single global file (`~/.cache/claude-status/`): tool
counts accumulated forever across every project, branch names were cached
across sessions, and the context limit ignored compaction settings.

## Why Rust, not the original Bun/TypeScript version?

The first implementation of this statusline was TypeScript, compiled to a
standalone binary with `bun build --compile`. That worked, but the binary was
~60MB and needed a workaround for a Bun 1.3.12 macOS codesign regression
(corrupt Mach-O signature, SIGKILL on launch). The Rust rewrite has full
feature parity, no runtime baggage, and a normally-signed ~400KB binary.

## Setup

Build and install the binary, then point `~/.claude/settings.json` at it:

```sh
cargo install --path crates/statusline   # -> ~/.cargo/bin/statusline
```

```json
{
  "statusLine": {
    "type": "command",
    "command": "/Users/<you>/.cargo/bin/statusline",
    "refreshInterval": 30
  },
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "",
        "hooks": [
          { "type": "command", "command": "/Users/<you>/.cargo/bin/statusline --hook" }
        ]
      }
    ]
  }
}
```

Both commands need an absolute path (`~` is not expanded).
To drop segments, pass their names to `--hide`, e.g.
`statusline --hide cache,api,tools,lines`. Names: `session`, `weekly`,
`context`, `cache`, `cost`, `model`, `tools`, `api`, `lines`, `branch`.
`--linear-workspace datanaut` links ticket labels to
`linear.app/datanaut/issue/<id>`.
`refreshInterval` re-renders every 30s so reset countdowns stay current while
idle. The hook is optional — without it the tools segment simply doesn't
render. After changing the source, re-run `cargo install --path crates/statusline`
to update the installed binary.

## Tray (macOS)

`statusline-tray` is a second binary in this workspace: a macOS menu-bar app.
Its menu bar icon is the statusline glyph with a badge for the open sessions (a
cup while it is keeping the Mac awake, a bolt while one is working, a moon when
all are idle), dimmed when none is open; the glyph's own meter fills with the
5-hour window's usage, green while it is on pace to end within the limit and
yellow once it is heading past it. The menu opens on a meter for each
rate-limit window, side by side: how much is used, a fainter fill to where it
is heading by the reset (`16% → 82%`), a tick where an even pace would be now,
how far ahead of or behind that pace it is, when it resets, and for the 5-hour
window what it has cost so far. The meters are bars or rings, whichever
**Meters** is set to in Settings (the gear in the footer opens a small Settings
window). On the spend line, an **Auto caffeinate** switch (on by default, remembered across
launches) keeps the Mac from idle-sleeping while any session is working; sessions in
`/rc` keep it awake either way. The **Stats** button (the chart beside Quit) opens a native window with
the rest: today / this week / this month
(and where the month is heading) / all time, daily cost over 7, 30 or 90 days
stacked by model, cost by model, tokens by kind, session averages with the
latest and the most expensive sessions, and every model with its tier.
`statusline-tray --stats-preview <snapshot.json> [<out.png> [light|dark]]`
opens just that window over a `--dump-snapshot` file, or renders it to a PNG;
`--icon-preview <prefix>` draws the menu bar icon's states, the meters in both
styles, the Regular menu's rows and the Compact popover with sample sessions.
It reads the same `~/.claude/projects/**/*.jsonl`
transcripts `ccusage` does and prices them from embedded pricing catalogs, so it
needs no network and no API key.

```sh
make install     # build, bundle as ~/Applications/Statusline Tray.app, sign, and start it as a login item
make restart     # restart the installed tray
make logs        # follow its log (~/Library/Logs/statusline-tray.log)
make uninstall   # stop it and remove the app and the login item
make run         # build and run in the foreground with tracing, without installing
```

Install it with `make install`: macOS only delivers notifications to a signed
app bundle it knows about. `cargo install --path crates/sl-tray` also works and
runs the same tray (`statusline-tray`, an accessory app with no Dock icon), but
it sends no notifications.

The open sessions sit under the usage limits: the running Claude Code sessions,
read from the record Claude Code keeps for each in `~/.claude/sessions/`
(checked against `claude agents --json` every few minutes, which stands in
whenever the two disagree). **Layout** in Settings chooses how (Settings also
holds Meters, Auto caffeinate and which notifications to send, and remembers
them all across launches):

- **Regular** (the default) is a menu, the sessions grouped by what needs you:
  **Needs you** (idle with its prompt cache running out within 15 minutes, its
  CI failed, or changes requested), **Working** and **Idle**, then a **Cold**
  row for those whose cache has expired, which folds them away until clicked.
  The order holds still while the menu is open; a menu taller than the screen
  scrolls, as any macOS menu does. Each session shows its state (working,
  idle, cold, failed CI) as a small glyph inside a ring of how full its
  context is -- the status line writes the same numbers as its own Context bar
  to `~/.cache/statusline/context/<session>.json` each time it renders, and the
  ring turns orange from 70% and red from 85% -- then its name and PR
  number, and under them project, branch, `draft`, review outcome, CI and how
  long it has been idle; on the right, how long its prompt cache has left
  (`7m left`, read from the transcript: the cache lifetime, 5m or 1h, is
  whichever the last cache write used). A session compacted since its last
  turn shows `compacted` there instead: its next turn builds a fresh cache, so
  there is nothing to lose and it is never counted as Cold.
  Sessions that have stopped running are remembered for two weeks under a
  folded **Closed** row, each with Open worktree in Zed, Copy resume command
  (`claude --resume <id>`, or `codex resume <id>`) and Dismiss; one whose
  worktree or transcript is gone is dropped. The menu lists the newest eight;
  **All closed sessions…** opens a window with every one (up to 200), filtered
  as you type by name, project, branch, PR, session id, or `claude` / `codex`.
  Under them, **Stacks**: each open stack of PRs a running or closed session
  is on (`Stack #746 · 6/10 merged`, and the PR the work is at). Its submenu
  lists the PRs top first, as GitHub does -- merged, open or draft, and which
  session is on each -- each opening its PR, and Open stack on GitHub. GitHub's
  own stacks come from its `repos/{owner}/{repo}/stacks` endpoint (via `gh`,
  on the PR & CI refresh interval); PRs chained by hand, each based on the one
  below, are pieced together from their base PRs.
- **Compact** is a popover of one-line cards, grouped by project (the git repo,
  so all of its worktrees together), with an All / Needs you / Working / Cold
  filter; the list scrolls once it outgrows the screen. Click a card to unfold it in place: branch, PR state, CI, review
  scores and base branch, the failed CI jobs with their re-run, logs and link
  buttons, and buttons for what the Regular submenu offers. The footer holds
  the Auto caffeinate toggle and the Settings gear.

PR and CI data come from the same per-branch `gh` cache the status line uses.

A branch named for a Linear ticket (`jakub/dn-479-…` is `DN-479`) shows the
ticket and its state (`DN-479 · In Progress`) in the session's submenu and on its
Compact card, with an Open in Linear action. It is looked up with the `linear`
command line tool, which must be installed and signed in (`linear auth login`);
without it nothing is shown. Lookups follow the PR & CI refresh interval, and
Settings can switch them off.

In Regular, hover a session for its submenu: the PR (draft or ready, base branch), each
reviewer and their verdict, copy the unresolved review feedback (its reviews,
unresolved threads and conversation as markdown, fetched with `gh`), open the
PR, copy the directory's full path, open the worktree in Zed (the directory is
already the branch's worktree), open the review feedback in Zed (written to a
file under `/tmp/statusline`), open the Linear ticket the branch is named for, and the failed CI jobs, each with its job id, a re-run
button (with confirmation) and the failed-step logs.

Notifications, each its own switch in Settings → Notifications: an idle
session's prompt cache about to expire (once per message), a reviewer
requesting changes on, approving or commenting on a session's PR, a CI job
starting to fail on one, and, off until chosen, its failing CI turning green,
a PR in a tracked stack being merged and a stack PR needing a rebase. They only
appear if Statusline Tray is allowed to notify in System Settings →
Notifications. `SL_TRAY_NO_NOTIFY=1` turns them all off.

Settings has three tabs, and every control acts at once. **General**: layout,
meters, the spend line and Linear ticket, which window the menu bar icon's
meter shows (Auto, Session or Weekly) and whether its sessions badge is drawn,
how often the usage numbers and PR & CI are refreshed, and Keep awake. **Sessions**: the order within a group
(Project, Recent or Newest), what to do with Cold sessions (Fold, Show or Hide;
in the popover Fold and Hide leave them out of All and the Cold filter lists
them), how many open sessions are listed (the rest are counted as "More
sessions"), the popover's card density, how much cache left makes a session
need you (5, 15 or 30 minutes), how long before a one-hour cache expires to warn
(1 to 15 minutes), and where a context ring turns orange and red (the status
line's own `⚠` stays at 85 %). **Notifications**: the switches above. Every
default is what the tray did before the setting existed.

macOS only — it links AppKit, so it neither builds nor runs anywhere else.

Background threads refresh when their files change: the usage numbers when a
transcript does (no more often than every 15 seconds while a session is
writing, or 30 or 45 as set in Settings, and at least once a minute), the open
sessions when Claude
Code rewrites a session's record or a Codex rollout changes (at most every 3
seconds, and at least every 15 while one is open). A PR's CI and reviews are
the one thing that has to be asked for: every 60 seconds by default, set in
Settings. The menu itself repaints once a second and again the moment it is
opened.

### Rate limits

Claude Code hands its rate-limit windows to the status line and to nothing else,
so the status line writes them to `$XDG_CACHE_HOME/statusline/limits.json` on
every turn and the tray reads them back. There is no Keychain access and no
network call: a window Claude Code has not mentioned is one the tray does not
know about either. The capture therefore only refreshes while Claude Code is
running, and the tray greys the title once it is more than two minutes old
rather than presenting a frozen percentage as live. Before the first capture the
title falls back to today's spend.

Debug flags:

```sh
statusline-tray --dump-snapshot   # print the whole computed snapshot as JSON and exit
statusline-tray --rebuild         # discard the usage store, re-read every transcript, then run
SL_TRAY_DEBUG=1 statusline-tray   # trace timer ticks and menu open/close to stderr
statusline-tray --test-notification   # send one notification and say whether it was delivered
SL_TRAY_NO_CAFFEINATE=1 statusline-tray   # never hold the Mac awake (hides Auto caffeinate)
```

For working on the tray without anything real behind it, `SL_TRAY_FIXTURE=full`
(or `stale`, `nolimits`, `empty`) runs it on made-up sessions and usage with no
background threads. `--dump-views <fixture>` prints both layouts as text, and
`--self-test` clicks every control in the menu, the popover and the windows and
prints what each did, recording effects (opening a URL, copying, `gh` calls)
instead of performing them. Both need `SL_TRAY_DEFAULTS_SUITE` set to a scratch
defaults suite, so they never touch the tray's real settings;
`crates/sl-tray/tests/self_test.rs` runs them in a sandbox.

`--dump-snapshot` is also the ground-truth check: its `totalCost` should match
`ccusage claude daily --json --mode calculate` to the cent.

### Usage store

Every parsed usage line lives in one SQLite database,
`$XDG_CACHE_HOME/statusline/usage.sqlite` (default
`~/.cache/statusline/usage.sqlite`), about 180MB for the author's corpus of
~8GB of transcripts. The menu is drawn from rollups per session, day and
model, rebuilt from the rows whenever they change. (The same directory also
holds the status line's per-session counters in `sessions/` and its rate-limit
capture in `limits.json`.)

A refresh reads the bytes that were appended since the last one; a quiet
refresh opens no transaction at all, and a new subagent file is read by
itself. The tray listens for file-system events (FSEvents) under the
transcript directories and `stat()`s and re-lists only what an event named, so
a quiet refresh touches no directory and no transcript. Events are only a
hint: the first refresh, an overflow or error from the watcher, a directory
that did not exist yet, and every five minutes after the last one are a full
`stat()` sweep (one per transcript and per directory, about 10 ms for 2,600
transcripts), and anything modified in the last two seconds is looked at
again regardless. Without the tray's watcher (the `statusline` binary, or
when none can be created) every refresh is that sweep. The first run reads
every transcript once (about 5 seconds for 900k usage lines); after that a new
process opens the database and reads only the rollups. A price-list change recomputes stored
costs without reading a transcript. `--rebuild` discards the store and reads
everything again; a database that cannot be opened, or that an older build
wrote, is replaced the same way. One that a newer build wrote is left alone,
and the tray runs on an in-memory store meanwhile.

`SL_TRAY_DEBUG=1` traces each refresh: what the store read and wrote, and the
process's own disk counters (the numbers Activity Monitor shows) since the
last refresh.

### Codex

When OpenAI's Codex CLI is installed (`$CODEX_HOME`, else `~/.codex`), the tray
shows it beside Claude Code:

- Open, closed and stacked sessions of both are listed together. Each row and
  card carries a small Claude or OpenAI mark beside its right-hand text.
- The footer's switch, left of the cup, picks whose limits the meters and the
  menu bar glyph show: **Claude Code** or **Codex**. It shows the current one's
  logo.
- The Stats window's toolbar picks whose spend it charts: **Claude Code**,
  **Codex** or **All**.

Without a Codex home there is nothing to switch, and the tray is exactly as
before.

Everything is read from `~/.codex` without changing it:

- **Spend.** Each `token_usage_record` in the rollouts
  (`sessions/` and `archived_sessions/`, plain or `.jsonl.zst`) is one model
  response. These records are kept in the same `usage.sqlite`, each file marked
  as Codex's. A response's model is its
  turn's `turn_context` (written before or after it), and a `priority` service
  tier bills at the model's fast multiplier. A changed rollout is re-read whole,
  because a model named later can change an earlier record's price. Runs started
  by Claude Code's Codex plugin count too, when they wrote a rollout.
- **Limits.** Codex reports these only in its transcripts (`token_count` events),
  so the meters show the newest reading seen. They dim once it is over two
  minutes old. A window whose reset has passed shows as unknown, not as
  its old percentage. There is no `codex app-server` call, no auth and no network.
- **Open sessions.** A thread being written holds
  `thread-writer-locks/<thread>.lock`, and one batched `lsof` finds the holders.
  Since Codex 0.164 the holder of a TUI's thread is a shared app-server daemon,
  which keeps a thread loaded for a minute after its TUI quits. So a
  daemon-held thread counts only while an interactive `codex` runs in its
  directory. Working or idle comes from the rollout's latest
  `task_started` / `task_complete`, and the name from `session_index.jsonl`.
  Plugin runs and subagents are not listed. A closed Codex session's
  **Copy resume command** gives `codex resume <id>`.

```sh
statusline-tray --dump-snapshot codex   # or all; claude is the default
statusline-tray --dump-codex-sessions   # the open Codex threads as detected, and exit
```

## Development

```sh
make test               # cargo test --workspace: unit tests, plus the integration tests in crates/*/tests (the tray's self-test among them)
make lint               # clippy over every target, warnings as errors (the lints are in the root Cargo.toml)
cargo build --release   # -> target/release/statusline, target/release/statusline-tray
echo '<payload json>' | cargo run -p statusline --quiet   # render once
```

All `unsafe` lives in `sl-macos`; every other crate is `#![forbid(unsafe_code)]`.
Each unsafe block there does one thing and says why it is sound in a
`// SAFETY:` comment, which the lints enforce. The tray's custom views are
behaviours plugged into `sl-macos`'s two canvas classes, and its clicks and
delegates are Rust closures that `sl-macos` keeps alive on the objects they
serve.

The workspace holds five crates: `statusline` (the status line binary),
`sl-tray` (the menu-bar binary), `sl-core` (formatting/ANSI/payload types
shared by both), `sl-usage` (transcript discovery, parsing, dedupe, pricing
and the incremental cache) and `sl-macos` (safe wrappers over the AppKit,
Foundation and libc calls the tray makes).
