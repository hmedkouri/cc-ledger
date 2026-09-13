//! SQLite storage for the token ledger.
//!
//! Several Claude Code sessions render their status line concurrently, so every
//! connection runs in WAL mode with a short busy timeout. Writes are
//! `INSERT OR IGNORE` on a stable dedupe key, which makes a lost pass, a
//! repeated pass and a concurrent pass all harmless.
//!
//! Timestamps are stored as UTC Unix seconds. Bucketing into days, weeks and
//! months happens in local time, in Rust, so DST transitions land correctly —
//! see `bucket` in the `cc-usage` binary and the tests at the bottom of this
//! file.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::payload::RateLimit;
use crate::transcript::{Cursor, UsageRecord};

pub struct Ledger {
    conn: Connection,
}

/// The few numbers the status line needs. Every one is an indexed range scan.
#[derive(Debug, Default, PartialEq)]
pub struct LedgerSummary {
    pub session_tokens: i64,
    pub today_tokens: i64,
    pub week_tokens: i64,
}

/// One ledger row, for `cc-usage` to aggregate.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub ts: i64,
    pub model: String,
    pub project_dir: String,
    pub input: i64,
    pub output: i64,
    pub cache_create: i64,
    pub cache_read: i64,
}

impl Row {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_create + self.cache_read
    }
}

/// One session, as `cc-usage sessions` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub session_id: String,
    pub project_dir: String,
    pub first_seen: i64,
    pub last_seen: i64,
    pub tokens: i64,
}

/// `CC_LEDGER_DB`, else `$XDG_DATA_HOME/cc-ledger/ledger.db`, else
/// `~/.local/share/cc-ledger/ledger.db`.
pub fn default_db_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("CC_LEDGER_DB") {
        return PathBuf::from(explicit);
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".local/share")
        });
    data_home.join("cc-ledger").join("ledger.db")
}

impl Ledger {
    pub fn open_default() -> Result<Self> {
        Self::open(&default_db_path())
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening ledger at {}", path.display()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL lets a reader (rendering) proceed while another session writes.
        // journal_mode is a no-op on :memory:, hence query_row rather than execute.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap_or_default();
        conn.busy_timeout(std::time::Duration::from_millis(250))?;
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;

        let ledger = Ledger { conn };
        ledger.migrate()?;
        Ok(ledger)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS requests (
                dedupe_key      TEXT PRIMARY KEY,
                session_id      TEXT,
                project_dir     TEXT,
                model           TEXT,
                ts              INTEGER NOT NULL,
                input           INTEGER NOT NULL DEFAULT 0,
                output          INTEGER NOT NULL DEFAULT 0,
                cache_create    INTEGER NOT NULL DEFAULT 0,
                cache_read      INTEGER NOT NULL DEFAULT 0,
                -- subsets of output / cache_create, never added to totals
                thinking        INTEGER NOT NULL DEFAULT 0,
                cache_1h        INTEGER NOT NULL DEFAULT 0,
                cache_5m        INTEGER NOT NULL DEFAULT 0,
                source          TEXT NOT NULL DEFAULT 'main',
                transcript_path TEXT
            );
            CREATE INDEX IF NOT EXISTS requests_ts         ON requests(ts);
            CREATE INDEX IF NOT EXISTS requests_model_ts   ON requests(model, ts);
            CREATE INDEX IF NOT EXISTS requests_project_ts ON requests(project_dir, ts);
            CREATE INDEX IF NOT EXISTS requests_session    ON requests(session_id);

            CREATE TABLE IF NOT EXISTS cursors (
                transcript_path TEXT PRIMARY KEY,
                byte_offset     INTEGER NOT NULL,
                file_id         INTEGER NOT NULL,
                last_seen       INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
                session_id  TEXT PRIMARY KEY,
                project_dir TEXT,
                first_seen  INTEGER NOT NULL,
                last_seen   INTEGER NOT NULL
            );

            -- The only historical record of subscription consumption that
            -- exists anywhere: the server keeps none, and the percentages are
            -- gone from the payload the moment the window rolls over.
            -- One row per observed change, not per invocation.
            CREATE TABLE IF NOT EXISTS limits (
                ts                  INTEGER PRIMARY KEY,
                five_hour_pct       REAL,
                five_hour_resets_at INTEGER,
                seven_day_pct       REAL,
                seven_day_resets_at INTEGER
            );
            "#,
        )?;
        Ok(())
    }

    /// Returns how many rows were genuinely new.
    pub fn ingest(&mut self, records: &[UsageRecord]) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO requests
                 (dedupe_key, session_id, project_dir, model, ts, input, output,
                  cache_create, cache_read, thinking, cache_1h, cache_5m, source, transcript_path)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?;
            let mut session = tx.prepare_cached(
                "INSERT INTO sessions (session_id, project_dir, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET
                   last_seen  = max(last_seen,  excluded.last_seen),
                   first_seen = min(first_seen, excluded.first_seen),
                   project_dir = coalesce(sessions.project_dir, excluded.project_dir)",
            )?;

            for r in records {
                inserted += stmt.execute(rusqlite::params![
                    r.dedupe_key,
                    r.session_id,
                    r.project_dir,
                    r.model,
                    r.ts,
                    r.input,
                    r.output,
                    r.cache_create,
                    r.cache_read,
                    r.thinking,
                    r.cache_1h,
                    r.cache_5m,
                    r.source.as_str(),
                    r.transcript_path,
                ])?;
                if let Some(sid) = &r.session_id {
                    session.execute(rusqlite::params![sid, r.project_dir, r.ts])?;
                }
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn cursor(&self, transcript_path: &str) -> Result<Option<Cursor>> {
        let found = self
            .conn
            .query_row(
                "SELECT byte_offset, file_id FROM cursors WHERE transcript_path = ?1",
                [transcript_path],
                |row| {
                    Ok(Cursor {
                        byte_offset: row.get::<_, i64>(0)? as u64,
                        file_id: row.get::<_, i64>(1)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(found)
    }

    pub fn set_cursor(&self, transcript_path: &str, cursor: Cursor, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cursors (transcript_path, byte_offset, file_id, last_seen)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(transcript_path) DO UPDATE SET
               byte_offset = excluded.byte_offset,
               file_id     = excluded.file_id,
               last_seen   = excluded.last_seen",
            rusqlite::params![
                transcript_path,
                cursor.byte_offset as i64,
                cursor.file_id as i64,
                now
            ],
        )?;
        Ok(())
    }

    /// Append a rate-limit observation, but only when it differs from the last
    /// one recorded. The status line fires many times per turn; storing every
    /// invocation would be almost entirely duplicate rows.
    pub fn record_limits(
        &self,
        five_hour: Option<RateLimit>,
        seven_day: Option<RateLimit>,
        now: i64,
    ) -> Result<bool> {
        if five_hour.is_none() && seven_day.is_none() {
            return Ok(false);
        }
        let new = (
            five_hour.and_then(|r| r.used_percentage),
            five_hour.and_then(|r| r.resets_at),
            seven_day.and_then(|r| r.used_percentage),
            seven_day.and_then(|r| r.resets_at),
        );

        let last = self
            .conn
            .query_row(
                "SELECT five_hour_pct, five_hour_resets_at, seven_day_pct, seven_day_resets_at
                 FROM limits ORDER BY ts DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<f64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<f64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;

        if last.as_ref() == Some(&new) {
            return Ok(false);
        }

        self.conn.execute(
            "INSERT OR REPLACE INTO limits
             (ts, five_hour_pct, five_hour_resets_at, seven_day_pct, seven_day_resets_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![now, new.0, new.1, new.2, new.3],
        )?;
        Ok(true)
    }

    /// `day_start` and `week_start` are local-time boundaries as UTC seconds,
    /// computed by the caller so this stays a pure indexed range scan.
    pub fn summary(
        &self,
        session_id: Option<&str>,
        day_start: i64,
        week_start: i64,
    ) -> Result<LedgerSummary> {
        let total = "coalesce(sum(input + output + cache_create + cache_read), 0)";

        let today_tokens: i64 = self.conn.query_row(
            &format!("SELECT {total} FROM requests WHERE ts >= ?1"),
            [day_start],
            |r| r.get(0),
        )?;
        let week_tokens: i64 = self.conn.query_row(
            &format!("SELECT {total} FROM requests WHERE ts >= ?1"),
            [week_start],
            |r| r.get(0),
        )?;
        let session_tokens: i64 = match session_id {
            Some(sid) => self.conn.query_row(
                &format!("SELECT {total} FROM requests WHERE session_id = ?1"),
                [sid],
                |r| r.get(0),
            )?,
            None => 0,
        };

        Ok(LedgerSummary {
            session_tokens,
            today_tokens,
            week_tokens,
        })
    }

    /// Rows in `[since, until)`, ordered by time, for `cc-usage` to bucket.
    pub fn rows(&self, since: Option<i64>, until: Option<i64>) -> Result<Vec<Row>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, coalesce(model,'unknown'), coalesce(project_dir,'unknown'),
                    input, output, cache_create, cache_read
             FROM requests
             WHERE ts >= ?1 AND ts < ?2
             ORDER BY ts",
        )?;
        let rows = stmt
            .query_map(
                [since.unwrap_or(i64::MIN), until.unwrap_or(i64::MAX)],
                |row| {
                    Ok(Row {
                        ts: row.get(0)?,
                        model: row.get(1)?,
                        project_dir: row.get(2)?,
                        input: row.get(3)?,
                        output: row.get(4)?,
                        cache_create: row.get(5)?,
                        cache_read: row.get(6)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn request_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM requests", [], |r| r.get(0))?)
    }

    /// Rows whose dedupe key was seen in more than one transcript. Always empty
    /// on the data this was built against; a non-empty result means a resumed
    /// session copied history, which is worth knowing rather than silently
    /// collapsing.
    pub fn duplicate_sources(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, count(DISTINCT transcript_path) c
             FROM requests WHERE session_id IS NOT NULL
             GROUP BY session_id HAVING c > 1",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn sessions(&self, project: Option<&str>) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.session_id, coalesce(s.project_dir,'unknown'), s.first_seen, s.last_seen,
                    coalesce((SELECT sum(input+output+cache_create+cache_read)
                              FROM requests r WHERE r.session_id = s.session_id), 0)
             FROM sessions s
             WHERE (?1 IS NULL OR s.project_dir = ?1)
             ORDER BY s.last_seen DESC",
        )?;
        let rows = stmt
            .query_map([project], |row| {
                Ok(SessionRow {
                    session_id: row.get(0)?,
                    project_dir: row.get(1)?,
                    first_seen: row.get(2)?,
                    last_seen: row.get(3)?,
                    tokens: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Source;

    fn record(key: &str, ts: i64, output: i64) -> UsageRecord {
        UsageRecord {
            dedupe_key: key.to_string(),
            session_id: Some("s1".into()),
            project_dir: Some("/p".into()),
            model: "claude-opus-5".into(),
            ts,
            input: 1,
            output,
            cache_create: 10,
            cache_read: 100,
            thinking: output / 2,
            cache_1h: 10,
            cache_5m: 0,
            source: Source::Main,
            transcript_path: "/t.jsonl".into(),
        }
    }

    #[test]
    fn same_request_twice_yields_one_row() {
        let mut l = Ledger::open_in_memory().unwrap();
        let r = record("msg_A", 1_000, 50);

        assert_eq!(l.ingest(std::slice::from_ref(&r)).unwrap(), 1);
        assert_eq!(
            l.ingest(std::slice::from_ref(&r)).unwrap(),
            0,
            "second pass inserts none"
        );
        assert_eq!(l.request_count().unwrap(), 1);
    }

    /// The real shape of the bug this guards: one response, three content-block
    /// lines, identical usage. The ledger must bill it once.
    #[test]
    fn repeated_content_block_lines_collapse_to_one_row() {
        let mut l = Ledger::open_in_memory().unwrap();
        let r = record("msg_A", 1_000, 589);
        let batch = vec![r.clone(), r.clone(), r.clone()];

        assert_eq!(l.ingest(&batch).unwrap(), 1);
        let rows = l.rows(None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].output, 589, "not 1767");
    }

    #[test]
    fn subset_columns_are_excluded_from_totals() {
        let mut l = Ledger::open_in_memory().unwrap();
        l.ingest(&[record("msg_A", 1_000, 50)]).unwrap();
        let rows = l.rows(None, None).unwrap();
        // input 1 + output 50 + cache_create 10 + cache_read 100; thinking and
        // the ephemeral split must not appear.
        assert_eq!(rows[0].total(), 161);
    }

    #[test]
    fn cursor_round_trips_and_updates() {
        let l = Ledger::open_in_memory().unwrap();
        assert_eq!(l.cursor("/t.jsonl").unwrap(), None);

        let c = Cursor {
            byte_offset: 4096,
            file_id: 77,
        };
        l.set_cursor("/t.jsonl", c, 1_000).unwrap();
        assert_eq!(l.cursor("/t.jsonl").unwrap(), Some(c));

        let c2 = Cursor {
            byte_offset: 8192,
            file_id: 77,
        };
        l.set_cursor("/t.jsonl", c2, 2_000).unwrap();
        assert_eq!(l.cursor("/t.jsonl").unwrap(), Some(c2));
    }

    #[test]
    fn summary_counts_only_the_requested_windows() {
        let mut l = Ledger::open_in_memory().unwrap();
        l.ingest(&[
            record("old", 1_000, 10),
            record("recent", 50_000, 20),
            record("newest", 90_000, 30),
        ])
        .unwrap();

        // Each record carries input 1 + cache_create 10 + cache_read 100 = 111
        // of fixed weight, plus its own output.
        let s = l.summary(Some("s1"), 60_000, 40_000).unwrap();
        assert_eq!(s.today_tokens, 141); // "newest" only: 111 + 30
        assert_eq!(s.week_tokens, 272); // "recent" (131) + "newest" (141)
        assert_eq!(s.session_tokens, 393); // all three: 121 + 131 + 141
    }

    #[test]
    fn summary_without_session_reports_zero_session_tokens() {
        let mut l = Ledger::open_in_memory().unwrap();
        l.ingest(&[record("a", 1_000, 10)]).unwrap();
        assert_eq!(l.summary(None, 0, 0).unwrap().session_tokens, 0);
    }

    #[test]
    fn limits_are_recorded_only_when_they_change() {
        let l = Ledger::open_in_memory().unwrap();
        let five = |p: f64| {
            Some(RateLimit {
                used_percentage: Some(p),
                resets_at: Some(1_788_616_252),
            })
        };

        assert!(l.record_limits(five(12.5), None, 1_000).unwrap());
        assert!(
            !l.record_limits(five(12.5), None, 1_001).unwrap(),
            "unchanged observation must not add a row"
        );
        assert!(l.record_limits(five(13.0), None, 1_002).unwrap());

        let n: i64 = l
            .conn
            .query_row("SELECT count(*) FROM limits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    /// rate_limits vanished from the payload once already.
    #[test]
    fn absent_limits_are_not_recorded() {
        let l = Ledger::open_in_memory().unwrap();
        assert!(!l.record_limits(None, None, 1_000).unwrap());
        let n: i64 = l
            .conn
            .query_row("SELECT count(*) FROM limits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn sessions_track_first_and_last_seen() {
        let mut l = Ledger::open_in_memory().unwrap();
        l.ingest(&[record("a", 5_000, 10), record("b", 1_000, 10)])
            .unwrap();

        let sessions = l.sessions(None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].first_seen, 1_000, "first_seen is the earliest");
        assert_eq!(sessions[0].last_seen, 5_000, "last_seen is the latest");
    }

    #[test]
    fn duplicate_transcript_sources_are_reportable() {
        let mut l = Ledger::open_in_memory().unwrap();
        let mut a = record("a", 1_000, 10);
        a.transcript_path = "/one.jsonl".into();
        let mut b = record("b", 1_000, 10);
        b.transcript_path = "/two.jsonl".into();

        l.ingest(&[a]).unwrap();
        assert!(l.duplicate_sources().unwrap().is_empty());

        l.ingest(&[b]).unwrap();
        let dupes = l.duplicate_sources().unwrap();
        assert_eq!(
            dupes.len(),
            1,
            "same session across two transcripts shows up"
        );
        assert_eq!(dupes[0].1, 2);
    }

    #[test]
    fn db_path_prefers_explicit_override() {
        // Safe: this process never reads the real env in other tests.
        std::env::set_var("CC_LEDGER_DB", "/tmp/explicit.db");
        assert_eq!(default_db_path(), PathBuf::from("/tmp/explicit.db"));
        std::env::remove_var("CC_LEDGER_DB");
    }
}
