//! The cost-state reconciliation test.
//!
//! Claude Code writes its own cumulative token accounting into `cost-state`
//! records. That makes it an independent check on our dedupe: if we collapse
//! content-block lines correctly, our per-request sums equal Claude's own
//! totals for the session.
//!
//! This runs against a **fixture**, deliberately. On live transcripts the
//! comparison is unsound in two documented ways (`docs/formats.md` §2.8):
//! requests that never produced an `assistant` record are billed but absent,
//! and `cost-state` has been observed dropping an entire model from
//! `modelUsage`. The fixture encodes the well-behaved case, which is the one
//! that actually tests our arithmetic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use cc_ledger::ledger::Ledger;
use cc_ledger::transcript;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct CostState {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "modelUsage")]
    model_usage: HashMap<String, ModelUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelUsage {
    #[serde(rename = "inputTokens")]
    input: i64,
    #[serde(rename = "outputTokens")]
    output: i64,
    #[serde(rename = "thinkingTokens")]
    thinking: i64,
    #[serde(rename = "cacheReadInputTokens")]
    cache_read: i64,
    #[serde(rename = "cacheCreationInputTokens")]
    cache_create: i64,
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcript_oracle.jsonl")
}

/// Claude Code's own totals, summed across every model it lists.
fn claude_totals(path: &Path) -> ModelUsage {
    let text = std::fs::read_to_string(path).expect("fixture readable");
    let mut totals = ModelUsage::default();
    for line in text.lines() {
        let Ok(cs) = serde_json::from_str::<CostState>(line) else {
            continue;
        };
        if cs.kind != "cost-state" {
            continue;
        }
        // Later snapshots supersede earlier ones; the fixture has exactly one.
        totals = ModelUsage::default();
        for usage in cs.model_usage.values() {
            totals.input += usage.input;
            totals.output += usage.output;
            totals.thinking += usage.thinking;
            totals.cache_read += usage.cache_read;
            totals.cache_create += usage.cache_create;
        }
    }
    totals
}

#[test]
fn deduped_ledger_totals_match_claude_codes_own_accounting() {
    let path = fixture();
    let claude = claude_totals(&path);

    let scan = transcript::scan(&path, None).expect("scan succeeds");
    let mut ledger = Ledger::open_in_memory().expect("ledger opens");
    ledger.ingest(&scan.records).expect("ingest succeeds");

    let rows = ledger.rows(None, None).expect("rows readable");

    let input: i64 = rows.iter().map(|r| r.input).sum();
    let output: i64 = rows.iter().map(|r| r.output).sum();
    let cache_read: i64 = rows.iter().map(|r| r.cache_read).sum();
    let cache_create: i64 = rows.iter().map(|r| r.cache_create).sum();

    assert_eq!(input, claude.input, "input tokens");
    assert_eq!(output, claude.output, "output tokens");
    assert_eq!(cache_read, claude.cache_read, "cache read tokens");
    assert_eq!(cache_create, claude.cache_create, "cache creation tokens");
}

/// The failure this whole design exists to prevent. `msg_ORACLE_A` spans three
/// content-block lines; summing lines instead of requests inflates output from
/// 157 to 357.
#[test]
fn summing_raw_lines_would_overcount() {
    let path = fixture();
    let claude = claude_totals(&path);

    let scan = transcript::scan(&path, None).expect("scan succeeds");
    let naive: i64 = scan.records.iter().map(|r| r.output).sum();

    assert_eq!(scan.records.len(), 5, "five usage-bearing lines");
    assert_eq!(naive, 357, "line-wise sum");
    assert!(
        naive > claude.output,
        "naive summing must overcount: {naive} vs {}",
        claude.output
    );
}

#[test]
fn dedupe_collapses_the_three_block_lines_into_one_request() {
    let path = fixture();
    let scan = transcript::scan(&path, None).expect("scan succeeds");
    let mut ledger = Ledger::open_in_memory().expect("ledger opens");

    let inserted = ledger.ingest(&scan.records).expect("ingest succeeds");
    assert_eq!(inserted, 3, "three distinct requests from five lines");
    assert_eq!(ledger.request_count().unwrap(), 3);
}

/// Ingesting the same transcript repeatedly must not move any number.
#[test]
fn reingesting_is_idempotent() {
    let path = fixture();
    let claude = claude_totals(&path);

    let scan = transcript::scan(&path, None).expect("scan succeeds");
    let mut ledger = Ledger::open_in_memory().expect("ledger opens");

    ledger.ingest(&scan.records).expect("first ingest");
    ledger.ingest(&scan.records).expect("second ingest");
    ledger.ingest(&scan.records).expect("third ingest");

    let rows = ledger.rows(None, None).expect("rows readable");
    let output: i64 = rows.iter().map(|r| r.output).sum();

    assert_eq!(ledger.request_count().unwrap(), 3);
    assert_eq!(output, claude.output, "totals unchanged after re-ingest");
}

/// `thinking` is a subset of `output` and the ephemeral split is a subset of
/// `cache_create`. Folding either into the total would break reconciliation.
#[test]
fn subset_columns_stay_out_of_the_reconciled_totals() {
    let path = fixture();
    let claude = claude_totals(&path);
    assert_eq!(claude.thinking, 60, "fixture records thinking tokens");

    let scan = transcript::scan(&path, None).expect("scan succeeds");
    let mut ledger = Ledger::open_in_memory().expect("ledger opens");
    ledger.ingest(&scan.records).expect("ingest succeeds");

    let rows = ledger.rows(None, None).expect("rows readable");
    let grand: i64 = rows.iter().map(|r| r.total()).sum();

    assert_eq!(
        grand,
        claude.input + claude.output + claude.cache_read + claude.cache_create,
        "total counts each token exactly once"
    );
}
