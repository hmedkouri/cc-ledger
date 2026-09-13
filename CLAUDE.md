# cc-ledger — notes for Claude Code

Read `docs/formats.md` before changing anything that parses a transcript or a
status-line payload. It is the source of truth for every Claude Code data shape
this tool reads, and each claim carries a provenance marker: observed on disk,
read out of the `claude` binary, or inferred. Changing a parser without it tends
to reintroduce a bug the document already records.

Six invariants, stated in full under "Design constraints" in `CONTRIBUTING.md`:

1. The status line never breaks Claude Code — no panic, no hang, always exit 0.
   New code on the render path goes inside the existing `catch_unwind`.
2. Render before ingest. The line is printed and flushed first; the transcript
   read follows under a wall-clock budget and is abandoned if it overruns.
3. One API response is written as many JSONL lines, each repeating the same
   `usage` object. Key on `message.id`; summing lines double-counts every
   response.
4. Subset columns are never folded into totals — `thinking` is part of `output`,
   `cache_1h` + `cache_5m` are part of `cache_create`.
5. Timestamps are stored UTC and bucketed local, via `src/bucket.rs` rather than
   by dividing epoch seconds.
6. No new dependency for what a few lines of stdlib can do, and nothing
   network-facing, ever.

Never put real transcript data in fixtures, docs or commit messages: project
paths and directory names identify clients. `CONTRIBUTING.md` lists what to
strip and which placeholders to use.

Gates before any commit, checked by exit code rather than by eye:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Timing budgets only mean anything in `--release`; in debug they relax to loose
bounds. Never run a schema migration against the live ledger — work on a
`sqlite3 .backup` copy.
