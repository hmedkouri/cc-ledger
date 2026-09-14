# Internals — timing budgets and storage

How the status line stays out of the way, and what the ledger costs on disk.
For the data shapes themselves, see [`formats.md`](formats.md).

## Performance

A cold render is budgeted under 20 ms in release on Linux, asserted in
`tests/timing.rs`. That test measures a whole process invocation, and on macOS
the spawn dominates it and varies far too widely to bound — three CI samples of
one build gave 70 ms, 77 ms and 156 ms — so the assertion is skipped there rather
than loosened into meaninglessness. The work this crate actually does is guarded
in-process instead, and the macOS runner completes those checks faster than the
Linux one.

The status line itself spawns no subprocess of any kind: the branch comes from
reading `.git/HEAD` directly, and there is no network-facing dependency in the
crate. For scale, the reference implementation this borrows its styling from
spawns `git` twice on every single render — once to test whether the directory is
a repository, then again to read the branch.

Ingest runs only after the line has been printed and flushed. It is capped at a
150 ms wall-clock budget and commits in batches of 500 lines, each batch writing
its records and its cursor in one transaction, so a backlog is worked off
incrementally across renders rather than stalling on one oversized pass.

The one visible cost is that renders stay busy until an un-backfilled session
has been caught up. Running `cc-usage backfill` once after install avoids it
entirely.

Timing budgets only mean anything in `--release`; in debug they relax to loose
bounds.

## Where things live

| What | Where |
| --- | --- |
| Ledger database | `$XDG_DATA_HOME/cc-ledger/ledger.db`, else `~/.local/share/cc-ledger/ledger.db` |
| Override | `CC_LEDGER_DB` |
| Transcripts read | `~/.claude/projects/**/*.jsonl` |

The database runs in WAL mode with a 250 ms busy timeout, because several
Claude Code sessions write to it concurrently. Every write upserts on a stable
key, merging each token field with `max()`, so a repeated, concurrent or
abandoned pass cannot double-count — and a partially written first record is
corrected rather than kept, because usage only ever grows within a response.

A request costs about 356 bytes once every index is counted. At the development
machine's rate of roughly 250 requests a day that is around 92 000 rows and
31 MB a year, which is small enough to forget about. Transcript paths are the
reason it is not considerably larger: they repeat on every row and averaged 108
bytes, so they live in a `transcripts` table and each request stores an integer
instead. The saving scales with rows per transcript — at ~150 rows per transcript
the development database shrank 27%; many short sessions save less. If you
do want the space back, `cc-usage prune --before 2026-01-01` prints what it
would remove and deletes nothing until you add `--yes`, then compacts the file.
