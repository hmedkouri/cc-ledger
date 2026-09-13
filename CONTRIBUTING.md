# Contributing

Thanks for taking a look. This is a small, deliberately boring tool; the bar for
changes is that they keep it that way.

Participation is covered by the [Code of Conduct](CODE_OF_CONDUCT.md). If you
think you have found a security issue, follow [SECURITY.md](SECURITY.md) instead
of opening a public issue.

## Getting started

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Rust 1.87+ (2021 edition), enforced by the `msrv` job in CI. The only unusual
dependency is `rusqlite` with the `bundled` feature, so no system SQLite is
required.

## Never commit real transcript data

Fixtures under `tests/fixtures/` must be synthetic or thoroughly redacted.
Claude Code transcripts contain project paths, directory names that identify
clients, session identifiers, and API request IDs.

If you derive a fixture from a real transcript:

* replace `cwd` with `/home/user/projects/example`;
* replace `sessionId` / `session_id` / `uuid` with the placeholder UUIDs already
  used in the existing fixtures;
* replace `message.id` and `requestId` with `msg_FIXTURE…` / `req_FIXTURE…`;
* strip `message.content` down to `{"type": "..."}` stubs — the parsers never
  read the content, so there is no reason for it to be in the repository.

The same applies to `docs/formats.md`: it documents real observed shapes, but
every value in it must be redacted.

## Read `docs/formats.md` before touching a parser

That file is the source of truth for every Claude Code data shape this tool
reads, with provenance for each claim: what was observed on disk, what was read
out of the Claude Code binary, and what was inferred. It is not guesswork, and
changing a parser without reading it tends to reintroduce a bug it already
documents.

The single most important invariant:

> One API response is written to the transcript as **one JSONL line per content
> block**, and every one of those lines repeats an identical `usage` object.

The ledger is therefore keyed on `message.id`. Any change that sums lines rather
than requests will roughly double every figure. `tests/oracle.rs` exists to
catch exactly that.

## Design constraints worth knowing

* **The status line must never break Claude Code.** Every fallible step degrades
  to something printable and exits 0. No panics, no stack traces, no hangs.
* **Render before ingest.** The line is printed and flushed first; the transcript
  read happens afterwards under a wall-clock budget and is abandoned if it
  overruns.
* **Subset columns are never folded into totals.** `thinking` is part of
  `output`; `cache_1h` + `cache_5m` are part of `cache_create`. They are stored
  separately and excluded from any total.
* **Timestamps are stored UTC, bucketed local.** Use `src/bucket.rs` rather than
  dividing epoch seconds — a local day is 23 or 25 hours across a DST change.
* **No new dependencies for what a few lines of stdlib can do**, and nothing
  network-facing. Ever.

## Tests

Non-trivial logic needs a test. The suite covers unit behaviour, a cost-state
reconciliation oracle, an end-to-end run of the `statusline` binary, and timing
budgets. Timing thresholds are profile-aware, so `cargo test` in debug uses
loose bounds — check the release figures before claiming a performance win.

## Commits and pull requests

* Conventional-commit style subjects (`feat:`, `fix:`, `docs:`, `test:`,
  `chore:`), small and atomic.
* Explain *why* in the body when the change is not obvious.
* `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` should all be clean
  before you open the PR.

## Licence

By contributing you agree that your contributions are licensed under the MIT
Licence, the same terms as the project. See `LICENSE` and `NOTICE`.
