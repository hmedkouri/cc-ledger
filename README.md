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

```sh
make install          # builds, installs to ~/.claude/bin, prints the settings diff
make statusline-apply # only this touches settings.json, and keeps a backup
cc-usage backfill     # read every transcript currently on disk
```

`make install` never edits `settings.json`. It prints the exact diff and stops;
`statusline-apply` writes it, preserving every other key and keeping a timestamped
backup. macOS code-signing is applied automatically and skipped elsewhere.

## Set `cleanupPeriodDays` high before you start

The ledger only records what it has seen. Anything Claude Code has already
pruned is gone for good — no file on disk carries per-request token counts for
it, so there is nothing left to recover from.

This matters more than it sounds. On the machine this was developed against, the
surviving transcripts reached back 11 days, while Claude Code's own activity
cache showed usage stretching back nine months. Everything in between had
already been deleted.

Raise the retention so the raw transcripts survive as a second, independent
record:

```jsonc
// ~/.claude/settings.json
{
  "cleanupPeriodDays": 3650
}
```

Transcripts are plain JSONL and cost little to keep — tens of megabytes for
months of work. Set this before running the backfill, since the backfill can
only import what has not already been pruned.

## Usage

```sh
cc-usage summary --since 2026-06-01
cc-usage daily   --since 2026-09-01 --by model
cc-usage weekly  --by project
cc-usage monthly
cc-usage sessions --project ~/projects/example
cc-usage export --format csv --since 2026-09-01 > usage.csv
cc-usage backfill --root ~/.claude/projects
```

Dates are `YYYY-MM-DD` in local time. `--until` is exclusive. Columns are
input / output / cache-create / cache-read / total.

Timestamps are stored as UTC and bucketed in **local** time, so a day is 23 or
25 hours long across a DST transition rather than a flat 86 400 seconds.

### The status line

```
~/p/T/cc-ledger  master(+2405 -6) • ██████░░░░ 63% • Opus 5 (high) • 5.5M session • today 8.6M / week 383M • 5h 31% 7d 59%
```

`session` counts the current `sessionId` only. Resuming a session continues that
count — a resumed session can span days — while starting a fresh `claude`
resets it. `today` and `week` sum every session across every project.

`statusline --short` renders a compact variant: directory, context percent and
today's tokens.

Rendering never blocks on ingest. The line is printed and flushed first; the
transcript read then runs under a 150 ms wall-clock budget and is abandoned if
it overruns, because the next invocation resumes from a stored byte cursor.

## Why there is no cost estimate

There is deliberately no `pricing.toml`. Transcripts record the model as
`claude-opus-5` whether or not the account is running the 1M-context variant,
which is priced very differently — and no per-request field distinguishes them.
Any price table keyed on the recorded model silently under-reports.

Instead, the status line surfaces `rate_limits.five_hour` and `seven_day`
directly from Claude Code's payload, and every observed change is written to a
`limits` table. On a subscription that is the signal that matters, and the
ledger is the only place that history exists — the server keeps none, and the
percentages vanish from the payload the moment the window rolls over.

## Where things live

| What | Where |
| --- | --- |
| Ledger database | `$XDG_DATA_HOME/cc-ledger/ledger.db`, else `~/.local/share/cc-ledger/ledger.db` |
| Override | `CC_LEDGER_DB` |
| Transcripts read | `~/.claude/projects/**/*.jsonl` |

The database runs in WAL mode with a 250 ms busy timeout, because several
Claude Code sessions write to it concurrently. Every insert is
`INSERT OR IGNORE` on a stable key, so a repeated, concurrent or abandoned pass
cannot double-count.

## The one thing worth knowing about the data

A single API response is written to the transcript as **one JSONL line per
content block**, and every one of those lines repeats the identical, complete
`usage` object. Summing lines roughly doubles every number — 4 942 lines for
2 481 real requests on the development machine, with one response spanning 25
lines. The ledger is keyed on `message.id`, which is non-null on every usage
record and unique across every transcript on disk.

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
