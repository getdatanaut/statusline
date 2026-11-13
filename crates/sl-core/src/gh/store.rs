//! What GitHub said about each branch, kept in `gh.sqlite` under the cache
//! directory: one row per repository (`owner/name`) and branch, shared by all
//! of its worktrees and by the status line and the tray.
//!
//! The store, not its callers, enforces that a known PR is never lost: an
//! answer of "no PR" for a branch whose PR is known only marks it checked,
//! and a PR replaces a known one only when it is newer (a branch name reused
//! after its PR merged).
//!
//! It also holds the claims: while a process is asking GitHub about a
//! repository, others leave it alone instead of asking the same thing. A
//! claim is known by its `until_ms`, which only grows (a claim is taken only
//! once the one before ran out), so a process gives back or holds only its
//! own, never one another process took after its own ran out.
//!
//! And the branches the status line shows ([`Store::saw`]), so the tray asks
//! about them too, not only about its own sessions' branches.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{GhInfo, SCHEMA};

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS pr (
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    -- NULL: GitHub said this branch has no PR.
    number INTEGER,
    -- The GhInfo, as JSON.
    info TEXT NOT NULL,
    -- origin/<branch> here when last asked: a new one means a push.
    remote_sha TEXT,
    checked_ms INTEGER NOT NULL,
    -- When to ask again.
    next_ms INTEGER NOT NULL,
    PRIMARY KEY (repo, branch)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS claim (
    repo TEXT PRIMARY KEY,
    until_ms INTEGER NOT NULL,
    -- Why the last ask about this repository failed, and when; cleared by
    -- the next one that does not.
    last_error TEXT,
    error_ms INTEGER
) WITHOUT ROWID;
-- One row: when the tray, holding a token, last said it is asking GitHub.
CREATE TABLE IF NOT EXISTS tray (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    seen_ms INTEGER NOT NULL
);
";

/// Changes to [`TABLES`] since it was first shipped, in order; a database has
/// applied as many as its `user_version` says.
const MIGRATIONS: [&str; 2] = [
    // When `origin/<branch>` was last seen to move.
    "ALTER TABLE pr ADD COLUMN pushed_ms INTEGER;",
    // The branches the status line shows, and a directory they are checked
    // out in, for the tray to ask about.
    "CREATE TABLE IF NOT EXISTS seen (
        repo TEXT NOT NULL,
        branch TEXT NOT NULL,
        dir TEXT NOT NULL,
        seen_ms INTEGER NOT NULL,
        PRIMARY KEY (repo, branch)
    ) WITHOUT ROWID;",
];

/// How long the status line waits for the database: a render must not hang
/// on a tray in the middle of writing.
const RENDER_BUSY: Duration = Duration::from_millis(200);
/// How long anyone else waits for it.
const BUSY: Duration = Duration::from_secs(2);

/// One branch's stored answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// `pr_number` is `None` for a branch with no PR. `fetched_ms` is when
    /// GitHub last answered about it, `fresh_until_ms` when it is due again.
    pub info: GhInfo,
    pub remote_sha: Option<String>,
    /// When `origin/<branch>` was last seen to move, if it was.
    pub pushed_ms: Option<i64>,
}

/// When and how a branch was asked about, beside what GitHub said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asked<'a> {
    /// Where `origin/<branch>` is here; `None` keeps the one stored.
    pub remote_sha: Option<&'a str>,
    /// Set when this ask found a push; `None` keeps the time stored.
    pub pushed_ms: Option<i64>,
    pub now_ms: i64,
    /// When to ask again.
    pub next_ms: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn default_path() -> PathBuf {
        crate::paths::cache_dir().join("gh.sqlite")
    }

    /// The store at [`default_path`](Self::default_path); `None` if it cannot
    /// be opened, as when the disk is full: then nothing about PRs is shown.
    pub fn open_default() -> Option<Self> {
        Self::open(&Self::default_path()).ok()
    }

    /// [`open_default`](Self::open_default) for a render, which waits for
    /// the database only briefly.
    pub fn open_for_render() -> Option<Self> {
        Self::open_waiting(&Self::default_path(), RENDER_BUSY).ok()
    }

    /// # Errors
    ///
    /// When the file cannot be opened or set up.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::open_waiting(path, BUSY)
    }

    /// The store at `path`, waiting up to `busy` for another process's write.
    /// A database already set up is only opened: the set-up runs once.
    fn open_waiting(path: &Path, busy: Duration) -> rusqlite::Result<Self> {
        let conn = if let Ok(conn) = Connection::open(path) {
            conn
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            Connection::open(path)?
        };
        conn.busy_timeout(busy)?;
        if user_version(&conn)? < MIGRATIONS.len() {
            set_up(&conn)?;
        }
        Ok(Self { conn })
    }

    /// What is stored for `branch` of `repo`, if anything.
    pub fn get(&self, repo: &str, branch: &str) -> Option<Row> {
        self.conn
            .query_row(
                "SELECT info, remote_sha, checked_ms, next_ms, pushed_ms FROM pr
                 WHERE repo = ?1 AND branch = ?2",
                params![repo, branch],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|(info, remote_sha, checked_ms, next_ms, pushed_ms)| {
                let mut info: GhInfo = serde_json::from_str(&info).ok()?;
                info.fetched_ms = checked_ms;
                info.fresh_until_ms = next_ms;
                Some(Row {
                    info,
                    remote_sha,
                    pushed_ms,
                })
            })
    }

    /// Store GitHub's answer for `branch`: its PR, or `None` for no PR.
    /// A known PR is kept against "no PR" and against an older PR; then only
    /// what `asked` says is updated.
    ///
    /// # Errors
    ///
    /// When the database cannot be written.
    pub fn record(
        &self,
        repo: &str,
        branch: &str,
        found: Option<&GhInfo>,
        asked: Asked<'_>,
    ) -> rusqlite::Result<()> {
        let Asked {
            remote_sha,
            pushed_ms,
            now_ms,
            next_ms,
        } = asked;
        // Immediate: a read that later writes would otherwise fail outright
        // (not wait) when another process wrote in between.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let known: Option<i64> = tx
            .query_row(
                "SELECT number FROM pr WHERE repo = ?1 AND branch = ?2",
                params![repo, branch],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        // A known PR stays against "no PR" and against an older one.
        let replaces = match (found.and_then(|info| info.pr_number), known) {
            (Some(new), Some(old)) => new >= old,
            (new, old) => new.is_some() || old.is_none(),
        };
        if replaces {
            let miss = GhInfo {
                schema: SCHEMA,
                ..GhInfo::default()
            };
            let info = found.unwrap_or(&miss);
            let json = serde_json::to_string(info)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            tx.execute(
                "INSERT INTO pr (repo, branch, number, info, remote_sha, checked_ms, next_ms, pushed_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (repo, branch) DO UPDATE SET number = excluded.number,
                     info = excluded.info,
                     remote_sha = COALESCE(excluded.remote_sha, pr.remote_sha),
                     checked_ms = excluded.checked_ms, next_ms = excluded.next_ms,
                     pushed_ms = COALESCE(excluded.pushed_ms, pr.pushed_ms)",
                params![
                    repo,
                    branch,
                    info.pr_number,
                    json,
                    remote_sha,
                    now_ms,
                    next_ms,
                    pushed_ms
                ],
            )?;
        } else {
            tx.execute(
                "UPDATE pr SET remote_sha = COALESCE(?3, remote_sha), checked_ms = ?4,
                     next_ms = ?5, pushed_ms = COALESCE(?6, pushed_ms)
                 WHERE repo = ?1 AND branch = ?2",
                params![repo, branch, remote_sha, now_ms, next_ms, pushed_ms],
            )?;
        }
        tx.commit()
    }

    /// Claim the right to ask GitHub about `repo` until `until_ms`: `None`
    /// while another claim on it has not run out, else the claim, to give
    /// back ([`release`](Self::release)) or [`hold`](Self::hold).
    pub fn claim(&self, repo: &str, now_ms: i64, until_ms: i64) -> Option<Claim> {
        self.conn
            .execute(
                "INSERT INTO claim (repo, until_ms) VALUES (?1, ?2)
                 ON CONFLICT (repo) DO UPDATE SET until_ms = excluded.until_ms
                 WHERE claim.until_ms <= ?3",
                params![repo, until_ms, now_ms],
            )
            .is_ok_and(|changed| changed == 1)
            .then_some(Claim(until_ms))
    }

    /// Hold `claim` on `repo` until `until_ms`, noting `error`: after a failed
    /// ask, so no one asks again before then, and so
    /// `sqlite3 gh.sqlite 'SELECT * FROM claim'` says why. Nothing is held if
    /// the claim ran out and another process took `repo` since.
    pub fn hold(&self, repo: &str, claim: Claim, now_ms: i64, until_ms: i64, error: &str) {
        self.conn
            .execute(
                "INSERT INTO claim (repo, until_ms, last_error, error_ms) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (repo) DO UPDATE SET until_ms = excluded.until_ms,
                     last_error = excluded.last_error, error_ms = excluded.error_ms
                 WHERE claim.until_ms = ?5 OR claim.until_ms <= ?4",
                params![repo, until_ms, error, now_ms, claim.0],
            )
            .ok();
    }

    /// Why the last ask about `repo` failed, if the one after it has not
    /// succeeded yet.
    pub fn last_error(&self, repo: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT last_error FROM claim WHERE repo = ?1",
                params![repo],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
            .flatten()
    }

    /// Give `claim` on `repo` back, unless it ran out and another process
    /// took `repo` since.
    pub fn release(&self, repo: &str, claim: Claim) {
        self.conn
            .execute(
                "DELETE FROM claim WHERE repo = ?1 AND until_ms = ?2",
                params![repo, claim.0],
            )
            .ok();
    }

    /// The status line shows `branch` of `repo`, checked out in `dir`, as of
    /// `now_ms`. Written at most once per [`SEEN_EVERY_MS`] for a branch, so
    /// a render seldom writes.
    pub fn saw(&self, repo: &str, branch: &str, dir: &str, now_ms: i64) {
        let recent = self
            .conn
            .query_row(
                "SELECT seen_ms FROM seen WHERE repo = ?1 AND branch = ?2 AND dir = ?3",
                params![repo, branch, dir],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten()
            .is_some_and(|seen| now_ms - seen < SEEN_EVERY_MS);
        if !recent {
            self.conn
                .execute(
                    "INSERT INTO seen (repo, branch, dir, seen_ms) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (repo, branch) DO UPDATE SET dir = excluded.dir,
                         seen_ms = excluded.seen_ms",
                    params![repo, branch, dir, now_ms],
                )
                .ok();
        }
    }

    /// Each branch the status line showed since `since_ms`, as (directory,
    /// branch).
    pub fn seen_since(&self, since_ms: i64) -> Vec<(String, String)> {
        let Ok(mut statement) = self
            .conn
            .prepare("SELECT dir, branch FROM seen WHERE seen_ms >= ?1 ORDER BY dir, branch")
        else {
            return Vec::new();
        };
        statement
            .query_map(params![since_ms], |row| Ok((row.get(0)?, row.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Forget branches not asked about, and not shown, since `before_ms`:
    /// branches long merged or deleted, which would otherwise pile up.
    pub fn prune(&self, before_ms: i64) {
        self.conn
            .execute_batch(&format!(
                "DELETE FROM pr WHERE checked_ms < {before_ms};
                 DELETE FROM seen WHERE seen_ms < {before_ms};"
            ))
            .ok();
    }

    /// The tray, holding a token, is asking GitHub as of `now_ms`.
    pub fn mark_tray(&self, now_ms: i64) {
        self.conn
            .execute(
                "INSERT INTO tray (id, seen_ms) VALUES (1, ?1)
                 ON CONFLICT (id) DO UPDATE SET seen_ms = excluded.seen_ms",
                params![now_ms],
            )
            .ok();
    }

    /// When the tray last said it is asking GitHub; `None` if it never has.
    pub fn tray_seen_ms(&self) -> Option<i64> {
        self.conn
            .query_row("SELECT seen_ms FROM tray WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    /// The tray stopped asking (its token was removed).
    pub fn clear_tray(&self) {
        self.conn.execute("DELETE FROM tray", []).ok();
    }

    /// Lift every hold a failure left: after a new token is stored, so the
    /// repositories it failed for are asked again at once.
    pub fn release_failed(&self) {
        self.conn
            .execute("DELETE FROM claim WHERE last_error IS NOT NULL", [])
            .ok();
    }
}

/// A claim on a repository, known by when it runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claim(pub i64);

/// How often a branch the status line keeps showing is noted again.
pub const SEEN_EVERY_MS: i64 = 60_000;

fn user_version(conn: &Connection) -> rusqlite::Result<usize> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(usize::try_from(version).unwrap_or(0))
}

/// Create the tables and apply the [`MIGRATIONS`] not yet applied, all under
/// the write lock, so two processes opening a new database at once do not
/// both migrate it.
fn set_up(conn: &Connection) -> rusqlite::Result<()> {
    // `journal_mode` answers with a row, so it is queried, not executed; and
    // it cannot change inside a transaction.
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(TABLES)?;
    let applied = user_version(&tx)?;
    for (version, migration) in MIGRATIONS.iter().enumerate().skip(applied) {
        tx.execute_batch(migration)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", version + 1))?;
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: i64, state: &str) -> GhInfo {
        GhInfo {
            pr_number: Some(number),
            state: Some(state.into()),
            schema: SCHEMA,
            ..GhInfo::default()
        }
    }

    fn at(remote_sha: Option<&str>, now_ms: i64, next_ms: i64) -> Asked<'_> {
        Asked {
            remote_sha,
            pushed_ms: None,
            now_ms,
            next_ms,
        }
    }

    #[test]
    fn a_database_from_before_pushes_were_kept_gains_the_column_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gh.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(TABLES).unwrap();
            conn.execute(
                "INSERT INTO pr (repo, branch, number, info, checked_ms, next_ms)
                 VALUES ('o/r', 'b', 7, '{\"pr_number\":7,\"fetched_ms\":0,\"schema\":5,\"ci\":null,\"base\":null}', 1, 2)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let row = store.get("o/r", "b").unwrap();
        assert_eq!((row.info.pr_number, row.pushed_ms), (Some(7), None));
        store
            .record(
                "o/r",
                "b",
                None,
                Asked {
                    pushed_ms: Some(5),
                    ..at(None, 3, 4)
                },
            )
            .unwrap();
        assert_eq!(store.get("o/r", "b").unwrap().pushed_ms, Some(5));
        drop(store);
        Store::open(&path).expect("opened again: the migration is not run twice");
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("gh.sqlite")).unwrap();
        (dir, store)
    }

    #[test]
    fn a_known_pr_survives_a_later_no_pr_and_only_its_times_move() {
        let (_dir, store) = store();
        store
            .record("o/r", "b", Some(&pr(7, "OPEN")), at(Some("aaa"), 1, 10))
            .unwrap();
        store
            .record("o/r", "b", None, at(Some("bbb"), 2, 20))
            .unwrap();
        store.record("o/r", "b", None, at(None, 2, 20)).unwrap();
        let row = store.get("o/r", "b").unwrap();
        assert_eq!(row.info.pr_number, Some(7), "no answer erases a known PR");
        assert_eq!((row.info.fetched_ms, row.info.fresh_until_ms), (2, 20));
        assert_eq!(row.remote_sha.as_deref(), Some("bbb"));
    }

    #[test]
    fn a_newer_pr_on_a_reused_branch_replaces_the_old_one_but_an_older_does_not() {
        let (_dir, store) = store();
        store
            .record("o/r", "b", Some(&pr(7, "MERGED")), at(None, 1, 10))
            .unwrap();
        store
            .record("o/r", "b", Some(&pr(9, "OPEN")), at(None, 2, 20))
            .unwrap();
        assert_eq!(store.get("o/r", "b").unwrap().info.pr_number, Some(9));
        store
            .record("o/r", "b", Some(&pr(7, "MERGED")), at(None, 3, 30))
            .unwrap();
        assert_eq!(store.get("o/r", "b").unwrap().info.pr_number, Some(9));
        let mut updated = pr(9, "MERGED");
        updated.is_draft = Some(false);
        store
            .record("o/r", "b", Some(&updated), at(None, 4, 40))
            .unwrap();
        assert_eq!(
            store.get("o/r", "b").unwrap().info.state.as_deref(),
            Some("MERGED"),
            "the same PR is updated"
        );
    }

    #[test]
    fn no_pr_is_stored_as_an_answer_and_a_pr_found_later_replaces_it() {
        let (_dir, store) = store();
        assert_eq!(store.get("o/r", "b"), None, "never asked");
        store.record("o/r", "b", None, at(None, 1, 10)).unwrap();
        let miss = store.get("o/r", "b").unwrap();
        assert_eq!((miss.info.pr_number, miss.info.schema), (None, SCHEMA));
        store
            .record("o/r", "b", Some(&pr(3, "OPEN")), at(None, 2, 20))
            .unwrap();
        assert_eq!(store.get("o/r", "b").unwrap().info.pr_number, Some(3));
        assert_eq!(store.get("o/other", "b"), None, "repositories are apart");
    }

    #[test]
    fn a_claim_excludes_others_until_it_runs_out_or_is_given_back() {
        let (dir, store) = store();
        let other = Store::open(&dir.path().join("gh.sqlite")).unwrap();
        let first = store.claim("o/r", 0, 100).unwrap();
        assert_eq!(other.claim("o/r", 50, 150), None, "taken");
        assert!(
            other.claim("o/s", 50, 150).is_some(),
            "another repository is free"
        );
        let second = other.claim("o/r", 100, 200).expect("ran out");
        store.release("o/r", first);
        assert_eq!(
            store.claim("o/r", 150, 250),
            None,
            "a stale release gives back nothing"
        );
        store.hold("o/r", first, 150, 1_000, "late");
        assert_eq!(
            other.last_error("o/r"),
            None,
            "nor does a stale hold take it over"
        );
        other.release("o/r", second);
        let third = store.claim("o/r", 201, 300).unwrap();
        store.hold("o/r", third, 250, 1_000, "Unauthorized");
        assert_eq!(other.claim("o/r", 500, 600), None, "held after a failure");
        assert_eq!(other.last_error("o/r").as_deref(), Some("Unauthorized"));
        let fourth = other.claim("o/r", 1_000, 2_000).expect("until it runs out");
        other.release("o/r", fourth);
        assert_eq!(other.last_error("o/r"), None, "a success clears it");
    }

    #[test]
    fn two_processes_opening_a_new_database_at_once_both_get_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gh.sqlite");
        let opened: Vec<bool> = std::thread::scope(|scope| {
            let running: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| Store::open(&path).is_ok()))
                .collect();
            running.into_iter().map(|t| t.join().unwrap()).collect()
        });
        assert!(opened.iter().all(|ok| *ok), "{opened:?}");
        let store = Store::open(&path).unwrap();
        assert_eq!(user_version(&store.conn).unwrap(), MIGRATIONS.len());
    }

    #[test]
    fn a_shown_branch_is_noted_once_a_minute_and_both_kinds_of_row_are_pruned() {
        let (_dir, store) = store();
        store.saw("o/r", "b", "/w", 1_000);
        store.saw("o/r", "b", "/w", 2_000);
        let seen_ms = |store: &Store| -> i64 {
            store
                .conn
                .query_row("SELECT seen_ms FROM seen", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(seen_ms(&store), 1_000, "not written again within a minute");
        store.saw("o/r", "b", "/other", 2_000);
        assert_eq!(seen_ms(&store), 2_000, "another directory is noted at once");
        assert_eq!(
            store.seen_since(1_500),
            vec![("/other".to_owned(), "b".to_owned())]
        );
        store
            .record("o/r", "b", None, at(None, 3_000, 4_000))
            .unwrap();
        store.prune(2_500);
        assert_eq!(store.seen_since(0), Vec::new());
        assert!(store.get("o/r", "b").is_some(), "asked about since");
        store.prune(3_001);
        assert_eq!(store.get("o/r", "b"), None);
    }
}
