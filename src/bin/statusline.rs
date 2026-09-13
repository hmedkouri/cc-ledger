//! The Claude Code status line.
//!
//! Order of operations is deliberate and must not be rearranged:
//!
//! 1. read and parse stdin;
//! 2. read the (cheap, indexed) ledger summary;
//! 3. render, print, flush — the user sees the line;
//! 4. only then ingest new transcript records, under a hard wall-clock budget.
//!
//! Nothing in steps 1–3 may fail loudly. A status line that errors, hangs or
//! prints a stack trace breaks Claude Code's UI, so every fallible step here
//! degrades to something printable and exits 0.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cc_ledger::bucket;
use cc_ledger::ledger::{Ledger, LedgerSummary};
use cc_ledger::payload::StatusPayload;
use cc_ledger::render::{self, RenderInput};
use cc_ledger::transcript;

/// Wall-clock budget for the post-render ingest. Exceeding it abandons this
/// pass at a committed batch boundary; the next invocation resumes from the
/// stored cursor, having kept everything already committed.
const INGEST_BUDGET: Duration = Duration::from_millis(150);

/// Lines per committed batch. Small enough that a batch and its commit take a
/// few milliseconds, so the deadline is checked often and an abandoned pass
/// discards almost no work.
const INGEST_BATCH_LINES: usize = 500;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Handled before touching stdin: run by hand from a terminal, this would
    // otherwise block forever waiting for a payload that is never typed. clap
    // is deliberately not linked into this binary — it runs on every render.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("statusline {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let short = args.iter().any(|a| a == "--short");
    let cost = args.iter().any(|a| a == "--cost");
    let home = std::env::var("HOME").unwrap_or_default();

    // Last line of defence for "never break Claude Code". A panic anywhere
    // below would otherwise exit 101 and spray a backtrace where the status
    // bar is drawn; here it degrades to the fallback line and exit 0. The hook
    // silences the default stderr message. `panic = "unwind"` is pinned in
    // Cargo.toml because catch_unwind catches nothing under `panic = "abort"`.
    std::panic::set_hook(Box::new(|_| {}));
    if std::panic::catch_unwind(|| run(short, cost, &home)).is_err() {
        print_line(&render::fallback(None, &home));
    }
}

/// Everything after argument handling, so that a panic in any of it — parsing,
/// git, rendering, SQLite — is contained rather than fatal.
fn run(short: bool, cost: bool, home: &str) {
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);

    let Some(payload) = StatusPayload::parse(&stdin) else {
        // Unparseable payload: still print something, still exit 0.
        print_line(&render::fallback(None, home));
        return;
    };

    let now = chrono::Local::now().timestamp();
    let tz = chrono::Local;
    let summary = read_summary(now, &tz).unwrap_or_default();

    let branch = payload.display_dir().and_then(|d| git_branch(Path::new(d)));

    let line = render::render(&RenderInput {
        payload: &payload,
        summary: &summary,
        branch: branch.as_deref(),
        home,
        short,
        now,
        cost,
    });
    print_line(&line);

    // The user has their line. Everything below is best-effort.
    ingest_within_budget(payload, now);
}

fn print_usage() {
    println!("statusline {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("The Claude Code status line for cc-ledger. Reads the status-line JSON");
    println!("payload on stdin, prints one line, then ingests new usage records from");
    println!("the session transcript into the ledger.");
    println!();
    println!("Usage: statusline [--short] [--cost] < payload.json");
    println!();
    println!("Options:");
    println!("      --short    Compact variant: directory, context percent, today's tokens");
    println!("      --cost     Render today as API-equivalent dollars rather than tokens");
    println!("  -h, --help     Print help");
    println!("  -V, --version  Print version");
    println!();
    println!("Not normally run by hand: Claude Code invokes it via the statusLine entry");
    println!("in ~/.claude/settings.json. Use `cc-usage` to query the ledger.");
}

fn print_line(line: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn read_summary<Tz: chrono::TimeZone>(now: i64, tz: &Tz) -> Option<LedgerSummary> {
    // Short lock wait: this runs before anything is on screen and no budget
    // covers it, so it must not be able to stall the line.
    let ledger = Ledger::open_default_fast().ok()?;
    ledger.summary(bucket::day_start(now, tz)).ok()
}

/// Run the ingest on a worker thread and stop caring after the budget.
///
/// The worker checks the same deadline itself between batches, so it stops at
/// a committed boundary rather than being killed mid-transaction; the
/// `recv_timeout` here is only a backstop for one batch overrunning.
///
/// Abandoning a pass is safe because each batch commits its records and its
/// cursor together: whatever committed stays, the cursor never runs ahead of
/// the data, and the next invocation resumes from exactly there.
fn ingest_within_budget(payload: StatusPayload, now: i64) {
    let deadline = std::time::Instant::now() + INGEST_BUDGET;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ingest(&payload, now, deadline));
    });
    let _ = rx.recv_timeout(INGEST_BUDGET);
}

fn ingest(payload: &StatusPayload, now: i64, deadline: std::time::Instant) -> anyhow::Result<()> {
    let mut ledger = Ledger::open_default()?;

    // Cheap and time-sensitive: the rate-limit percentages are the only
    // historical record of subscription consumption that will ever exist, and
    // they are gone once the window rolls over. Record them before the
    // potentially slow transcript read.
    let _ = ledger.record_limits(payload.five_hour(), payload.seven_day(), now);

    let Some(path) = payload.transcript_path.as_deref() else {
        return Ok(());
    };
    ingest_one(&mut ledger, Path::new(path), now, deadline)
}

/// Ingest in bounded batches, each committing its records and its cursor in one
/// transaction, with the deadline checked between batches.
///
/// This used to read the whole backlog into a single transaction. When the
/// backlog took longer than the budget the process exited before the commit,
/// the cursor never moved, and every later invocation repeated the same doomed
/// pass — measured, a transcript above roughly 20 MB was never ingested at all
/// while burning the full budget on every render. Batching makes progress
/// monotonic under any budget.
fn ingest_one(
    ledger: &mut Ledger,
    path: &Path,
    now: i64,
    deadline: std::time::Instant,
) -> anyhow::Result<()> {
    let key = path.to_string_lossy().to_string();

    loop {
        let cursor = ledger.cursor(&key).unwrap_or(None);
        let scan = transcript::scan_batch(path, cursor, Some(INGEST_BATCH_LINES))?;

        // Nothing new: skip the write entirely rather than touch the cursor
        // row on every render and contend for the lock for no reason.
        let advanced = cursor.map(|c| c.byte_offset) != Some(scan.cursor.byte_offset);
        if !scan.records.is_empty() || !scan.cost_states.is_empty() || advanced {
            ledger.ingest_batch(&scan, &key, now)?;
        }

        if scan.exhausted || std::time::Instant::now() >= deadline {
            return Ok(());
        }
    }
}

/// Resolve the branch by reading `.git/HEAD` rather than spawning `git`.
///
/// The reference implementation shells out on every render; that is a process
/// spawn per keystroke-ish refresh, and the status line re-runs several times
/// per assistant turn.
fn git_branch(dir: &Path) -> Option<String> {
    let mut current = Some(dir);
    while let Some(d) = current {
        let dot_git = d.join(".git");
        if dot_git.is_dir() {
            return head_branch(&dot_git);
        }
        if dot_git.is_file() {
            // Worktree or submodule: ".git" is a file pointing at the real dir.
            let contents = std::fs::read_to_string(&dot_git).ok()?;
            let target = contents.strip_prefix("gitdir:")?.trim();
            return head_branch(&PathBuf::from(target));
        }
        current = d.parent();
    }
    None
}

fn head_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        // Detached HEAD: show an abbreviated sha, as git itself would.
        //
        // Counted in chars, not bytes. `head[..7]` panics when byte 7 lands
        // inside a multi-byte character, and a panic here exits 101 — which is
        // exactly the "never break Claude Code" invariant this file claims to
        // uphold. A HEAD file is attacker-influenced in any cloned repository.
        None => {
            let abbrev: String = head.chars().take(7).collect();
            (abbrev.chars().count() == 7).then_some(abbrev)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branch_from_a_git_dir() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-git-{}", std::process::id()));
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();

        assert_eq!(git_branch(&dir), Some("feature/x".to_string()));

        // Also found from a nested directory.
        let nested = dir.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_branch(&nested), Some("feature/x".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn abbreviates_detached_head() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-detached-{}", std::process::id()));
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "0ce2532abcdef0123456789\n").unwrap();

        assert_eq!(git_branch(&dir), Some("0ce2532".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_git_directory_yields_no_branch() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Walks to the filesystem root without finding one; must not loop.
        assert!(git_branch(&dir).is_none() || git_branch(&dir).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
