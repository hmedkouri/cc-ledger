//! Query the token ledger.
//!
//! Tokens are the unit. `--cost` additionally prices them at Anthropic's
//! published API list rates and reports the result as **API Cost**: what the
//! usage would have cost on the API, which is not what a subscription charges.
//!
//! Pricing keys on the bare model id recorded in the transcript, which is
//! sufficient because long context bills at standard rates — see
//! `src/pricing.rs` and `docs/formats.md` §2.6. Each row is priced
//! individually, since a project bucket mixes models and cannot be priced from
//! its aggregate.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use cc_ledger::bucket;
use cc_ledger::ledger::{Ledger, Row};
use cc_ledger::pricing::{self, Cost};
use cc_ledger::transcript;
use chrono::Local;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "cc-usage",
    version,
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
        /// Price usage at API list rates and report it as API Cost.
        #[arg(long)]
        cost: bool,
    },
    /// Per-day breakdown.
    Daily {
        #[command(flatten)]
        range: Range,
        /// Split each bucket by model or by project.
        #[arg(long, value_enum)]
        by: Option<GroupBy>,
        /// Price usage at API list rates and report it as API Cost.
        #[arg(long)]
        cost: bool,
    },
    /// Per-week breakdown (weeks start Monday, local time).
    Weekly {
        #[command(flatten)]
        range: Range,
        /// Split each bucket by model or by project.
        #[arg(long, value_enum)]
        by: Option<GroupBy>,
        /// Price usage at API list rates and report it as API Cost.
        #[arg(long)]
        cost: bool,
    },
    /// Per-month breakdown.
    Monthly {
        #[command(flatten)]
        range: Range,
        /// Split each bucket by model or by project.
        #[arg(long, value_enum)]
        by: Option<GroupBy>,
        /// Price usage at API list rates and report it as API Cost.
        #[arg(long)]
        cost: bool,
    },
    /// Sessions, most recently active first.
    Sessions {
        /// Only sessions whose project directory matches this path exactly.
        #[arg(long)]
        project: Option<String>,
    },
    /// Walk every transcript on disk and ingest what is missing.
    Backfill {
        /// Transcript root to walk [default: ~/.claude/projects].
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Delete ledger rows older than a date, then compact the database.
    Prune {
        /// Delete everything strictly before this date (YYYY-MM-DD, local).
        #[arg(long)]
        before: String,
        /// Actually delete. Without it, prune only reports what it would do.
        #[arg(long)]
        yes: bool,
    },
    /// Dump raw rows.
    Export {
        /// Output format for the dumped rows.
        #[arg(long, value_enum)]
        format: Format,
        #[command(flatten)]
        range: Range,
    },
    /// Print the settings.json key that points Claude Code at the status line.
    Install,
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
    cost: Cost,
    /// Tokens belonging to a model that is not in the price table. Reported
    /// rather than valued at zero, so an unrecognised model shows as a gap.
    unpriced: i64,
}

impl Totals {
    /// Priced per row, never per bucket: a project bucket mixes models, so the
    /// aggregate cannot be priced after the fact.
    fn add(&mut self, row: &Row) {
        self.requests += 1;
        self.input += row.input;
        self.output += row.output;
        self.cache_create += row.cache_create;
        self.cache_read += row.cache_read;

        match pricing::cost_of(
            &row.model,
            row.ts,
            row.input,
            row.output,
            row.cache_1h,
            row.cache_5m,
            row.cache_read,
        ) {
            Some(cost) => self.cost.add(cost),
            None => self.unpriced += row.total(),
        }
    }

    fn merge(&mut self, other: &Totals) {
        self.requests += other.requests;
        self.input += other.input;
        self.output += other.output;
        self.cache_create += other.cache_create;
        self.cache_read += other.cache_read;
        self.cost.add(other.cost);
        self.unpriced += other.unpriced;
    }

    fn total(&self) -> i64 {
        self.input + self.output + self.cache_create + self.cache_read
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Summary { range, cost } => summary(range, cost),
        Command::Daily { range, by, cost } => breakdown(range, by, Period::Day, cost),
        Command::Weekly { range, by, cost } => breakdown(range, by, Period::Week, cost),
        Command::Monthly { range, by, cost } => breakdown(range, by, Period::Month, cost),
        Command::Sessions { project } => sessions(project),
        Command::Backfill { root } => backfill(root),
        Command::Prune { before, yes } => prune(before, yes),
        Command::Export { format, range } => export(format, range),
        Command::Install => install(),
    }
}

fn open() -> Result<Ledger> {
    Ledger::open_default()
}

/// Print the `statusLine` key for `~/.claude/settings.json`, with the absolute
/// path Claude Code needs.
///
/// Printing rather than writing is deliberate. A broken status line degrades
/// Claude Code; a malformed `settings.json` stops it starting, and that file is
/// the one an installer would have to edit. `make statusline-apply` is the only
/// writer, and it validates with `jq` and keeps a backup first.
fn install() -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate the running cc-usage binary")?;
    let statusline = exe.with_file_name("statusline");
    println!("{}", settings_key(&statusline.to_string_lossy()));
    Ok(())
}

fn settings_key(statusline: &str) -> String {
    // serde_json does the escaping, so a path holding a quote or a backslash
    // survives the paste as valid JSON.
    format!(
        "\"statusLine\": {{\"type\": \"command\", \"command\": {}}}",
        serde_json::Value::String(statusline.to_owned())
    )
}

fn summary(range: Range, cost: bool) -> Result<()> {
    let (since, until) = range.resolve()?;
    let rows = open()?.rows(since, until)?;

    let mut totals = Totals::default();
    let mut by_model: BTreeMap<String, Totals> = BTreeMap::new();
    let mut by_project: BTreeMap<String, Totals> = BTreeMap::new();
    for row in &rows {
        totals.add(row);
        by_model.entry(row.model.clone()).or_default().add(row);
        by_project
            .entry(row.project_root.clone())
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

    print_table("model", &by_model, cost);
    println!();
    let width = print_table("project", &by_project, cost);
    println!();
    print_row("TOTAL", &totals, width, cost);

    if cost {
        print_cost_breakdown(&totals);
    }

    print_coverage(&open()?.coverage().unwrap_or_default());

    Ok(())
}

/// States the error bar rather than leaving the reader to trust a figure known
/// to run low. Claude Code bills requests that never reach the transcript, so
/// slightly under 100% is the expected, healthy result.
fn print_coverage(coverage: &cc_ledger::ledger::Coverage) {
    let Some(percent) = coverage.percent() else {
        return;
    };
    println!();
    println!(
        "Coverage: {percent:.1}% of the tokens Claude Code itself counted across \
         {} session(s)",
        coverage.sessions
    );
    println!("with its own accounting. The shortfall is requests that never produced a");
    println!("transcript record — retries, aborted turns — and is expected.");
}

fn breakdown(range: Range, by: Option<GroupBy>, period: Period, cost: bool) -> Result<()> {
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
            Some(GroupBy::Project) => shorten(&row.project_root),
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
    print_header(width, cost);

    let mut grand = Totals::default();
    for (label, groups) in &buckets {
        for (group, totals) in groups {
            let name = if group.is_empty() {
                label.clone()
            } else {
                format!("{label}  {group}")
            };
            print_row(&name, totals, width, cost);
            grand.merge(totals);
        }
    }
    println!();
    print_row("TOTAL", &grand, width, cost);

    if cost {
        print_cost_breakdown(&grand);
    }
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
    let (mut inserted, mut malformed, mut failed, mut dropped) = (0usize, 0usize, 0usize, 0usize);

    for path in &paths {
        let key = path.to_string_lossy().to_string();
        let cursor = ledger.cursor(&key).unwrap_or(None);
        match transcript::scan(path, cursor) {
            Ok(scan) => {
                malformed += scan.malformed;
                dropped += scan.dropped;
                // Records and cursor in one transaction. An interrupted
                // backfill must never leave the cursor ahead of the committed
                // data, which would skip those requests permanently.
                match ledger.ingest_batch(&scan, &key, now) {
                    Ok(n) => inserted += n,
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
    if dropped > 0 {
        println!(
            "Warning: {dropped} billed requests could not be recorded (missing id, \
             model or timestamp) and are absent from every total."
        );
    }
    if failed > 0 {
        println!("{failed} transcripts could not be read.");
    }
    for (session, count) in ledger.duplicate_sources()? {
        println!("note: session {session} appears in {count} transcripts");
    }

    // Housekeeping that only backfill can do. Status line processes are
    // short-lived and sometimes killed mid-pass, so they cannot be relied on to
    // checkpoint the WAL or to notice that a transcript has been pruned.
    let stale = ledger.stale_cursors()?;
    if !stale.is_empty() {
        let removed = ledger.delete_cursors(&stale)?;
        println!("Dropped {removed} cursors whose transcript no longer exists.");
    }
    ledger.checkpoint()?;

    Ok(())
}

/// Reports before it deletes, and refuses to delete without `--yes`.
fn prune(before: String, yes: bool) -> Result<()> {
    let cutoff = bucket::parse_date(&before, &Local)
        .with_context(|| format!("--before is not a YYYY-MM-DD date: {before}"))?;

    let mut ledger = open()?;
    let counts = ledger.prune_preview(cutoff)?;
    let bytes_before = ledger.file_bytes()?;

    println!(
        "Older than {before}: {} requests, {} rate-limit observations, \
         {} session cost snapshots.",
        thousands(counts.requests),
        thousands(counts.limits),
        thousands(counts.cost_state),
    );

    if counts.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }

    // Only an estimate: how much a delete actually frees is not known until
    // VACUUM has repacked the pages, so this is measured bytes-per-row times
    // the rows going away.
    let rows_total = ledger.request_count()?.max(1);
    let approx = counts.requests * (bytes_before / rows_total);
    println!(
        "Database is {} now; removing those would free roughly {}.",
        bytes_human(bytes_before),
        bytes_human(approx),
    );

    if !yes {
        println!();
        println!("Nothing was deleted. Re-run with --yes to apply.");
        println!("Pruned rows are gone for good: the transcripts they came from");
        println!("may already have been deleted by Claude Code's own cleanup.");
        return Ok(());
    }

    let removed = ledger.prune(cutoff)?;
    let bytes_after = ledger.file_bytes()?;
    println!(
        "Deleted {} requests, {} limits, {} cost snapshots.",
        thousands(removed.requests),
        thousands(removed.limits),
        thousands(removed.cost_state),
    );
    println!(
        "Database {} → {}, {} reclaimed.",
        bytes_human(bytes_before),
        bytes_human(bytes_after),
        bytes_human(bytes_before - bytes_after),
    );
    Ok(())
}

fn bytes_human(bytes: i64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{} kB", bytes / 1024)
    }
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
            println!(
                "timestamp,model,project_root,project_dir,input,output,cache_create,cache_read,total"
            );
            for r in &rows {
                println!(
                    "{},{},{},{},{},{},{},{},{}",
                    chrono::DateTime::from_timestamp(r.ts, 0)
                        .unwrap_or_default()
                        .to_rfc3339(),
                    csv_escape(&r.model),
                    csv_escape(&r.project_root),
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
                        "project_root": r.project_root,
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

fn header_line(name: &str, width: usize, cost: bool) -> String {
    let base = format!(
        "{:<width$} {:>8} {:>12} {:>12} {:>13} {:>13} {:>14}",
        name, "requests", "input", "output", "cache-create", "cache-read", "total",
    );
    if cost {
        format!("{base} {:>12}", "API Cost")
    } else {
        base
    }
}

fn print_header(width: usize, cost: bool) {
    println!("{}", header_line("", width, cost));
}

/// Returns the name-column width it used, so a following TOTAL row lines up.
fn print_table(title: &str, groups: &BTreeMap<String, Totals>, cost: bool) -> usize {
    let width = groups
        .keys()
        .map(|k| shorten(k).len())
        .max()
        .unwrap_or(0)
        .max(title.len());
    println!("{}", header_line(title, width, cost));
    for (name, totals) in groups {
        print_row(&shorten(name), totals, width, cost);
    }
    width
}

fn print_row(name: &str, t: &Totals, width: usize, cost: bool) {
    let base = format!(
        "{:<width$} {:>8} {:>12} {:>12} {:>13} {:>13} {:>14}",
        name,
        thousands(t.requests),
        thousands(t.input),
        thousands(t.output),
        thousands(t.cache_create),
        thousands(t.cache_read),
        thousands(t.total()),
    );
    if cost {
        println!("{base} {:>12}", pricing::format_usd(t.cost.total()));
    } else {
        println!("{base}");
    }
}

/// The breakdown that answers "where does the money actually go".
fn print_cost_breakdown(t: &Totals) {
    println!();
    println!("API Cost — list-price estimate of what this usage would have cost");
    println!("on the API. It is not what a subscription charges.");
    println!("Reconstructed from local transcripts, not billing — see Accuracy in README.");
    println!();
    let line = |label: &str, amount: f64| {
        let share = if t.cost.total() > 0.0 {
            format!("{:>5.1}%", amount / t.cost.total() * 100.0)
        } else {
            "    -".to_string()
        };
        println!("  {label:<12} {:>12}  {share}", pricing::format_usd(amount));
    };
    line("input", t.cost.input);
    line("output", t.cost.output);
    line("cache write", t.cost.cache_write);
    line("cache read", t.cost.cache_read);
    println!(
        "  {:<12} {:>12}",
        "TOTAL",
        pricing::format_usd(t.cost.total())
    );

    if t.unpriced > 0 {
        println!();
        println!(
            "  note: {} tokens came from models with no entry in the price",
            thousands(t.unpriced)
        );
        println!("  table and are excluded from the figures above.");
    }
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

    /// The printed key is pasted straight into settings.json, so it has to
    /// parse as JSON there — including when the path holds a quote.
    #[test]
    fn settings_key_pastes_as_valid_json() {
        for path in ["/home/u/.claude/bin/statusline", "/tmp/we\"ird/statusline"] {
            let object = format!("{{{}}}", settings_key(path));
            let parsed: serde_json::Value =
                serde_json::from_str(&object).expect("printed key is not valid JSON");
            assert_eq!(parsed["statusLine"]["type"], "command");
            assert_eq!(parsed["statusLine"]["command"], path);
        }
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
            project_root: "/p".into(),
            input: 1,
            output: 2,
            cache_create: 4,
            cache_read: 8,
            cache_1h: 4,
            cache_5m: 0,
        };
        let mut t = Totals::default();
        t.add(&row);
        t.add(&row);
        assert_eq!(t.requests, 2);
        assert_eq!(t.total(), 30);
    }

    fn row(model: &str) -> Row {
        Row {
            ts: 0,
            model: model.into(),
            project_dir: "/p".into(),
            project_root: "/p".into(),
            input: 0,
            output: 0,
            cache_create: 0,
            cache_read: 0,
            cache_1h: 0,
            cache_5m: 0,
        }
    }

    /// A project bucket mixes models, so each row must be priced at its own
    /// rate before aggregation — 1M Opus output ($25) + 1M Sonnet output ($10).
    #[test]
    fn each_row_is_priced_by_its_own_model() {
        let opus = Row {
            output: 1_000_000,
            ..row("claude-opus-5")
        };
        let sonnet = Row {
            output: 1_000_000,
            ..row("claude-sonnet-5")
        };

        let mut t = Totals::default();
        t.add(&opus);
        t.add(&sonnet);

        assert!((t.cost.output - 35.0).abs() < 1e-9, "{}", t.cost.output);
        assert_eq!(t.unpriced, 0);
    }

    /// An unrecognised model must surface as unpriced tokens, never as free
    /// usage that quietly shrinks the total.
    #[test]
    fn unknown_models_are_reported_not_silently_free() {
        let unknown = Row {
            input: 10,
            output: 20,
            cache_create: 30,
            cache_read: 40,
            cache_1h: 30,
            ..row("claude-unreleased-9")
        };

        let mut t = Totals::default();
        t.add(&unknown);

        assert_eq!(t.cost.total(), 0.0);
        assert_eq!(t.unpriced, 100, "tokens are reported, not valued at zero");
    }

    /// The TTL split must reach the price: 1h writes cost 2x base input, 5m
    /// writes 1.25x. Collapsing them understates a 1h-only workload by 37.5%.
    #[test]
    fn cache_write_ttl_split_reaches_the_price() {
        let one_hour = Row {
            cache_create: 1_000_000,
            cache_1h: 1_000_000,
            ..row("claude-opus-5")
        };
        let five_min = Row {
            cache_create: 1_000_000,
            cache_5m: 1_000_000,
            ..row("claude-opus-5")
        };

        let mut a = Totals::default();
        a.add(&one_hour);
        let mut b = Totals::default();
        b.add(&five_min);

        assert!((a.cost.cache_write - 10.00).abs() < 1e-9);
        assert!((b.cost.cache_write - 6.25).abs() < 1e-9);
    }

    #[test]
    fn merge_preserves_cost_and_unpriced() {
        let mut known = Totals::default();
        known.add(&Row {
            output: 1_000_000,
            ..row("claude-opus-5")
        });
        let mut unknown = Totals::default();
        unknown.add(&Row {
            output: 5,
            ..row("nope")
        });

        known.merge(&unknown);
        assert!((known.cost.total() - 25.0).abs() < 1e-9);
        assert_eq!(known.unpriced, 5);
        assert_eq!(known.requests, 2);
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
