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
/// pass; the next invocation resumes from the stored cursor.
const INGEST_BUDGET: Duration = Duration::from_millis(150);

fn main() {
    let short = std::env::args().any(|a| a == "--short");
    let home = std::env::var("HOME").unwrap_or_default();

    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);

    let Some(payload) = StatusPayload::parse(&stdin) else {
        // Unparseable payload: still print something, still exit 0.
        print_line(&render::fallback(None, &home));
        return;
    };

    let now = chrono::Local::now().timestamp();
    let tz = chrono::Local;
    let summary = read_summary(&payload, now, &tz).unwrap_or_default();

    let branch = payload.display_dir().and_then(|d| git_branch(Path::new(d)));

    let line = render::render(&RenderInput {
        payload: &payload,
        summary: &summary,
        branch: branch.as_deref(),
        home: &home,
        short,
    });
    print_line(&line);

    // The user has their line. Everything below is best-effort.
    ingest_within_budget(payload, now);
}

fn print_line(line: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn read_summary<Tz: chrono::TimeZone>(
    payload: &StatusPayload,
    now: i64,
    tz: &Tz,
) -> Option<LedgerSummary> {
    let ledger = Ledger::open_default().ok()?;
    ledger
        .summary(
            payload.session_id.as_deref(),
            bucket::day_start(now, tz),
            bucket::week_start(now, tz),
        )
        .ok()
}

/// Run the ingest on a worker thread and give up on it after the budget.
///
/// Abandoning a pass is safe: SQLite's WAL keeps the database consistent, the
/// cursor is only advanced after its transaction commits, and every insert is
/// `INSERT OR IGNORE` on a stable key, so the next run redoes the work rather
/// than losing or double-counting it.
fn ingest_within_budget(payload: StatusPayload, now: i64) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ingest(&payload, now));
    });
    let _ = rx.recv_timeout(INGEST_BUDGET);
}

fn ingest(payload: &StatusPayload, now: i64) -> anyhow::Result<()> {
    let mut ledger = Ledger::open_default()?;

    // Cheap and time-sensitive: the rate-limit percentages are the only
    // historical record of subscription consumption that will ever exist, and
    // they are gone once the window rolls over. Record them before the
    // potentially slow transcript read.
    let _ = ledger.record_limits(payload.five_hour(), payload.seven_day(), now);

    let Some(path) = payload.transcript_path.as_deref() else {
        return Ok(());
    };
    ingest_one(&mut ledger, Path::new(path), now)
}

fn ingest_one(ledger: &mut Ledger, path: &Path, now: i64) -> anyhow::Result<()> {
    let key = path.to_string_lossy().to_string();
    let cursor = ledger.cursor(&key).unwrap_or(None);
    let scan = transcript::scan(path, cursor)?;

    ledger.ingest(&scan.records)?;
    // Only after the records are committed.
    ledger.set_cursor(&key, scan.cursor, now)?;
    Ok(())
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
        None if head.len() >= 7 => Some(head[..7].to_string()),
        None => None,
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
