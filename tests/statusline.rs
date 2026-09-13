//! End-to-end test of the `statusline` binary.
//!
//! Runs the real executable against a fixture payload and a fixture transcript,
//! then asserts both halves of its contract: the line it prints, and the rows
//! it leaves behind in the ledger.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cc_ledger::ledger::Ledger;

struct Sandbox {
    dir: PathBuf,
    db: PathBuf,
    transcript: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cc-ledger-it-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // A real .git/HEAD: the branch segment (and the lines-changed counter
        // that renders beside it) only appears for a directory inside a repo.
        // The status line reads HEAD directly rather than spawning git.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let transcript = dir.join("session.jsonl");
        std::fs::copy(fixture("transcript_oracle.jsonl"), &transcript).unwrap();

        Sandbox {
            db: dir.join("ledger.db"),
            transcript,
            dir,
        }
    }

    fn payload(&self) -> String {
        serde_json::json!({
            "session_id": "11111111-2222-4333-8444-555555555555",
            "transcript_path": self.transcript.to_string_lossy(),
            "cwd": self.dir.to_string_lossy(),
            "model": { "id": "claude-opus-5", "display_name": "Opus 5" },
            "workspace": {
                "current_dir": self.dir.to_string_lossy(),
                "project_dir": self.dir.to_string_lossy()
            },
            "cost": { "total_lines_added": 12, "total_lines_removed": 3 },
            "context_window": { "context_window_size": 200000, "used_percentage": 42.0 },
            "effort": { "level": "high" },
            "rate_limits": {
                "five_hour": { "used_percentage": 20.0, "resets_at": 1788616252 },
                "seven_day": { "used_percentage": 55.0, "resets_at": 1789616252 }
            }
        })
        .to_string()
    }

    fn run(&self, stdin: &str, args: &[&str]) -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_statusline"))
            .args(args)
            .env("CC_LEDGER_DB", &self.db)
            .env("HOME", "/home/user")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("statusline runs");

        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();

        let out = child.wait_with_output().expect("statusline exits");
        assert!(
            out.status.success(),
            "status line must always exit 0, got {:?}",
            out.status
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn ledger(&self) -> Ledger {
        Ledger::open(&self.db).expect("ledger opens")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Ingest runs on a worker thread that the binary abandons after its budget,
/// and that thread dies with the process — so polling the database after one
/// render cannot help: no further rows will ever appear from it. Re-render
/// instead. Each pass commits its batches, so progress across passes is
/// monotonic and this converges. Asserting after a single pass makes the test
/// flaky on a loaded machine for a reason unrelated to what it tests.
fn wait_for_rows(sandbox: &Sandbox, expected: i64) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let seen = sandbox.ledger().request_count().unwrap_or(0);
        if seen >= expected || std::time::Instant::now() >= deadline {
            return seen;
        }
        sandbox.run(&sandbox.payload(), &[]);
    }
}

#[test]
fn renders_the_line_and_ingests_the_transcript() {
    let sandbox = Sandbox::new("full");
    let stdout = sandbox.run(&sandbox.payload(), &[]);
    let text = strip_ansi(&stdout);

    // --- the rendered line -------------------------------------------------
    let leaf = sandbox
        .dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(text.contains(&leaf), "path keeps its last segment: {text}");
    assert!(text.contains("main"), "branch read from .git/HEAD: {text}");
    assert!(text.contains("Opus 5"), "model: {text}");
    assert!(text.contains("(high)"), "effort: {text}");
    assert!(text.contains("42%"), "context percent: {text}");
    assert!(text.contains("(+12 -3)"), "lines changed: {text}");
    assert!(text.contains("5h 20%"), "five hour limit: {text}");
    assert!(text.contains("7d 55%"), "seven day limit: {text}");
    assert_eq!(text.lines().count(), 1, "exactly one line: {text}");

    // --- the resulting ledger rows ----------------------------------------
    assert_eq!(
        wait_for_rows(&sandbox, 3),
        3,
        "three requests from five usage lines"
    );
    let ledger = sandbox.ledger();

    let rows = ledger.rows(None, None).unwrap();
    let output: i64 = rows.iter().map(|r| r.output).sum();
    assert_eq!(output, 157, "deduped output, not the 357 line-wise sum");

    let total: i64 = rows.iter().map(|r| r.total()).sum();
    assert_eq!(total, 16 + 157 + 7500 + 300);

    // The rate-limit observation is persisted, since it exists nowhere else.
    let sessions = ledger.sessions(None).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].tokens, total);
}

#[test]
fn second_invocation_does_not_double_count() {
    let sandbox = Sandbox::new("twice");
    sandbox.run(&sandbox.payload(), &[]);
    sandbox.run(&sandbox.payload(), &[]);
    sandbox.run(&sandbox.payload(), &[]);

    let ledger = sandbox.ledger();
    assert_eq!(ledger.request_count().unwrap(), 3);
    let output: i64 = ledger
        .rows(None, None)
        .unwrap()
        .iter()
        .map(|r| r.output)
        .sum();
    assert_eq!(output, 157);
}

#[test]
fn short_flag_renders_a_compact_line() {
    let sandbox = Sandbox::new("short");
    let long = strip_ansi(&sandbox.run(&sandbox.payload(), &[]));
    let short = strip_ansi(&sandbox.run(&sandbox.payload(), &["--short"]));

    assert!(short.len() < long.len(), "short: {short}\nlong: {long}");
    assert!(!short.contains("Opus 5"));
    assert!(short.contains("42%"));
}

/// `--help` and `--version` must be answered before stdin is read. Run by hand
/// from a terminal the binary would otherwise sit waiting for a payload nobody
/// is going to type. If the flag handling were removed this would print the
/// fallback status line instead of usage text, so the assertion catches it.
#[test]
fn help_and_version_are_answered_without_a_payload() {
    for flag in ["--help", "-h"] {
        let out = Command::new(env!("CARGO_BIN_EXE_statusline"))
            .arg(flag)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("statusline runs")
            .wait_with_output()
            .expect("statusline exits");

        assert!(out.status.success(), "{flag} must exit 0");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("Usage:"), "{flag} printed: {text}");
        assert!(text.contains("--short"), "{flag} should document --short");
    }

    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_statusline"))
            .arg(flag)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("statusline runs")
            .wait_with_output()
            .expect("statusline exits");

        assert!(out.status.success(), "{flag} must exit 0");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.starts_with("statusline ") && text.trim().len() > "statusline ".len(),
            "{flag} printed: {text}"
        );
    }
}

/// The detached-HEAD abbreviation used to slice `head[..7]` by *byte* index,
/// which panics when byte 7 lands inside a multi-byte character. That exited
/// 101 and printed a backtrace where Claude Code draws its status bar — a
/// direct violation of the invariant this file exists to defend, reachable
/// from any cloned repository. Asserts both the char-safe slicing and the
/// catch_unwind net behind it.
#[test]
fn a_git_head_that_once_panicked_still_exits_zero() {
    let dir = std::env::temp_dir().join(format!(
        "cc-ledger-head-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    // "abcdef" + 'é' is eight bytes, so byte 7 falls inside the 'é'. No
    // "ref: " prefix, so it is treated as a detached HEAD and abbreviated.
    std::fs::write(dir.join(".git").join("HEAD"), "abcdef\u{e9}\n").unwrap();

    let payload = serde_json::json!({
        "cwd": dir.to_string_lossy(),
        "workspace": { "current_dir": dir.to_string_lossy() },
        "model": { "display_name": "Opus 5" },
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_statusline"))
        .env("CC_LEDGER_DB", dir.join("ledger.db"))
        .env("HOME", "/home/user")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("statusline runs");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("statusline exits");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "must exit 0, got {:?}; stderr: {stderr}",
        out.status
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "must still print a usable line"
    );
    assert!(
        !stderr.contains("panicked"),
        "no panic message should reach stderr: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The status line must never break Claude Code, whatever arrives on stdin.
#[test]
fn malformed_payload_still_prints_and_exits_zero() {
    let sandbox = Sandbox::new("malformed");

    for input in ["", "not json at all", "{\"cwd\":", "[]", "null"] {
        let stdout = sandbox.run(input, &[]);
        assert!(
            !stdout.trim().is_empty(),
            "must print something for input {input:?}"
        );
        assert_eq!(stdout.lines().count(), 1);
    }
}

/// A payload with no transcript still renders; there is simply nothing to ingest.
#[test]
fn missing_transcript_path_is_not_an_error() {
    let sandbox = Sandbox::new("notranscript");
    let payload = serde_json::json!({
        "cwd": "/home/user/projects/example",
        "model": { "display_name": "Opus 5" }
    })
    .to_string();

    let text = strip_ansi(&sandbox.run(&payload, &[]));
    assert!(text.contains("Opus 5"), "{text}");
    assert_eq!(sandbox.ledger().request_count().unwrap(), 0);
}

/// A transcript that has grown since the last pass yields only the new rows.
#[test]
fn resumes_from_the_stored_cursor() {
    let sandbox = Sandbox::new("resume");
    sandbox.run(&sandbox.payload(), &[]);
    assert_eq!(sandbox.ledger().request_count().unwrap(), 3);

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&sandbox.transcript)
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","sessionId":"11111111-2222-4333-8444-555555555555","timestamp":"2026-09-05T11:00:00.000Z","cwd":"/home/user/projects/oracle","message":{{"id":"msg_ORACLE_D","model":"claude-opus-5","usage":{{"input_tokens":2,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
    )
    .unwrap();
    drop(f);

    sandbox.run(&sandbox.payload(), &[]);
    assert_eq!(sandbox.ledger().request_count().unwrap(), 4);

    let output: i64 = sandbox
        .ledger()
        .rows(None, None)
        .unwrap()
        .iter()
        .map(|r| r.output)
        .sum();
    assert_eq!(output, 160, "157 + the one appended request");
}
