//! A persistent, per-request token ledger for Claude Code.
//!
//! Claude Code prunes its own transcripts (`cleanupPeriodDays`, 30 by default),
//! so token history evaporates. This crate reads the transcripts incrementally
//! and keeps every request in a local SQLite database that outlives them.
//!
//! The data formats this crate parses are documented, with provenance, in
//! `docs/formats.md`. Read that before changing a parser.

pub mod bucket;
pub mod ledger;
pub mod payload;
pub mod render;
pub mod transcript;
