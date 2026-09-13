//! Regression tests for incremental ingest under a wall-clock deadline.
//!
//! The bug these exist for: `scan` read an entire backlog into memory and
//! `ingest` committed it in one transaction, so a backlog larger than the
//! status line's budget committed *nothing*. The cursor never moved, and every
//! later invocation repeated the same doomed pass. Measured, a transcript above
//! roughly 20 MB was never ingested at all, while burning the full budget on
//! every single render.
//!
//! Nothing in the existing suite could see it. The timing test measured the
//! library with no deadline at all, and the end-to-end test used a five-line
//! fixture that always fit. The property that actually matters is the one
//! asserted here: **a backlog drains, whatever its size**.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Shaped like a real assistant record so per-line parse cost is comparable.
fn synth_line(index: usize) -> String {
    let padding = "x".repeat(900);
    format!(
        r#"{{"type":"assistant","uuid":"00000000-0000-4000-8000-{index:012}","sessionId":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee","requestId":"req_{index}","timestamp":"2026-09-05T13:56:32.140Z","cwd":"/home/user/projects/example","gitBranch":"main","version":"2.1.270","isSidechain":false,"apiBlockIndex":0,"message":{{"id":"msg_{index}","model":"claude-opus-5","role":"assistant","content":[{{"type":"text","text":"{padding}"}}],"usage":{{"input_tokens":2,"cache_creation_input_tokens":40446,"cache_read_input_tokens":34552,"output_tokens":589,"output_tokens_details":{{"thinking_tokens":226}},"cache_creation":{{"ephemeral_1h_input_tokens":40446,"ephemeral_5m_input_tokens":0}}}}}}}}"#
    )
}

fn write_transcript(path: &Path, megabytes: usize) -> usize {
    let file = std::fs::File::create(path).expect("create transcript");
    let mut writer = BufWriter::new(file);
    let (mut written, mut lines) = (0usize, 0usize);
    while written < megabytes * 1024 * 1024 {
        let line = synth_line(lines);
        writeln!(writer, "{line}").unwrap();
        written += line.len() + 1;
        lines += 1;
    }
    writer.flush().unwrap();
    lines
}

struct Fixture {
    dir: PathBuf,
    db: PathBuf,
    transcript: PathBuf,
    payload: String,
    lines: usize,
    size: u64,
}

impl Fixture {
    fn new(name: &str, megabytes: usize) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cc-ledger-ingest-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("session.jsonl");
        let lines = write_transcript(&transcript, megabytes);
        let size = std::fs::metadata(&transcript).unwrap().len();

        let payload = serde_json::json!({
            "session_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            "transcript_path": transcript.to_string_lossy(),
            "cwd": "/home/user/projects/example",
            "workspace": { "current_dir": "/home/user/projects/example" },
        })
        .to_string();

        Fixture {
            db: dir.join("ledger.db"),
            transcript,
            payload,
            lines,
            size,
            dir,
        }
    }

    /// One status-line invocation, exactly as Claude Code would make it.
    fn render(&self) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_statusline"))
            .env("CC_LEDGER_DB", &self.db)
            .env("HOME", "/home/user")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("statusline runs");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(self.payload.as_bytes())
            .unwrap();
        let status = child.wait().expect("statusline exits");
        assert!(status.success(), "status line must always exit 0");
    }

    fn cursor(&self) -> u64 {
        let Ok(ledger) = cc_ledger::ledger::Ledger::open(&self.db) else {
            return 0;
        };
        ledger
            .cursor(&self.transcript.to_string_lossy())
            .ok()
            .flatten()
            .map(|c| c.byte_offset)
            .unwrap_or(0)
    }

    fn rows(&self) -> i64 {
        cc_ledger::ledger::Ledger::open(&self.db)
            .and_then(|l| l.request_count())
            .unwrap_or(0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// The regression. Pre-fix this looped forever at `cursor == 0`.
#[test]
fn a_backlog_larger_than_the_budget_still_drains() {
    let fixture = Fixture::new("drain", 12);
    assert!(fixture.size > 8 * 1024 * 1024, "backlog should be large");

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last = 0u64;
    let mut passes = 0;

    while Instant::now() < deadline {
        fixture.render();
        passes += 1;
        let cursor = fixture.cursor();

        assert!(
            cursor >= last,
            "cursor went backwards: {cursor} after {last} (pass {passes})"
        );
        assert!(
            cursor > last || cursor == fixture.size,
            "pass {passes} committed nothing and the backlog is not drained \
             ({cursor} / {}): this is the livelock",
            fixture.size
        );

        last = cursor;
        if cursor == fixture.size {
            break;
        }
    }

    assert_eq!(
        last, fixture.size,
        "backlog never drained after {passes} passes"
    );
    assert_eq!(
        fixture.rows(),
        fixture.lines as i64,
        "every request in the backlog should be recorded"
    );
}

/// Once drained, renders must stop writing and stay cheap — the steady state
/// that the vast majority of invocations are in.
#[test]
fn a_drained_transcript_costs_nothing_to_re_render() {
    let fixture = Fixture::new("steady", 1);

    let deadline = Instant::now() + Duration::from_secs(60);
    while fixture.cursor() != fixture.size && Instant::now() < deadline {
        fixture.render();
    }
    assert_eq!(fixture.cursor(), fixture.size, "should drain quickly");

    let rows_before = fixture.rows();
    let start = Instant::now();
    fixture.render();
    let elapsed = start.elapsed();

    assert_eq!(fixture.rows(), rows_before, "no new rows from a no-op pass");
    assert_eq!(fixture.cursor(), fixture.size, "cursor unchanged");
    assert!(
        elapsed < Duration::from_secs(2),
        "a no-op render took {elapsed:?}"
    );
}

/// A transcript that grows between renders is picked up incrementally, without
/// re-reading what was already committed.
#[test]
fn appended_records_are_picked_up_on_the_next_render() {
    let fixture = Fixture::new("append", 1);

    let deadline = Instant::now() + Duration::from_secs(60);
    while fixture.cursor() != fixture.size && Instant::now() < deadline {
        fixture.render();
    }
    let rows_before = fixture.rows();

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&fixture.transcript)
        .unwrap();
    for i in 0..10 {
        writeln!(file, "{}", synth_line(1_000_000 + i)).unwrap();
    }
    drop(file);

    fixture.render();
    assert_eq!(
        fixture.rows(),
        rows_before + 10,
        "the ten appended requests should be ingested"
    );
}
