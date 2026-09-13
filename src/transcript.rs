//! Incremental reader for Claude Code transcript JSONL files.
//!
//! See `docs/formats.md` §2 for the verified record shapes. The one fact that
//! drives this whole module: **a single API response is written as one JSONL
//! line per content block**, and every one of those lines repeats the
//! identical, complete `usage` object. Summing lines double-counts by ~2x
//! (4942 lines for 2481 real requests on the machine this was written against,
//! worst case 25 lines for one response).
//!
//! `message.id` is therefore the dedupe key. It is non-null on every usage
//! record and globally unique across every transcript on disk, so it is kept
//! global rather than scoped per file — a resumed-and-forked session that does
//! copy history will collide on it and be counted once, which is correct.

use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Longest line worth buffering. A record still being written — a large
/// attachment, say — would otherwise be read into memory in full on every
/// pass, only to be discarded because it has no newline yet. Over-length lines
/// are consumed and counted, never parsed.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// How much of a file's head is hashed to detect replacement. An inode can be
/// reused after delete-and-recreate; the offset alone would then resume into
/// the middle of an unrelated file and skip its beginning permanently.
const HEAD_HASH_BYTES: usize = 4096;

/// Where a request came from. Subagent records have never been observed on
/// disk (see `Source::from_sidechain` and `discover`), but the flag is cheap to
/// carry and impossible to reconstruct later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Main,
    Subagent,
}

impl Source {
    fn from_sidechain(is_sidechain: bool) -> Self {
        if is_sidechain {
            Source::Subagent
        } else {
            Source::Main
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Main => "main",
            Source::Subagent => "subagent",
        }
    }
}

/// One billed API request.
///
/// `thinking`, `cache_1h` and `cache_5m` are **subsets** of `output` and
/// `cache_create` respectively. They are stored alongside, never folded into,
/// the totals — see `total()`.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRecord {
    pub dedupe_key: String,
    pub session_id: Option<String>,
    pub project_dir: Option<String>,
    pub model: String,
    pub ts: i64,
    pub input: i64,
    pub output: i64,
    pub cache_create: i64,
    pub cache_read: i64,
    pub thinking: i64,
    pub cache_1h: i64,
    pub cache_5m: i64,
    /// `"standard"` or `"fast"`. Fast mode doubles Opus rates, so a ledger that
    /// drops this cannot price a fast-mode session. Captured even though no
    /// observed traffic uses it: transcripts are pruned, and a field not
    /// recorded before that is unrecoverable afterwards.
    pub speed: Option<String>,
    /// `"us"` pins inference to the United States and adds 10% to every token
    /// class. Captured for the same reason as `speed`.
    pub inference_geo: Option<String>,
    pub source: Source,
    pub transcript_path: String,
}

impl UsageRecord {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_create + self.cache_read
    }
}

/// Claude Code's own cumulative accounting for a session, from a `cost-state`
/// record.
///
/// This is the only trace of billed usage the per-request rows can never hold.
/// Requests that produced no `assistant` record — retries, aborted turns,
/// auxiliary generations — are billed and counted here but absent from the
/// transcript entirely (`docs/formats.md` §2.8), which is why ledger totals run
/// a few percent under. Capturing it turns "a number known to run low" into a
/// number with a stated error bar.
///
/// Snapshots are cumulative and re-emitted, so a later one supersedes an
/// earlier one. They are pruned with everything else, hence capturing now.
#[derive(Debug, Clone, PartialEq)]
pub struct CostState {
    pub session_id: String,
    pub cost_usd: f64,
    pub input: i64,
    pub output: i64,
    pub thinking: i64,
    pub cache_read: i64,
    pub cache_create: i64,
    /// Model ids exactly as Claude Code names them, comma-separated and sorted
    /// — including the `[1m]` suffix that `message.model` drops.
    pub models: String,
}

impl CostState {
    /// Comparable with a ledger total: `thinking` is a subset of `output` and
    /// is excluded, exactly as it is for a `UsageRecord`.
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_create + self.cache_read
    }
}

/// Position in a transcript, so the next pass resumes instead of re-reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub byte_offset: u64,
    /// Inode on unix. Used to notice the file was replaced rather than appended.
    pub file_id: u64,
    /// Hash of the file's first [`HEAD_HASH_BYTES`] bytes. An inode can be
    /// reused, so a delete-and-recreate that lands on the same number would
    /// otherwise resume mid-file and skip the new file's head — the one cursor
    /// failure that loses rows instead of merely re-reading them.
    ///
    /// Zero means "unknown", as written by a version before this existed; such
    /// a cursor is honoured rather than forcing a needless full re-read.
    pub head_hash: u64,
}

#[derive(Debug)]
pub struct Scan {
    pub records: Vec<UsageRecord>,
    /// Claude Code's own session totals seen in this batch, if any.
    pub cost_states: Vec<CostState>,
    pub cursor: Cursor,
    /// Lines that failed to parse. Skipped, counted, never fatal.
    pub malformed: usize,
    /// Lines that *were* billed requests — assistant records carrying a usage
    /// object — but could not be turned into a record, because `message.id`,
    /// `message.model` or `timestamp` was missing or unparseable. Each one is a
    /// billed request vanishing from the ledger, so it is counted rather than
    /// silently discarded.
    pub dropped: usize,
    /// True when the file shrank or its identity changed and we restarted.
    pub restarted: bool,
    /// False when the batch stopped at its line cap rather than at end of file,
    /// meaning more remains and the caller should commit and loop.
    pub exhausted: bool,
}

// ---------------------------------------------------------------------------
// Raw JSON shapes. Every field optional; unknown fields ignored by default.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    message: Option<RawMessage>,
    // Present only on `cost-state` records.
    #[serde(rename = "totalCostUSD")]
    total_cost_usd: Option<f64>,
    #[serde(rename = "modelUsage")]
    model_usage: Option<std::collections::BTreeMap<String, RawModelUsage>>,
}

/// Per-model totals inside a `cost-state` record. Note the camelCase names:
/// this object is shaped differently from `message.usage`.
#[derive(Deserialize)]
struct RawModelUsage {
    #[serde(rename = "inputTokens")]
    input_tokens: Option<i64>,
    #[serde(rename = "outputTokens")]
    output_tokens: Option<i64>,
    #[serde(rename = "thinkingTokens")]
    thinking_tokens: Option<i64>,
    #[serde(rename = "cacheReadInputTokens")]
    cache_read_input_tokens: Option<i64>,
    #[serde(rename = "cacheCreationInputTokens")]
    cache_creation_input_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct RawMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    output_tokens_details: Option<RawOutputDetails>,
    cache_creation: Option<RawCacheCreation>,
    speed: Option<String>,
    inference_geo: Option<String>,
}

#[derive(Deserialize)]
struct RawOutputDetails {
    thinking_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct RawCacheCreation {
    ephemeral_1h_input_tokens: Option<i64>,
    ephemeral_5m_input_tokens: Option<i64>,
}

fn parse_ts(raw: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw?)
        .ok()
        .map(|dt| dt.timestamp())
}

impl RawLine {
    /// Claude Code's cumulative totals for the session, summed across every
    /// model it lists. A `BTreeMap` keeps the model names sorted, so the stored
    /// string is stable between snapshots.
    fn into_cost_state(self) -> Option<CostState> {
        let session_id = self.session_id?;
        let usage = self.model_usage?;

        let mut state = CostState {
            session_id,
            cost_usd: self.total_cost_usd.unwrap_or(0.0),
            input: 0,
            output: 0,
            thinking: 0,
            cache_read: 0,
            cache_create: 0,
            models: usage.keys().cloned().collect::<Vec<_>>().join(","),
        };
        for m in usage.values() {
            state.input += m.input_tokens.unwrap_or(0);
            state.output += m.output_tokens.unwrap_or(0);
            state.thinking += m.thinking_tokens.unwrap_or(0);
            state.cache_read += m.cache_read_input_tokens.unwrap_or(0);
            state.cache_create += m.cache_creation_input_tokens.unwrap_or(0);
        }
        Some(state)
    }

    /// `dropped` counts billed requests that could not be recorded, so they
    /// surface as a number instead of disappearing.
    fn into_record(self, transcript_path: &str, dropped: &mut usize) -> Option<UsageRecord> {
        if self.kind.as_deref() != Some("assistant") {
            return None;
        }
        let message = self.message?;
        // No usage object means nothing was billed on this line.
        let usage = message.usage?;

        // Past this point the line *is* a billed request, so failing to record
        // it loses money from the ledger silently. Count it.
        let (Some(dedupe_key), Some(model), Some(ts)) = (
            message.id,
            message.model,
            parse_ts(self.timestamp.as_deref()),
        ) else {
            *dropped += 1;
            return None;
        };

        // `<synthetic>` records are local placeholders with all-zero usage, not
        // API calls. Not a drop: nothing was billed. Skipping them keeps the
        // model breakdown honest.
        if model.starts_with('<') {
            return None;
        }

        let details = usage.output_tokens_details;
        let cache = usage.cache_creation;

        Some(UsageRecord {
            dedupe_key,
            session_id: self.session_id,
            project_dir: self.cwd,
            model,
            ts,
            input: usage.input_tokens.unwrap_or(0),
            output: usage.output_tokens.unwrap_or(0),
            cache_create: usage.cache_creation_input_tokens.unwrap_or(0),
            cache_read: usage.cache_read_input_tokens.unwrap_or(0),
            thinking: details.and_then(|d| d.thinking_tokens).unwrap_or(0),
            cache_1h: cache
                .as_ref()
                .and_then(|c| c.ephemeral_1h_input_tokens)
                .unwrap_or(0),
            cache_5m: cache
                .as_ref()
                .and_then(|c| c.ephemeral_5m_input_tokens)
                .unwrap_or(0),
            speed: usage.speed,
            inference_geo: usage.inference_geo,
            source: Source::from_sidechain(self.is_sidechain.unwrap_or(false)),
            transcript_path: transcript_path.to_string(),
        })
    }
}

/// Read everything appended since `cursor`, in one unbounded pass.
///
/// Suitable for `backfill`, which has no deadline. A caller working to a
/// wall-clock budget must use [`scan_batch`] instead — see why there.
pub fn scan(path: &Path, cursor: Option<Cursor>) -> std::io::Result<Scan> {
    scan_batch(path, cursor, None)
}

/// Read at most `max_lines` complete lines appended since `cursor`.
///
/// Bounded batches exist so a caller can commit incrementally. Reading an
/// entire backlog into a single transaction means a caller working to a
/// deadline commits *nothing* once the backlog exceeds that deadline — and
/// then repeats the same doomed pass on every invocation, making no progress
/// ever. Batching makes progress monotonic: an abandoned pass loses at most
/// the batch in flight.
///
/// Only complete lines are consumed: a trailing partial line (a session
/// writing as we read) is left for the next pass, and the cursor stops short
/// of it. The caller advances its stored cursor only by committing.
pub fn scan_batch(
    path: &Path,
    cursor: Option<Cursor>,
    max_lines: Option<usize>,
) -> std::io::Result<Scan> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let file_id = file_id(&meta);
    let len = meta.len();
    let head_hash = head_hash(&mut file, len)?;

    // Rotation, truncation or reuse: the file we were reading is not this file,
    // it got shorter, or its head changed under a recycled inode. Either way
    // the stored offset is meaningless — start over. Re-ingesting is harmless
    // because the dedupe key is stable.
    let (start, restarted) = match cursor {
        Some(c)
            if c.file_id == file_id
                && c.byte_offset <= len
                && (c.head_hash == 0 || c.head_hash == head_hash) =>
        {
            (c.byte_offset, false)
        }
        Some(_) => (0, true),
        None => (0, false),
    };

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(start))?;

    let path_str = path.to_string_lossy();
    let mut records = Vec::new();
    let mut cost_states = Vec::new();
    let mut malformed = 0usize;
    let mut dropped = 0usize;
    let mut offset = start;
    let mut buf = Vec::new();
    let mut lines = 0usize;
    let mut exhausted = true;

    loop {
        if max_lines.is_some_and(|cap| lines >= cap) {
            // Stopped on a line boundary with more to read. The cursor is
            // valid here precisely because only whole lines were consumed.
            exhausted = false;
            break;
        }

        buf.clear();
        let n = (&mut reader)
            .take(MAX_LINE_BYTES as u64)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if buf.last() != Some(&b'\n') {
            if n == MAX_LINE_BYTES {
                // Over-length. Consume the remainder so the cursor can move
                // past it — capping the buffer without draining would stall
                // the reader on this line forever — but never parse it.
                let mut rest = Vec::new();
                let extra = reader.read_until(b'\n', &mut rest)?;
                if rest.last() == Some(&b'\n') {
                    offset += (n + extra) as u64;
                    lines += 1;
                    malformed += 1;
                    continue;
                }
            }
            // Ordinary partial line at EOF — a live session mid-write. Leave it.
            break;
        }
        offset += n as u64;
        lines += 1;

        let line = &buf[..n - 1];
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<RawLine>(line) {
            Ok(raw) => {
                if raw.kind.as_deref() == Some("cost-state") {
                    if let Some(state) = raw.into_cost_state() {
                        cost_states.push(state);
                    }
                } else if let Some(record) = raw.into_record(&path_str, &mut dropped) {
                    records.push(record);
                }
            }
            Err(_) => malformed += 1,
        }
    }

    Ok(Scan {
        records,
        cost_states,
        cursor: Cursor {
            byte_offset: offset,
            file_id,
            head_hash,
        },
        malformed,
        dropped,
        restarted,
        exhausted,
    })
}

/// Hash the file's **first line**, leaving the handle rewound to the start.
///
/// The first line never changes once written, so this is stable under append.
/// Hashing a fixed-size prefix instead is not: for a file shorter than the
/// window the hashed region grows with the file, so every append to a young
/// transcript looks like a replacement and restarts the scan from zero — on
/// every render, for exactly the sessions that are still being written.
///
/// Returns 0, meaning "unknown", when no newline falls within the cap. A file
/// whose first line is still mid-write is then honoured rather than restarted,
/// as are cursors written before this existed.
fn head_hash(file: &mut File, len: u64) -> std::io::Result<u64> {
    use std::hash::{Hash, Hasher};

    let want = HEAD_HASH_BYTES.min(len as usize);
    let mut head = vec![0u8; want];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut head)?;

    let Some(newline) = head.iter().position(|&b| b == b'\n') else {
        return Ok(0);
    };

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    head[..=newline].hash(&mut hasher);
    // Reserve zero for "unknown" so a real hash is never mistaken for one.
    Ok(hasher.finish() | 1)
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn file_id(meta: &std::fs::Metadata) -> u64 {
    // No stable id available; length is a weak proxy that still catches
    // truncation, which is the case that actually corrupts a cursor.
    meta.len()
}

/// Every transcript under `root`, recursively.
///
/// TODO(subagents): subagent transcripts are documented to live in
/// `subagents/*.jsonl` directories beside the parent session file, tagged
/// `source='subagent'`. No such directory, no `isSidechain: true` record and no
/// `Task` tool call exists anywhere on the machine this was written against, so
/// the layout is unverified and deliberately not special-cased — the recursive
/// walk below already picks such files up, it just cannot attribute them to a
/// parent session. Discovery heuristic when a real one appears: look for
/// `<session-dir>/subagents/*.jsonl` next to `<session-id>.jsonl`, confirm the
/// records carry `isSidechain: true`, capture one as a fixture, then map each
/// file back to the parent via its `sessionId` field rather than its path.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // unreadable directory: skip, never abort a backfill
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "jsonl") => {
                    found.push(path)
                }
                _ => {}
            }
        }
    }

    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcript_main.jsonl")
    }

    fn scan_all(path: &Path) -> Scan {
        scan(path, None).expect("scan succeeds")
    }

    #[test]
    fn collapses_repeated_content_block_lines_to_one_request() {
        let scan = scan_all(&fixture());
        let keys: Vec<&str> = scan.records.iter().map(|r| r.dedupe_key.as_str()).collect();

        // msg_FIXTURE0 occupies three lines (apiBlockIndex 0,1,2) with an
        // identical usage object. It must appear three times here — the reader
        // reports lines, the ledger's primary key collapses them — but the
        // three must be byte-identical so that collapsing is lossless.
        let repeats: Vec<&UsageRecord> = scan
            .records
            .iter()
            .filter(|r| r.dedupe_key == "msg_FIXTURE0")
            .collect();
        assert_eq!(repeats.len(), 3, "fixture should carry the 3-line group");
        assert_eq!(repeats[0].output, repeats[1].output);
        assert_eq!(repeats[0].output, repeats[2].output);
        assert_eq!(repeats[0].input, repeats[2].input);
        assert_eq!(repeats[0].cache_read, repeats[2].cache_read);

        assert!(keys.contains(&"msg_FIXTURE3"));
        assert!(keys.contains(&"msg_FIXTURE4"));
    }

    #[test]
    fn skips_synthetic_non_api_records() {
        let scan = scan_all(&fixture());
        assert!(
            !scan
                .records
                .iter()
                .any(|r| r.dedupe_key == "msg_FIXTURESYNTH"),
            "<synthetic> records are local placeholders, not billed requests"
        );
    }

    /// A billed request that cannot be recorded must surface as a number.
    /// Previously `into_record` returned `None` for a missing `message.id`,
    /// `model` or timestamp exactly as it does for a user line, so a billed
    /// request could vanish with nothing counting it.
    #[test]
    fn billed_requests_that_cannot_be_recorded_are_counted() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-dropped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-09-05T15:00:00.000Z","message":{"id":"msg_OK","model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":2}}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-09-05T15:00:01.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":9,"output_tokens":9}}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"not-a-date","message":{"id":"msg_B","model":"claude-opus-5","usage":{"input_tokens":9,"output_tokens":9}}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-09-05T15:00:02.000Z","message":{"id":"msg_C","model":"claude-opus-5"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-09-05T15:00:03.000Z","message":{"id":"msg_D","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let scan = scan(&path, None).unwrap();

        assert_eq!(scan.records.len(), 1, "only the usable record");
        assert_eq!(scan.malformed, 0, "every line is valid JSON");
        assert_eq!(
            scan.dropped, 2,
            "the id-less and the bad-timestamp lines were billed but unrecordable"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn counts_malformed_lines_without_aborting() {
        let scan = scan_all(&fixture());
        assert_eq!(scan.malformed, 1, "fixture contains one unparseable line");
        assert!(
            scan.records.len() >= 5,
            "records after the malformed line are still read"
        );
    }

    #[test]
    fn ignores_non_assistant_records() {
        let scan = scan_all(&fixture());
        assert!(scan.records.iter().all(|r| r.model != "user"));
        assert_eq!(scan.records.iter().filter(|r| r.input == 0).count(), 0);
    }

    #[test]
    fn tags_sidechain_records_as_subagent() {
        let scan = scan_all(&fixture());
        let side = scan
            .records
            .iter()
            .find(|r| r.dedupe_key == "msg_FIXTURESIDE")
            .expect("sidechain record present");
        assert_eq!(side.source, Source::Subagent);
        assert_eq!(side.model, "claude-sonnet-5");
        assert_eq!(side.thinking, 7);
        assert_eq!(side.cache_1h, 20);
        assert_eq!(side.cache_5m, 2);
    }

    #[test]
    fn tolerates_records_missing_optional_fields() {
        let scan = scan_all(&fixture());
        let minimal = scan
            .records
            .iter()
            .find(|r| r.dedupe_key == "msg_FIXTUREMINIMAL")
            .expect("minimal record present");
        assert_eq!(minimal.session_id, None);
        assert_eq!(minimal.project_dir, None);
        assert_eq!(minimal.thinking, 0);
        assert_eq!(minimal.source, Source::Main);
        assert_eq!(minimal.total(), 11);
    }

    #[test]
    fn subset_columns_are_never_folded_into_total() {
        let scan = scan_all(&fixture());
        let side = scan
            .records
            .iter()
            .find(|r| r.dedupe_key == "msg_FIXTURESIDE")
            .unwrap();
        // thinking(7) is part of output(44); cache_1h+cache_5m(22) is
        // cache_create(22). Total must count each token exactly once.
        assert_eq!(side.total(), 11 + 22 + 33 + 44);
    }

    #[test]
    fn resumes_from_cursor_after_append() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::copy(fixture(), &path).unwrap();

        let first = scan(&path, None).unwrap();
        let seen = first.records.len();
        assert!(seen > 0);

        // Nothing new: a second pass returns no records and the same cursor.
        let second = scan(&path, Some(first.cursor)).unwrap();
        assert!(second.records.is_empty());
        assert_eq!(second.cursor, first.cursor);
        assert!(!second.restarted);

        // Append one request; only that one comes back.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-09-05T15:00:00.000Z","message":{{"id":"msg_APPENDED","model":"claude-opus-5","usage":{{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        )
        .unwrap();

        let third = scan(&path, Some(second.cursor)).unwrap();
        assert_eq!(third.records.len(), 1);
        assert_eq!(third.records[0].dedupe_key, "msg_APPENDED");
        assert!(third.cursor.byte_offset > second.cursor.byte_offset);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaves_trailing_partial_line_for_the_next_pass() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");

        let whole = "{\"type\":\"assistant\",\"timestamp\":\"2026-09-05T15:00:00.000Z\",\"message\":{\"id\":\"msg_A\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n";
        let partial = "{\"type\":\"assistant\",\"timestamp\":\"2026-09";
        std::fs::write(&path, format!("{whole}{partial}")).unwrap();

        let first = scan(&path, None).unwrap();
        assert_eq!(first.records.len(), 1);
        assert_eq!(first.malformed, 0, "a partial line is not malformed");
        assert_eq!(first.cursor.byte_offset, whole.len() as u64);

        // Complete that line; the next pass picks it up whole.
        let rest = "-05T15:00:01.000Z\",\"message\":{\"id\":\"msg_B\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(rest.as_bytes()).unwrap();

        let second = scan(&path, Some(first.cursor)).unwrap();
        assert_eq!(second.records.len(), 1);
        assert_eq!(second.records[0].dedupe_key, "msg_B");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restarts_when_file_is_truncated() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::copy(fixture(), &path).unwrap();

        let first = scan(&path, None).unwrap();
        assert!(first.cursor.byte_offset > 0);

        // Rewrite the file shorter, keeping a valid record.
        std::fs::write(
            &path,
            "{\"type\":\"assistant\",\"timestamp\":\"2026-09-05T15:00:00.000Z\",\"message\":{\"id\":\"msg_AFTER_TRUNCATE\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
        )
        .unwrap();

        let stale = Cursor {
            byte_offset: first.cursor.byte_offset,
            file_id: first.cursor.file_id,
            head_hash: first.cursor.head_hash,
        };
        let second = scan(&path, Some(stale)).unwrap();
        assert!(second.restarted, "shrunk file must restart from zero");
        assert_eq!(second.records.len(), 1);
        assert_eq!(second.records[0].dedupe_key, "msg_AFTER_TRUNCATE");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restarts_when_file_identity_changes() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::copy(fixture(), &path).unwrap();

        let first = scan(&path, None).unwrap();
        let bogus = Cursor {
            byte_offset: 0,
            file_id: first.cursor.file_id.wrapping_add(1),
            head_hash: first.cursor.head_hash,
        };
        let second = scan(&path, Some(bogus)).unwrap();
        assert!(second.restarted, "different inode must restart");
        assert_eq!(second.records.len(), first.records.len());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An inode is reusable: delete and recreate can land on the same number.
    /// Offset and inode would then both look valid while pointing into the
    /// middle of an unrelated file, permanently skipping its head — the one
    /// cursor failure that loses rows rather than merely re-reading them.
    #[test]
    fn restarts_when_the_file_head_changes_under_the_same_inode() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-reuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");

        let original = "{\"type\":\"assistant\",\"timestamp\":\"2026-09-05T15:00:00.000Z\",\"message\":{\"id\":\"msg_FIRST\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n";
        std::fs::write(&path, original).unwrap();

        let first = scan(&path, None).unwrap();
        assert_eq!(first.records.len(), 1);
        assert_ne!(first.cursor.head_hash, 0, "a real hash is never zero");

        // Rewrite in place: same path, same inode, but longer — so both the
        // inode check and the `offset <= len` check still pass. Only the head
        // has changed. Without hashing, the scan would resume at the old offset
        // and never see msg_NEW_A.
        let replacement = concat!(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-09-05T16:00:00.000Z\",\"message\":{\"id\":\"msg_NEW_A\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-09-05T16:00:01.000Z\",\"message\":{\"id\":\"msg_NEW_B\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
        );
        std::fs::write(&path, replacement).unwrap();

        let second = scan(&path, Some(first.cursor)).unwrap();
        assert!(second.restarted, "a changed head must force a restart");
        assert_eq!(
            second.records.len(),
            2,
            "the replacement file's head must not be skipped"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Appending to a young transcript must resume, not restart.
    ///
    /// Regression: hashing a fixed-size prefix rather than the first line made
    /// the hashed region grow with any file shorter than the window, so every
    /// append looked like a replacement. Live sessions append constantly, so
    /// this would have re-read them from zero on every render.
    #[test]
    fn appending_to_a_small_file_does_not_restart() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-grow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");

        let line = |id: &str| {
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"2026-09-05T15:00:00.000Z\",\"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":2}}}}}}\n"
            )
        };

        std::fs::write(&path, line("msg_A")).unwrap();
        let first = scan(&path, None).unwrap();
        assert_eq!(first.records.len(), 1);
        assert_ne!(first.cursor.head_hash, 0, "a complete first line hashes");

        // The file is far shorter than HEAD_HASH_BYTES, which is the case that
        // previously broke.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(line("msg_B").as_bytes()).unwrap();
        drop(f);

        let second = scan(&path, Some(first.cursor)).unwrap();
        assert!(!second.restarted, "an append must resume, not restart");
        assert_eq!(second.records.len(), 1, "only the appended record");
        assert_eq!(second.records[0].dedupe_key, "msg_B");
        assert_eq!(
            second.cursor.head_hash, first.cursor.head_hash,
            "the first line did not change, so neither should its hash"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Cursors written before head hashing existed store zero. They must be
    /// honoured, not treated as a mismatch forcing a pointless full re-read.
    #[test]
    fn an_unknown_head_hash_is_honoured() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::copy(fixture(), &path).unwrap();

        let first = scan(&path, None).unwrap();
        let legacy = Cursor {
            head_hash: 0,
            ..first.cursor
        };

        let second = scan(&path, Some(legacy)).unwrap();
        assert!(!second.restarted, "a pre-hash cursor should still resume");
        assert!(second.records.is_empty(), "nothing new to read");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A line past the cap is consumed and counted but never parsed, and must
    /// not stall the reader: capping the buffer without draining the line would
    /// leave the cursor stuck before it forever.
    #[test]
    fn an_over_long_line_is_skipped_without_stalling() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-longline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");

        let mut content = String::with_capacity(MAX_LINE_BYTES + 4096);
        content.push_str("{\"type\":\"assistant\",\"pad\":\"");
        content.push_str(&"x".repeat(MAX_LINE_BYTES + 16));
        content.push_str("\"}\n");
        content.push_str("{\"type\":\"assistant\",\"timestamp\":\"2026-09-05T15:00:00.000Z\",\"message\":{\"id\":\"msg_AFTER\",\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n");
        std::fs::write(&path, &content).unwrap();

        let scanned = scan(&path, None).unwrap();

        assert_eq!(scanned.malformed, 1, "counted, not parsed");
        assert_eq!(scanned.records.len(), 1, "the record after it is found");
        assert_eq!(scanned.records[0].dedupe_key, "msg_AFTER");
        assert_eq!(
            scanned.cursor.byte_offset,
            content.len() as u64,
            "the cursor moved past the over-long line"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discover_finds_nested_transcripts() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-disc-{}", std::process::id()));
        let nested = dir.join("proj").join("subagents");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("proj").join("a.jsonl"), "").unwrap();
        std::fs::write(nested.join("b.jsonl"), "").unwrap();
        std::fs::write(dir.join("proj").join("ignore.txt"), "").unwrap();

        let found = discover(&dir);
        assert_eq!(found.len(), 2, "recursive, extension-filtered: {found:?}");
        assert!(found.iter().all(|p| p.extension().unwrap() == "jsonl"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn timestamps_parse_as_utc_seconds() {
        assert_eq!(parse_ts(Some("2026-09-05T13:56:32.140Z")), Some(1788616592));
        assert_eq!(parse_ts(Some("not a timestamp")), None);
        assert_eq!(parse_ts(None), None);
    }
}
