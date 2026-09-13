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
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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
    pub source: Source,
    pub transcript_path: String,
}

impl UsageRecord {
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
}

#[derive(Debug)]
pub struct Scan {
    pub records: Vec<UsageRecord>,
    pub cursor: Cursor,
    /// Lines that failed to parse. Skipped, counted, never fatal.
    pub malformed: usize,
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
    fn into_record(self, transcript_path: &str) -> Option<UsageRecord> {
        if self.kind.as_deref() != Some("assistant") {
            return None;
        }
        let message = self.message?;
        let usage = message.usage?;
        let dedupe_key = message.id?;
        let model = message.model?;

        // `<synthetic>` records are local placeholders with all-zero usage,
        // not API calls. Skipping them keeps the model breakdown honest.
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
            ts: parse_ts(self.timestamp.as_deref())?,
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
    let file = File::open(path)?;
    let meta = file.metadata()?;
    let file_id = file_id(&meta);
    let len = meta.len();

    // Rotation or truncation: the file we were reading is not this file, or it
    // got shorter. Either way the stored offset is meaningless — start over.
    // Re-ingesting is harmless because the dedupe key is stable.
    let (start, restarted) = match cursor {
        Some(c) if c.file_id == file_id && c.byte_offset <= len => (c.byte_offset, false),
        Some(_) => (0, true),
        None => (0, false),
    };

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(start))?;

    let path_str = path.to_string_lossy();
    let mut records = Vec::new();
    let mut malformed = 0usize;
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
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if buf.last() != Some(&b'\n') {
            // Partial line at EOF — a live session mid-write. Leave it.
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
                if let Some(record) = raw.into_record(&path_str) {
                    records.push(record);
                }
            }
            Err(_) => malformed += 1,
        }
    }

    Ok(Scan {
        records,
        cursor: Cursor {
            byte_offset: offset,
            file_id,
        },
        malformed,
        restarted,
        exhausted,
    })
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
        };
        let second = scan(&path, Some(bogus)).unwrap();
        assert!(second.restarted, "different inode must restart");
        assert_eq!(second.records.len(), first.records.len());

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
