## What and why

<!-- What changes, and what problem it solves. Link an issue if there is one. -->

## Checklist

- [ ] `cargo fmt --check` is clean
- [ ] `cargo clippy --all-targets -- -D warnings` is clean
- [ ] `cargo test` passes
- [ ] No real transcript data in fixtures, docs or the PR description — paths,
      directory names, session IDs and request IDs are redacted
      (see [CONTRIBUTING.md](CONTRIBUTING.md))

## If you touched a parser

- [ ] I read the relevant section of [`docs/formats.md`](docs/formats.md)
- [ ] `docs/formats.md` is updated if the observed format changed, with provenance
      for anything newly claimed
- [ ] Requests are still counted per `message.id`, not per JSONL line — one API
      response is written as one line per content block, each repeating an
      identical `usage` object (§2.3)

## If you touched the ledger or totals

- [ ] `thinking`, `cache_1h` and `cache_5m` are still excluded from totals — they
      are subsets of `output` and `cache_create`
- [ ] `tests/oracle.rs` still reconciles

## If you touched the status line

- [ ] It still prints and flushes before ingesting
- [ ] It still exits 0 and prints something usable on a malformed or empty payload
- [ ] No blocking work was added to the render path

## Notes for the reviewer

<!-- Anything surprising, deliberately left out, or worth a second opinion. -->
