//! Timing guards.
//!
//! These are budgets, not benchmarks: they exist to catch a regression that
//! makes the status line visibly slow, not to measure precisely.
//!
//! Thresholds are profile-aware. `cargo test` builds unoptimised, where
//! `serde_json` in particular runs several times slower than the release build
//! that actually gets installed, so asserting release numbers against a debug
//! binary would just produce a flaky test. The release figures are the ones in
//! the spec; the debug figures are loose sanity bounds.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cc_ledger::ledger::Ledger;
use cc_ledger::transcript;

const RELEASE_BUILD: bool = !cfg!(debug_assertions);

/// Cold status-line invocation with nothing to ingest.
fn render_budget() -> Duration {
    if RELEASE_BUILD {
        Duration::from_millis(20)
    } else {
        Duration::from_millis(250)
    }
}

/// One committed batch: lines scanned, inserted, and the cursor advanced, all
/// in a single transaction.
///
/// This replaced a whole-file budget, which measured something the binary never
/// does. The status line works to a 150 ms deadline and commits in batches, so
/// the figure that matters is per batch — it must be small enough that many
/// batches fit inside the deadline, which is what makes progress monotonic. The
/// old whole-file number looked reassuring at 500 ms while the binary was in
/// fact livelocked on any backlog above roughly 20 MB.
fn batch_budget() -> Duration {
    if RELEASE_BUILD {
        Duration::from_millis(25)
    } else {
        Duration::from_millis(400)
    }
}

/// Mirrors `INGEST_BATCH_LINES` in the status-line binary, which owns the real
/// constant. Kept in step by hand.
const BATCH_LINES: usize = 500;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cc-ledger-timing-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A realistic assistant record: the padding mimics a real `content` block, so
/// the parser does comparable work per line.
fn synth_line(index: usize) -> String {
    let padding = "x".repeat(900);
    format!(
        r#"{{"type":"assistant","uuid":"00000000-0000-4000-8000-{index:012}","parentUuid":null,"sessionId":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee","session_id":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee","requestId":"req_{index}","timestamp":"2026-09-05T13:56:32.140Z","cwd":"/home/user/projects/example","gitBranch":"main","version":"2.1.270","isSidechain":false,"userType":"external","entrypoint":"cli","apiBlockIndex":0,"effort":"high","message":{{"id":"msg_{index}","model":"claude-opus-5","role":"assistant","stop_reason":"tool_use","content":[{{"type":"text","text":"{padding}"}}],"usage":{{"input_tokens":2,"cache_creation_input_tokens":40446,"cache_read_input_tokens":34552,"output_tokens":589,"output_tokens_details":{{"thinking_tokens":226}},"server_tool_use":{{"web_search_requests":0,"web_fetch_requests":0}},"service_tier":"standard","cache_creation":{{"ephemeral_1h_input_tokens":40446,"ephemeral_5m_input_tokens":0}},"inference_geo":"not_available","speed":"standard"}}}}}}"#
    )
}

#[test]
fn cold_statusline_renders_within_budget() {
    let dir = temp_dir("render");
    let db = dir.join("ledger.db");

    // No transcript_path: this measures parse + ledger read + render only.
    let payload = serde_json::json!({
        "session_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
        "cwd": "/home/user/projects/example",
        "model": { "display_name": "Opus 5" },
        "context_window": { "context_window_size": 200000, "used_percentage": 42.0 }
    })
    .to_string();

    // Warm the database file so we time rendering, not schema creation.
    run_statusline(&db, &payload);

    let mut best = Duration::from_secs(99);
    for _ in 0..5 {
        let start = Instant::now();
        run_statusline(&db, &payload);
        best = best.min(start.elapsed());
    }

    println!(
        "cold statusline: {best:?} (budget {:?}, {} build)",
        render_budget(),
        if RELEASE_BUILD { "release" } else { "debug" }
    );
    assert!(
        best <= render_budget(),
        "status line took {best:?}, budget {:?}",
        render_budget()
    );

    std::fs::remove_dir_all(&dir).ok();
}

fn run_statusline(db: &PathBuf, payload: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_statusline"))
        .env("CC_LEDGER_DB", db)
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
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait().expect("statusline exits");
}

/// One batch must be cheap enough that many of them fit the status line's
/// 150 ms deadline — that headroom is exactly what makes progress monotonic
/// when a backlog cannot be drained in a single pass.
#[test]
fn one_committed_batch_stays_within_budget() {
    let dir = temp_dir("batch");
    let path = dir.join("big.jsonl");

    {
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        let mut written = 0usize;
        let mut index = 0usize;
        while written < 10 * 1024 * 1024 {
            let line = synth_line(index);
            writeln!(writer, "{line}").unwrap();
            written += line.len() + 1;
            index += 1;
        }
        writer.flush().unwrap();
    }

    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size >= 10 * 1024 * 1024, "fixture is {size} bytes");

    let mut ledger = Ledger::open(&dir.join("ledger.db")).unwrap();
    let key = path.to_string_lossy().to_string();

    // Scan, insert and commit exactly one batch with its cursor: the unit of
    // work the binary actually performs, rather than the whole file.
    let start = Instant::now();
    let scan = transcript::scan_batch(&path, None, Some(BATCH_LINES)).expect("scan succeeds");
    let inserted = ledger
        .ingest_batch(&scan.records, &key, scan.cursor, 0)
        .expect("ingest succeeds");
    let elapsed = start.elapsed();

    println!(
        "one batch of {inserted} records: {elapsed:?} (budget {:?}, {} build)",
        batch_budget(),
        if RELEASE_BUILD { "release" } else { "debug" }
    );

    assert_eq!(inserted, BATCH_LINES, "a full batch should be committed");
    assert!(
        !scan.exhausted,
        "the batch should stop at its line cap, not at end of file"
    );
    assert_eq!(scan.malformed, 0);
    assert!(
        elapsed <= batch_budget(),
        "one batch took {elapsed:?}, budget {:?}",
        batch_budget()
    );

    // The cursor advanced part-way, so the next batch resumes instead of
    // repeating — the property the livelock violated.
    assert!(scan.cursor.byte_offset > 0, "cursor did not advance");
    assert!(
        scan.cursor.byte_offset < size,
        "only part of the file should be consumed by one batch"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Resuming a fully-read transcript must be near-instant regardless of size:
/// this is what makes the per-keystroke status-line refresh affordable.
#[test]
fn resuming_an_unchanged_transcript_is_cheap() {
    let dir = temp_dir("resume");
    let path = dir.join("big.jsonl");

    {
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        for index in 0..2_000 {
            writeln!(writer, "{}", synth_line(index)).unwrap();
        }
        writer.flush().unwrap();
    }

    let first = transcript::scan(&path, None).expect("first scan");
    assert!(!first.records.is_empty());

    let start = Instant::now();
    let second = transcript::scan(&path, Some(first.cursor)).expect("second scan");
    let elapsed = start.elapsed();

    assert!(second.records.is_empty(), "nothing new to read");
    println!("no-op rescan: {elapsed:?}");
    assert!(
        elapsed <= Duration::from_millis(50),
        "no-op rescan took {elapsed:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
