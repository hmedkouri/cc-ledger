//! Query the token ledger.
//!
//! Tokens are the unit. There is deliberately no cost estimate: transcripts
//! record `claude-opus-5` whether or not the account is running the 1M-context
//! variant, which is priced very differently, so any per-model price table
//! silently under-reports. `docs/formats.md` §2.6 has the evidence. The
//! subscription signal worth watching is the rate-limit history the status line
//! records instead.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use cc_ledger::bucket;
use cc_ledger::ledger::{Ledger, Row};
use cc_ledger::transcript;
use chrono::Local;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "cc-usage",
    about = "Query the cc-ledger Claude Code token ledger"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Totals over a period.
    Summary {
        #[command(flatten)]
        range: Range,
    },
    /// Per-day breakdown.
    Daily {
        #[command(flatten)]
        range: Range,
        #[arg(long, value_enum)]
        by: Option<GroupBy>,
    },
    /// Per-week breakdown (weeks start Monday, local time).
    Weekly {
        #[command(flatten)]
        range: Range,
        #[arg(long, value_enum)]
        by: Option<GroupBy>,
    },
    /// Per-month breakdown.
    Monthly {
        #[command(flatten)]
        range: Range,
        #[arg(long, value_enum)]
        by: Option<GroupBy>,
    },
    /// Sessions, most recently active first.
    Sessions {
        #[arg(long)]
        project: Option<String>,
    },
    /// Walk every transcript on disk and ingest what is missing.
    Backfill {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Dump raw rows.
    Export {
        #[arg(long, value_enum)]
        format: Format,
        #[command(flatten)]
        range: Range,
    },
}

#[derive(Args, Clone)]
struct Range {
    /// Inclusive start date, YYYY-MM-DD, local time.
    #[arg(long)]
    since: Option<String>,
    /// Exclusive end date, YYYY-MM-DD, local time.
    #[arg(long)]
    until: Option<String>,
}

impl Range {
    fn resolve(&self) -> Result<(Option<i64>, Option<i64>)> {
        let parse = |text: &Option<String>, what: &str| -> Result<Option<i64>> {
            match text {
                None => Ok(None),
                Some(t) => bucket::parse_date(t, &Local)
                    .map(Some)
                    .with_context(|| format!("{what} is not a YYYY-MM-DD date: {t}")),
            }
        };
        Ok((
            parse(&self.since, "--since")?,
            parse(&self.until, "--until")?,
        ))
    }
}

#[derive(Copy, Clone, ValueEnum)]
enum GroupBy {
    Model,
    Project,
}

#[derive(Copy, Clone, ValueEnum)]
enum Format {
    Csv,
    Json,
}

#[derive(Copy, Clone)]
enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    fn label(self, ts: i64) -> String {
        match self {
            Period::Day => bucket::day_label(ts, &Local),
            Period::Week => bucket::week_label(ts, &Local),
            Period::Month => bucket::month_label(ts, &Local),
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Totals {
    requests: i64,
    input: i64,
    output: i64,
    cache_create: i64,
    cache_read: i64,
}

impl Totals {
    fn add(&mut self, row: &Row) {
        self.requests += 1;
        self.input += row.input;
        self.output += row.output;
        self.cache_create += row.cache_create;
        self.cache_read += row.cache_read;
    }

    fn total(&self) -> i64 {
        self.input + self.output + self.cache_create + self.cache_read
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Summary { range } => summary(range),
        Command::Daily { range, by } => breakdown(range, by, Period::Day),
        Command::Weekly { range, by } => breakdown(range, by, Period::Week),
        Command::Monthly { range, by } => breakdown(range, by, Period::Month),
        Command::Sessions { project } => sessions(project),
        Command::Backfill { root } => backfill(root),
        Command::Export { format, range } => export(format, range),
    }
}

fn open() -> Result<Ledger> {
    Ledger::open_default()
}

fn summary(range: Range) -> Result<()> {
    let (since, until) = range.resolve()?;
    let rows = open()?.rows(since, until)?;

    let mut totals = Totals::default();
    let mut by_model: BTreeMap<String, Totals> = BTreeMap::new();
    let mut by_project: BTreeMap<String, Totals> = BTreeMap::new();
    for row in &rows {
        totals.add(row);
        by_model.entry(row.model.clone()).or_default().add(row);
        by_project
            .entry(row.project_dir.clone())
            .or_default()
            .add(row);
    }

    if rows.is_empty() {
        println!("No requests in range. Run `cc-usage backfill` if the ledger is new.");
        return Ok(());
    }

    let first = rows.first().map(|r| r.ts).unwrap_or_default();
    let last = rows.last().map(|r| r.ts).unwrap_or_default();
    println!(
        "{} .. {}   {} requests",
        bucket::day_label(first, &Local),
        bucket::day_label(last, &Local),
        totals.requests
    );
    println!();

    print_table("model", &by_model);
    println!();
    let width = print_table("project", &by_project);
    println!();
    print_row("TOTAL", &totals, width);

    Ok(())
}

fn breakdown(range: Range, by: Option<GroupBy>, period: Period) -> Result<()> {
    let (since, until) = range.resolve()?;
    let rows = open()?.rows(since, until)?;
    if rows.is_empty() {
        println!("No requests in range.");
        return Ok(());
    }

    let mut buckets: BTreeMap<String, BTreeMap<String, Totals>> = BTreeMap::new();
    for row in &rows {
        let key = match by {
            Some(GroupBy::Model) => row.model.clone(),
            Some(GroupBy::Project) => shorten(&row.project_dir),
            None => String::new(),
        };
        buckets
            .entry(period.label(row.ts))
            .or_default()
            .entry(key)
            .or_default()
            .add(row);
    }

    let width = match by {
        None => 10,
        Some(_) => 10 + 2 + group_width(&buckets),
    };
    print_header(width);

    let mut grand = Totals::default();
    for (label, groups) in &buckets {
        for (group, totals) in groups {
            let name = if group.is_empty() {
                label.clone()
            } else {
                format!("{label}  {group}")
            };
            print_row(&name, totals, width);
            grand.requests += totals.requests;
            grand.input += totals.input;
            grand.output += totals.output;
            grand.cache_create += totals.cache_create;
            grand.cache_read += totals.cache_read;
        }
    }
    println!();
    print_row("TOTAL", &grand, width);
    Ok(())
}

fn group_width(buckets: &BTreeMap<String, BTreeMap<String, Totals>>) -> usize {
    buckets
        .values()
        .flat_map(|g| g.keys())
        .map(|k| k.len())
        .max()
        .unwrap_or(0)
}

fn sessions(project: Option<String>) -> Result<()> {
    let rows = open()?.sessions(project.as_deref())?;
    if rows.is_empty() {
        println!("No sessions recorded.");
        return Ok(());
    }
    println!(
        "{:<38} {:>12} {:>12}  project",
        "session", "tokens", "last seen"
    );
    for s in rows {
        println!(
            "{:<38} {:>12} {:>12}  {}",
            s.session_id,
            thousands(s.tokens),
            bucket::day_label(s.last_seen, &Local),
            shorten(&s.project_dir)
        );
    }
    Ok(())
}

fn backfill(root: Option<PathBuf>) -> Result<()> {
    let root = root.unwrap_or_else(default_root);
    let paths = transcript::discover(&root);
    println!(
        "Scanning {} transcripts under {}",
        paths.len(),
        root.display()
    );

    let mut ledger = open()?;
    let now = chrono::Local::now().timestamp();
    let (mut inserted, mut malformed, mut failed) = (0usize, 0usize, 0usize);

    for path in &paths {
        let key = path.to_string_lossy().to_string();
        let cursor = ledger.cursor(&key).unwrap_or(None);
        match transcript::scan(path, cursor) {
            Ok(scan) => {
                malformed += scan.malformed;
                match ledger.ingest(&scan.records) {
                    Ok(n) => {
                        inserted += n;
                        let _ = ledger.set_cursor(&key, scan.cursor, now);
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("ingest failed for {}: {e}", path.display());
                    }
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("read failed for {}: {e}", path.display());
            }
        }
    }

    println!(
        "Added {inserted} new requests ({} total in ledger).",
        ledger.request_count()?
    );
    if malformed > 0 {
        println!("Skipped {malformed} unparseable lines.");
    }
    if failed > 0 {
        println!("{failed} transcripts could not be read.");
    }
    for (session, count) in ledger.duplicate_sources()? {
        println!("note: session {session} appears in {count} transcripts");
    }
    Ok(())
}

fn default_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude").join("projects")
}

fn export(format: Format, range: Range) -> Result<()> {
    let (since, until) = range.resolve()?;
    let rows = open()?.rows(since, until)?;

    match format {
        Format::Csv => {
            println!("timestamp,model,project_dir,input,output,cache_create,cache_read,total");
            for r in &rows {
                println!(
                    "{},{},{},{},{},{},{},{}",
                    chrono::DateTime::from_timestamp(r.ts, 0)
                        .unwrap_or_default()
                        .to_rfc3339(),
                    csv_escape(&r.model),
                    csv_escape(&r.project_dir),
                    r.input,
                    r.output,
                    r.cache_create,
                    r.cache_read,
                    r.total()
                );
            }
        }
        Format::Json => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "timestamp": chrono::DateTime::from_timestamp(r.ts, 0)
                            .unwrap_or_default().to_rfc3339(),
                        "model": r.model,
                        "project_dir": r.project_dir,
                        "input": r.input,
                        "output": r.output,
                        "cache_create": r.cache_create,
                        "cache_read": r.cache_read,
                        "total": r.total(),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&items)?);
        }
    }
    Ok(())
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn print_header(width: usize) {
    println!(
        "{:<width$} {:>8} {:>12} {:>12} {:>13} {:>13} {:>14}",
        "", "requests", "input", "output", "cache-create", "cache-read", "total",
    );
}

/// Returns the name-column width it used, so a following TOTAL row lines up.
fn print_table(title: &str, groups: &BTreeMap<String, Totals>) -> usize {
    let width = groups
        .keys()
        .map(|k| shorten(k).len())
        .max()
        .unwrap_or(0)
        .max(title.len());
    println!(
        "{:<width$} {:>8} {:>12} {:>12} {:>13} {:>13} {:>14}",
        title, "requests", "input", "output", "cache-create", "cache-read", "total",
    );
    for (name, totals) in groups {
        print_row(&shorten(name), totals, width);
    }
    width
}

fn print_row(name: &str, t: &Totals, width: usize) {
    println!(
        "{:<width$} {:>8} {:>12} {:>12} {:>13} {:>13} {:>14}",
        name,
        thousands(t.requests),
        thousands(t.input),
        thousands(t.output),
        thousands(t.cache_create),
        thousands(t.cache_read),
        thousands(t.total()),
    );
}

/// Project directories are long and share a prefix; show the tail.
fn shorten(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && path.starts_with(&home) {
        return path.replacen(&home, "~", 1);
    }
    path.to_string()
}

fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1 000");
        assert_eq!(thousands(47_312_579), "47 312 579");
        assert_eq!(thousands(-1_234), "-1 234");
    }

    #[test]
    fn csv_escapes_separators_and_quotes() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn totals_exclude_nothing_and_double_count_nothing() {
        let row = Row {
            ts: 0,
            model: "m".into(),
            project_dir: "/p".into(),
            input: 1,
            output: 2,
            cache_create: 4,
            cache_read: 8,
        };
        let mut t = Totals::default();
        t.add(&row);
        t.add(&row);
        assert_eq!(t.requests, 2);
        assert_eq!(t.total(), 30);
    }

    #[test]
    fn range_rejects_bad_dates() {
        let bad = Range {
            since: Some("not-a-date".into()),
            until: None,
        };
        assert!(bad.resolve().is_err());

        let good = Range {
            since: Some("2026-09-05".into()),
            until: None,
        };
        let (since, until) = good.resolve().unwrap();
        assert!(since.is_some());
        assert!(until.is_none());
    }
}
