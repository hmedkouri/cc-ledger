//! SQLite storage for the token ledger.
//!
//! Several Claude Code sessions render their status line concurrently, so every
//! connection runs in WAL mode with a short busy timeout. Writes upsert on a
//! stable dedupe key, merging each token field with `max()`, which makes a lost
//! pass, a repeated pass and a concurrent pass all harmless — and, unlike
//! ignoring the conflict, leaves a partially-written first record recoverable,
//! because usage is monotonic within a response.
//!
//! Timestamps are stored as UTC Unix seconds. Bucketing into days, weeks and
//! months happens in local time, in Rust, so DST transitions land correctly —
//! see `bucket` in the `cc-usage` binary and the tests at the bottom of this
//! file.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::payload::RateLimit;
use crate::transcript::{CostState, Cursor, Scan, UsageRecord};

/// How long a writer waits for another session's lock before giving up.
const WRITE_BUSY_TIMEOUT_MS: u64 = 250;

/// How long the pre-render summary read waits. Deliberately far shorter than
/// the ingest budget: that read happens before anything is on screen and is
/// not covered by any deadline, so a contended database must not stall the
/// status line.
const RENDER_BUSY_TIMEOUT_MS: u64 = 50;

pub struct Ledger {
    conn: Connection,
}

/// The few numbers the status line needs. Every one is an indexed range scan.
#[derive(Debug, Default, PartialEq)]
pub struct LedgerSummary {
    pub today_tokens: i64,
    /// Today's tokens priced at API list rates, for `statusline --cost`.
    /// Computed from the same single query, never a second pass over rows.
    pub today_cost: f64,
}

/// One ledger row, for `cc-usage` to aggregate.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub ts: i64,
    pub model: String,
    /// The cwd recorded on *this individual request*. Claude Code updates it
    /// mid-session when the working directory moves, so grouping by it splits
    /// one project across every subdirectory a session happened to visit.
    pub project_dir: String,
    /// The directory the *session* started in — the project Claude Code
    /// considers the work to belong to, and the right key for a per-project
    /// breakdown. Falls back to `project_dir` when the session is unknown.
    pub project_root: String,
    pub input: i64,
    pub output: i64,
    pub cache_create: i64,
    pub cache_read: i64,
    /// The ephemeral split of `cache_create`. Carried separately because the
    /// two TTLs are priced differently (1h is 2x base input, 5m is 1.25x).
    pub cache_1h: i64,
    pub cache_5m: i64,
}

impl Row {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_create + self.cache_read
    }
}

/// Ledger totals measured against Claude Code's own, over the sessions where
/// both exist.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Coverage {
    pub sessions: i64,
    pub claude_tokens: i64,
    pub ledger_tokens: i64,
}

impl Coverage {
    pub fn percent(&self) -> Option<f64> {
        (self.claude_tokens > 0)
            .then(|| self.ledger_tokens as f64 / self.claude_tokens as f64 * 100.0)
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

/// The request and session writes shared by [`Ledger::ingest`] and
/// [`Ledger::ingest_batch`], so both run identical SQL inside whatever
/// transaction the caller opened.
fn insert_records(tx: &rusqlite::Transaction<'_>, records: &[UsageRecord]) -> Result<usize> {
    let mut inserted = 0usize;
    // Merge on conflict rather than ignore. Today every content-block line of
    // a response repeats a byte-identical usage object, so first-line-wins and
    // max() agree — but nothing guarantees Claude Code keeps buffering the
    // final usage onto every line. If a release ever wrote the first line with
    // `message_start` usage (output ~1) and the rest with the final figures,
    // INSERT OR IGNORE would keep the first and under-count silently forever.
    // Usage is monotonic within a response, so max() is always correct.
    //
    // The WHERE clause keeps the return value meaning "rows genuinely new":
    // an identical repeat updates nothing and reports 0.
    let mut stmt = tx.prepare_cached(
        "INSERT INTO requests
         (dedupe_key, session_id, project_dir, model, ts, input, output,
          cache_create, cache_read, thinking, cache_1h, cache_5m, source,
          transcript_path, speed, inference_geo)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
         ON CONFLICT(dedupe_key) DO UPDATE SET
           input        = max(requests.input,        excluded.input),
           output       = max(requests.output,       excluded.output),
           cache_create = max(requests.cache_create, excluded.cache_create),
           cache_read   = max(requests.cache_read,   excluded.cache_read),
           thinking     = max(requests.thinking,     excluded.thinking),
           cache_1h     = max(requests.cache_1h,     excluded.cache_1h),
           cache_5m     = max(requests.cache_5m,     excluded.cache_5m),
           speed          = coalesce(requests.speed,         excluded.speed),
           inference_geo  = coalesce(requests.inference_geo, excluded.inference_geo)
         WHERE excluded.input        > requests.input
            OR excluded.output       > requests.output
            OR excluded.cache_create > requests.cache_create
            OR excluded.cache_read   > requests.cache_read
            OR excluded.thinking     > requests.thinking
            OR excluded.cache_1h     > requests.cache_1h
            OR excluded.cache_5m     > requests.cache_5m",
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
            r.speed,
            r.inference_geo,
        ])?;
        if let Some(sid) = &r.session_id {
            session.execute(rusqlite::params![sid, r.project_dir, r.ts])?;
        }
    }
    Ok(inserted)
}

/// Snapshots are cumulative, so the latest one seen wins outright.
fn insert_cost_states(
    tx: &rusqlite::Transaction<'_>,
    states: &[CostState],
    now: i64,
) -> Result<()> {
    if states.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(
        "INSERT INTO cost_state
         (session_id, cost_usd, input, output, thinking, cache_read,
          cache_create, models, captured_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(session_id) DO UPDATE SET
           cost_usd     = excluded.cost_usd,
           input        = excluded.input,
           output       = excluded.output,
           thinking     = excluded.thinking,
           cache_read   = excluded.cache_read,
           cache_create = excluded.cache_create,
           models       = excluded.models,
           captured_at  = excluded.captured_at",
    )?;
    for s in states {
        stmt.execute(rusqlite::params![
            s.session_id,
            s.cost_usd,
            s.input,
            s.output,
            s.thinking,
            s.cache_read,
            s.cache_create,
            s.models,
            now,
        ])?;
    }
    Ok(())
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

    /// The pre-render summary read, which happens *before* anything reaches the
    /// screen and is not covered by the ingest budget. A WAL reader almost
    /// never blocks; the short wait only bounds the rare DDL race on a
    /// first-ever open, so a contended database cannot stall the status line.
    pub fn open_default_fast() -> Result<Self> {
        let path = default_db_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening ledger at {}", path.display()))?;
        Self::from_connection_with(conn, RENDER_BUSY_TIMEOUT_MS)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        Self::from_connection_with(conn, WRITE_BUSY_TIMEOUT_MS)
    }

    fn from_connection_with(conn: Connection, busy_ms: u64) -> Result<Self> {
        // WAL lets a reader (rendering) proceed while another session writes.
        // journal_mode is a no-op on :memory:, hence query_row rather than execute.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap_or_default();
        conn.busy_timeout(std::time::Duration::from_millis(busy_ms))?;
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
                transcript_path TEXT,
                -- Added after the first release. Databases created before this
                -- are widened by the ALTER pass below, not by this statement.
                speed           TEXT,
                inference_geo   TEXT
            );
            CREATE INDEX IF NOT EXISTS requests_ts         ON requests(ts);
            CREATE INDEX IF NOT EXISTS requests_model_ts   ON requests(model, ts);
            CREATE INDEX IF NOT EXISTS requests_project_ts ON requests(project_dir, ts);
            CREATE INDEX IF NOT EXISTS requests_session    ON requests(session_id);

            CREATE TABLE IF NOT EXISTS cursors (
                transcript_path TEXT PRIMARY KEY,
                byte_offset     INTEGER NOT NULL,
                file_id         INTEGER NOT NULL,
                last_seen       INTEGER NOT NULL,
                -- Added after the first release; 0 means "unknown".
                head_hash       INTEGER NOT NULL DEFAULT 0
            );

            -- Claude Code's own cumulative accounting, one row per session,
            -- superseded by each later snapshot. The only record of billed
            -- requests that never produced an assistant line (docs/formats.md
            -- 2.8), and pruned along with the transcripts that carry it.
            CREATE TABLE IF NOT EXISTS cost_state (
                session_id   TEXT PRIMARY KEY,
                cost_usd     REAL    NOT NULL DEFAULT 0,
                input        INTEGER NOT NULL DEFAULT 0,
                output       INTEGER NOT NULL DEFAULT 0,
                thinking     INTEGER NOT NULL DEFAULT 0,
                cache_read   INTEGER NOT NULL DEFAULT 0,
                cache_create INTEGER NOT NULL DEFAULT 0,
                models       TEXT,
                captured_at  INTEGER NOT NULL
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

        // `CREATE TABLE IF NOT EXISTS` does nothing to a table that already
        // exists, so columns added after a release have to be applied
        // explicitly or every existing ledger fails at insert time with "no
        // such column". Checking first makes this idempotent and keeps startup
        // free of expected-and-ignored errors.
        for (table, column, ddl) in [
            (
                "requests",
                "speed",
                "ALTER TABLE requests ADD COLUMN speed TEXT",
            ),
            (
                "requests",
                "inference_geo",
                "ALTER TABLE requests ADD COLUMN inference_geo TEXT",
            ),
            (
                "cursors",
                "head_hash",
                "ALTER TABLE cursors ADD COLUMN head_hash INTEGER NOT NULL DEFAULT 0",
            ),
        ] {
            if !self.has_column(table, column)? {
                self.conn.execute(ddl, [])?;
            }
        }
        Ok(())
    }

    fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        // PRAGMA arguments cannot be bound; `table` is always a literal here.
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(1)? == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Returns how many rows were genuinely new.
    pub fn ingest(&mut self, records: &[UsageRecord]) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let inserted = insert_records(&tx, records)?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Ingest one batch and advance the cursor **in a single transaction**.
    ///
    /// Atomicity matters in both directions. A kill between two separate
    /// commits would either lose the records (cursor behind the data —
    /// harmless, the next pass redoes them) or, far worse, leave the cursor
    /// *ahead* of committed data and skip those requests permanently. One
    /// transaction makes both impossible.
    ///
    /// Called with an empty `records` when a batch held only blank or
    /// unparseable lines: the cursor must still advance, or the reader would
    /// re-read them forever.
    /// Takes the whole [`Scan`] rather than its parts: everything the batch
    /// produced — requests, Claude Code's own session totals, and the cursor —
    /// belongs in one transaction, and this stops the signature churning each
    /// time a scan learns to extract something new.
    pub fn ingest_batch(&mut self, scan: &Scan, transcript_path: &str, now: i64) -> Result<usize> {
        let cursor = scan.cursor;
        let tx = self.conn.transaction()?;
        let inserted = insert_records(&tx, &scan.records)?;
        insert_cost_states(&tx, &scan.cost_states, now)?;
        tx.execute(
            "INSERT INTO cursors
             (transcript_path, byte_offset, file_id, last_seen, head_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(transcript_path) DO UPDATE SET
               byte_offset = excluded.byte_offset,
               file_id     = excluded.file_id,
               last_seen   = excluded.last_seen,
               head_hash   = excluded.head_hash",
            rusqlite::params![
                transcript_path,
                cursor.byte_offset as i64,
                cursor.file_id as i64,
                now,
                cursor.head_hash as i64,
            ],
        )?;
        tx.commit()?;
        Ok(inserted)
    }

    pub fn cursor(&self, transcript_path: &str) -> Result<Option<Cursor>> {
        let found = self
            .conn
            .query_row(
                "SELECT byte_offset, file_id, head_hash FROM cursors
                 WHERE transcript_path = ?1",
                [transcript_path],
                |row| {
                    Ok(Cursor {
                        byte_offset: row.get::<_, i64>(0)? as u64,
                        file_id: row.get::<_, i64>(1)? as u64,
                        head_hash: row.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(found)
    }

    pub fn set_cursor(&self, transcript_path: &str, cursor: Cursor, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cursors
             (transcript_path, byte_offset, file_id, last_seen, head_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(transcript_path) DO UPDATE SET
               byte_offset = excluded.byte_offset,
               file_id     = excluded.file_id,
               last_seen   = excluded.last_seen,
               head_hash   = excluded.head_hash",
            rusqlite::params![
                transcript_path,
                cursor.byte_offset as i64,
                cursor.file_id as i64,
                now,
                cursor.head_hash as i64,
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
    /// One indexed range scan, grouped by model so the cost can be derived in
    /// the same pass — pricing is per-model, so a single `sum()` could not be
    /// priced afterwards.
    ///
    /// `day_start` is used as the pricing timestamp for the whole day. No rate
    /// in the table varies within a day, so grouping by model rather than by
    /// request loses nothing (`src/pricing.rs`).
    pub fn summary(&self, day_start: i64) -> Result<LedgerSummary> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT coalesce(model, 'unknown'),
                    coalesce(sum(input), 0), coalesce(sum(output), 0),
                    coalesce(sum(cache_1h), 0), coalesce(sum(cache_5m), 0),
                    coalesce(sum(cache_read), 0),
                    coalesce(sum(input + output + cache_create + cache_read), 0)
             FROM requests WHERE ts >= ?1 GROUP BY 1",
        )?;

        let mut rows = stmt.query([day_start])?;
        let mut summary = LedgerSummary::default();
        while let Some(row) = rows.next()? {
            let model: String = row.get(0)?;
            summary.today_tokens += row.get::<_, i64>(6)?;
            if let Some(cost) = crate::pricing::cost_of(
                &model,
                day_start,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ) {
                summary.today_cost += cost.total();
            }
        }
        Ok(summary)
    }

    /// Rows in `[since, until)`, ordered by time, for `cc-usage` to bucket.
    pub fn rows(&self, since: Option<i64>, until: Option<i64>) -> Result<Vec<Row>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.ts, coalesce(r.model,'unknown'), coalesce(r.project_dir,'unknown'),
                    coalesce(s.project_dir, r.project_dir, 'unknown') AS project_root,
                    r.input, r.output, r.cache_create, r.cache_read, r.cache_1h, r.cache_5m
             FROM requests r
             LEFT JOIN sessions s ON s.session_id = r.session_id
             WHERE r.ts >= ?1 AND r.ts < ?2
             ORDER BY r.ts",
        )?;
        let rows = stmt
            .query_map(
                [since.unwrap_or(i64::MIN), until.unwrap_or(i64::MAX)],
                |row| {
                    Ok(Row {
                        ts: row.get(0)?,
                        model: row.get(1)?,
                        project_dir: row.get(2)?,
                        project_root: row.get(3)?,
                        input: row.get(4)?,
                        output: row.get(5)?,
                        cache_create: row.get(6)?,
                        cache_read: row.get(7)?,
                        cache_1h: row.get(8)?,
                        cache_5m: row.get(9)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// How much of Claude Code's own accounting the per-request rows account
    /// for, over the sessions where both are known.
    ///
    /// Expected to be slightly under 100%: the transcript omits requests that
    /// never produced an assistant record. A number far below, or above, means
    /// something is wrong rather than merely incomplete.
    pub fn coverage(&self) -> Result<Coverage> {
        let row = self.conn.query_row(
            "SELECT
               (SELECT count(*) FROM cost_state),
               (SELECT coalesce(sum(input + output + cache_create + cache_read), 0)
                  FROM cost_state),
               (SELECT coalesce(sum(r.input + r.output + r.cache_create + r.cache_read), 0)
                  FROM requests r
                 WHERE r.session_id IN (SELECT session_id FROM cost_state))",
            [],
            |r| {
                Ok(Coverage {
                    sessions: r.get(0)?,
                    claude_tokens: r.get(1)?,
                    ledger_tokens: r.get(2)?,
                })
            },
        )?;
        Ok(row)
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
            speed: Some("standard".into()),
            inference_geo: Some("not_available".into()),
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

    /// Content-block lines repeat a byte-identical usage object today, so
    /// first-line-wins and max() agree. Nothing guarantees Claude Code keeps
    /// buffering the final usage onto every line, though: were a release to
    /// write the first line with `message_start` usage (output ~1) and the rest
    /// with the final figures, `INSERT OR IGNORE` would keep the first and
    /// under-count silently. Usage is monotonic within a response, so merging
    /// with max() is always correct and cannot regress.
    #[test]
    fn a_later_line_with_higher_usage_wins() {
        let mut l = Ledger::open_in_memory().unwrap();

        let partial = record("msg_A", 1_000, 1);
        let complete = record("msg_A", 1_000, 589);

        l.ingest(std::slice::from_ref(&partial)).unwrap();
        assert_eq!(
            l.ingest(std::slice::from_ref(&complete)).unwrap(),
            1,
            "a higher figure is a genuine update"
        );

        let rows = l.rows(None, None).unwrap();
        assert_eq!(rows.len(), 1, "still one request, not two");
        assert_eq!(rows[0].output, 589, "kept the final usage, not the first");

        // Replaying the low value must not drag it back down, and must not be
        // reported as new work.
        assert_eq!(l.ingest(std::slice::from_ref(&partial)).unwrap(), 0);
        assert_eq!(l.rows(None, None).unwrap()[0].output, 589);
    }

    #[test]
    fn speed_and_inference_geo_survive_a_round_trip() {
        let mut l = Ledger::open_in_memory().unwrap();
        l.ingest(&[record("msg_A", 1_000, 10)]).unwrap();

        let stored: (Option<String>, Option<String>) = l
            .conn
            .query_row(
                "SELECT speed, inference_geo FROM requests WHERE dedupe_key = 'msg_A'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored.0.as_deref(), Some("standard"));
        assert_eq!(stored.1.as_deref(), Some("not_available"));
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
            head_hash: 0xDEAD_BEEF,
        };
        l.set_cursor("/t.jsonl", c, 1_000).unwrap();
        assert_eq!(
            l.cursor("/t.jsonl").unwrap(),
            Some(c),
            "head_hash must survive the round trip, or every resume restarts"
        );

        let c2 = Cursor {
            byte_offset: 8192,
            file_id: 77,
            head_hash: 0xDEAD_BEEF,
        };
        l.set_cursor("/t.jsonl", c2, 2_000).unwrap();
        assert_eq!(l.cursor("/t.jsonl").unwrap(), Some(c2));
    }

    #[test]
    fn summary_counts_only_today() {
        let mut l = Ledger::open_in_memory().unwrap();
        l.ingest(&[
            record("old", 1_000, 10),
            record("recent", 50_000, 20),
            record("newest", 90_000, 30),
        ])
        .unwrap();

        // Each record carries input 1 + cache_create 10 + cache_read 100 = 111
        // of fixed weight, plus its own output.
        assert_eq!(l.summary(60_000).unwrap().today_tokens, 141); // "newest" only
        assert_eq!(l.summary(40_000).unwrap().today_tokens, 272); // plus "recent"
        assert_eq!(l.summary(0).unwrap().today_tokens, 393); // all three
    }

    /// The cost comes from the same query as the tokens, priced per model.
    #[test]
    fn summary_prices_today_in_the_same_pass() {
        let mut l = Ledger::open_in_memory().unwrap();
        let mut opus = record("a", 1_000, 0);
        opus.model = "claude-opus-5".into();
        opus.input = 0;
        opus.output = 1_000_000; // $25 at Opus output rates
        opus.cache_create = 0;
        opus.cache_read = 0;
        opus.cache_1h = 0;
        opus.cache_5m = 0;

        let mut unknown = record("b", 1_000, 0);
        unknown.model = "claude-unreleased-9".into();
        unknown.input = 0;
        unknown.output = 1_000_000;
        unknown.cache_create = 0;
        unknown.cache_read = 0;
        unknown.cache_1h = 0;
        unknown.cache_5m = 0;

        l.ingest(&[opus, unknown]).unwrap();
        let s = l.summary(0).unwrap();

        assert_eq!(s.today_tokens, 2_000_000, "both models counted in tokens");
        assert!(
            (s.today_cost - 25.0).abs() < 1e-9,
            "only the priceable model contributes cost: {}",
            s.today_cost
        );
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

    /// Claude Code rewrites `cwd` mid-session when the working directory moves,
    /// so grouping by it scatters one project across every subdirectory the
    /// session visited. `project_root` must report where the session started.
    #[test]
    fn project_root_follows_the_session_not_the_request_cwd() {
        let mut l = Ledger::open_in_memory().unwrap();
        let mut root = record("a", 1_000, 10);
        root.project_dir = Some("/proj".into());
        let mut sub = record("b", 2_000, 10);
        sub.project_dir = Some("/proj/www".into());
        let mut deeper = record("c", 3_000, 10);
        deeper.project_dir = Some("/proj/www/includes".into());

        l.ingest(&[root, sub, deeper]).unwrap();

        let rows = l.rows(None, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .map(|r| r.project_dir.as_str())
                .collect::<Vec<_>>(),
            ["/proj", "/proj/www", "/proj/www/includes"],
            "the per-request cwd is preserved, not overwritten"
        );
        assert!(
            rows.iter().all(|r| r.project_root == "/proj"),
            "every row rolls up to the directory the session started in"
        );
    }

    /// A request whose session was never recorded must still be attributable.
    #[test]
    fn project_root_falls_back_to_the_request_cwd() {
        let mut l = Ledger::open_in_memory().unwrap();
        let mut orphan = record("a", 1_000, 10);
        orphan.session_id = None;
        orphan.project_dir = Some("/lonely".into());

        l.ingest(&[orphan]).unwrap();

        let rows = l.rows(None, None).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "a sessionless row is not dropped by the join"
        );
        assert_eq!(rows[0].project_root, "/lonely");
    }

    #[test]
    fn db_path_prefers_explicit_override() {
        // Safe: this process never reads the real env in other tests.
        std::env::set_var("CC_LEDGER_DB", "/tmp/explicit.db");
        assert_eq!(default_db_path(), PathBuf::from("/tmp/explicit.db"));
        std::env::remove_var("CC_LEDGER_DB");
    }
}
