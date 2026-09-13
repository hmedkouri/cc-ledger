# cc-ledger

A Claude Code status line backed by a persistent, per-request token ledger.

Claude Code deletes its own transcripts on a schedule (`cleanupPeriodDays`, 30
days by default), and the token counts go with them. `cc-ledger` reads those
transcripts incrementally and keeps every request in a local SQLite database
that outlives them, so you get a complete consumption history going forward.

Two binaries:

* **`statusline`** — renders the status line, and as a side effect ingests new
  usage records from the current session's transcript.
* **`cc-usage`** — queries the ledger: totals, per-day/week/month breakdowns,
  grouped by model or project.

## Install

**Step 0: raise `cleanupPeriodDays` first.** The backfill can only import
transcripts that have not already been pruned, and those raw transcripts stay
your only independent record if the parser is ever wrong. On the development
machine the surviving transcripts reached back 11 days while Claude Code's own
activity cache showed usage stretching back nine months.

```jsonc
// ~/.claude/settings.json
{
  "cleanupPeriodDays": 3650
}
```

Then build and install. Needs a Rust toolchain (1.87+); SQLite is bundled, so
there is nothing else to install.

```sh
git clone https://github.com/hmedkouri/cc-ledger.git
cd cc-ledger
make install          # builds, installs to ~/.claude/bin, prints the settings diff
make statusline-apply # only this touches settings.json, and keeps a backup
cc-usage backfill     # read every transcript currently on disk
```

Both binaries are installed to `~/.claude/bin`. `statusline` is never typed —
Claude Code runs it via the absolute path written into `settings.json` — so only
`cc-usage` needs to be on your `PATH`, and `make install` symlinks it into
`~/.local/bin`, warning if that directory is not on your `PATH`. Override either
location with `make install BIN_DIR=... LINK_DIR=...`.

`make install` never edits `settings.json`. It prints the exact diff and stops;
`statusline-apply` writes it, preserving every other key and keeping a timestamped
backup. macOS code-signing is applied automatically and skipped elsewhere.

`make uninstall` reverses all of it: both binaries and the symlink are removed,
and the `statusLine` key is restored from the most recent backup — only that key,
so anything else you changed since survives. Your ledger database is left alone.

If you only want the `cc-usage` query tool and would rather skip the Makefile:

```sh
cargo install --git https://github.com/hmedkouri/cc-ledger --tag v0.1.0 --bin cc-usage
```

## The status line

```
~/p/T/cc-ledger main(+412 -37) • ████████░░ 80% • 5h 50% (2h10m) 7d 38% (3d) • Opus 5 · 1M · high • today 91M
```

Directory and branch, then context, then rate limits, then model, then today.
Context and limits sit together because they are the two signals that answer
"should I stop soon"; today is last because a narrow pane clips from the right
and that is the segment worth losing.

Each rate limit shows time remaining until it resets — `2h10m` below a day,
`3d` at or above one, and nothing at all when the payload carries no reset time
or the moment has already passed.

`today` sums every session across every project. Per-session and per-week totals
are not on the line; `cc-usage sessions` and `cc-usage weekly` have them.

`statusline --short` renders a compact variant: directory, context percent and
today's tokens, dropping the bar but keeping the number. `statusline --cost`
renders today as API-equivalent dollars instead of tokens.

Rendering never blocks on ingest. The line is printed and flushed first; the
transcript read then runs under a 150 ms wall-clock budget and is abandoned if
it overruns, because the next invocation resumes from a stored byte cursor.

## Usage

```sh
cc-usage summary --since 2026-06-01
cc-usage summary --cost
cc-usage daily   --since 2026-09-01 --by model
cc-usage weekly  --by project
cc-usage monthly --cost --by model
cc-usage sessions --project ~/projects/example
cc-usage export --format csv --since 2026-09-01 > usage.csv
cc-usage backfill --root ~/.claude/projects
cc-usage prune --before 2026-01-01        # reports only; --yes to apply
```

Dates are `YYYY-MM-DD` in local time. `--until` is exclusive. Columns are
input / output / cache-create / cache-read / total.

Every command takes `--help` for its full set of options:

```sh
cc-usage --help        # the available commands
cc-usage daily --help  # the options for one command
cc-usage --version
```

Timestamps are stored as UTC and bucketed in **local** time, so a day is 23 or
25 hours long across a DST transition rather than a flat 86 400 seconds.

Bucketing uses the timezone in force when you *query*, not when the request was
made. Totals are therefore stable for anyone who stays in one zone, but the same
ledger queried from a different `TZ` will move requests near midnight into
adjacent days. Only the bucket boundaries shift; the stored timestamps never do.

## API Cost

Pass `--cost` to `summary`, `daily`, `weekly` or `monthly` to price recorded
usage at Anthropic's published API list rates and report it as **API Cost**.

That is what the usage *would have cost* on the API. A subscription's cost is
its fee; this number is the one that says whether the subscription is earning
its keep, and which models and projects consume the value.

Four token classes are priced separately — input, output, cache write and cache
read — and cache writes differ again by TTL. In the 11-day development sample,
cache reads were 52% of the bill, cache writes 34%, output 14%, uncached input
0.02%. That mix is characteristic of long agentic sessions holding a large stable
context; short chat turns cache far less. What holds regardless of workload is
that the four classes carry different rates, and a report that prices only input
and output can miss most of the bill — 86% of it in this sample.

Long context does not change the rate — Claude 4.6 and later bill the full 1M
context window at standard pricing — so the model id recorded on each request is
enough to price it.

Rates live in `src/pricing.rs`, verified against the published pricing page on
2026-09-13. An unknown model id is reported as *unpriced* rather than counted as
free, so a model introduced by a future Claude Code release cannot silently
shrink the total.

**What the estimate assumes.** Standard speed — fast mode doubles Opus rates;
globally routed inference — pinning to `us` adds 10% to every token class; and
no server-tool charges, such as web search at $10 per 1 000 requests. The
`speed` and `inference_geo` fields *are* recorded per request, so the figures
can be corrected later, but none of the three is priced today. No traffic in the
development ledger used any of them.

**How complete the underlying counts are.** `summary` prints a coverage line:
the share of Claude Code's own token count that the per-request rows account
for. It sits slightly below 100% by design, because Claude Code bills requests
that never produce a transcript record — retries, aborted turns, and auxiliary
calls to models that appear nowhere in the transcript. On the development ledger
that share is 90.3%.

Separately, the status line records `rate_limits.five_hour` and `seven_day` from
Claude Code's payload into a `limits` table. On a subscription that is the live
budget signal, and the ledger is the only place that history exists — the server
keeps none, and the percentages vanish once a window rolls over.

## Accuracy

These numbers are a local reconstruction from Claude Code's transcripts, not a
read of Anthropic's billing. Requests that never produced an `assistant` record
— retries, aborted turns, auxiliary generations, server-side tool calls — are
billed but absent from the transcript, so the ledger runs under Claude Code's own
`cost-state` accounting: across the 16 sessions carrying a snapshot it covers
90.3% of the tokens Claude Code counted. That accounting is no gold standard
either, having been observed dropping an entire model from a session's totals.

Only transcripts still on disk can be read. Whatever `cleanupPeriodDays` removed
before your first backfill is unrecoverable — on the development machine that
left 11 days and roughly 2 481 requests. Raise the retention first; see Install
above.

`--cost` estimates API list price assuming standard speed, globally routed
inference, no server-side tool charges, and the rates dated in `src/pricing.rs`.
It says nothing about what a subscription costs.

The transcript and payload formats are undocumented and shift between Claude Code
releases, so a new release can break parsing silently until fixtures catch up —
the `format_change` issue template exists for reporting that.

Good for trends, budgeting, and comparing days and projects; not for disputing a
bill or auditing against an invoice.

## Performance

A cold render is budgeted under 20 ms in release, asserted in `tests/timing.rs`.
It spawns no subprocess of any kind: the branch comes from reading `.git/HEAD`
directly, and there is no network-facing dependency in the crate. For scale, the
reference implementation this borrows its styling from spawns `git` twice on
every single render — once to test whether the directory is a repository, then
again to read the branch.

Ingest runs only after the line has been printed and flushed. It is capped at a
150 ms wall-clock budget and commits in batches of 500 lines, each batch writing
its records and its cursor in one transaction, so a backlog is worked off
incrementally across renders rather than stalling on one oversized pass.

The one visible cost is that renders stay busy until an un-backfilled session
has been caught up. Running `cc-usage backfill` once after install avoids it
entirely.

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

## The one thing worth knowing about the data

A single API response is written to the transcript as **one JSONL line per
content block**, and every one of those lines repeats the identical, complete
`usage` object. Summing lines therefore double-counts every response, by a factor
equal to the number of content blocks it contains — several for a tool-heavy
agentic turn, one for a plain prose answer — so the size of the error depends on
how you work. On the development machine that factor was two: 4 942 lines for
2 481 real requests, with one response spanning 25 lines. The ledger is keyed on
`message.id`, which is non-null on every usage record and unique across every
transcript on disk.

`docs/formats.md` documents every field, with provenance and sample redacted
records. Read it before changing a parser.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). One rule worth stating up front: fixtures
and docs must never contain real transcript data. Claude Code transcripts carry
project paths and directory names that identify clients, along with session and
request identifiers.

Participation is covered by the [Code of Conduct](CODE_OF_CONDUCT.md). Security
reports go through [SECURITY.md](SECURITY.md) — privately, please, and redact any
transcript excerpt you attach.

## Licence

MIT. Presentation code in `src/render.rs` is adapted from
[khoi/cc-statusline-rs](https://github.com/khoi/cc-statusline-rs) (MIT) — see
`NOTICE`.
