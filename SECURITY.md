# Security Policy

## Scope

`cc-ledger` is a local tool. It has no network-facing code and no runtime
dependency that makes network calls — this is a deliberate design constraint,
not an accident. It reads Claude Code transcripts from disk and writes a SQLite
database under your data directory.

That said, it touches data worth protecting:

* **Claude Code transcripts** contain project paths and directory names that can
  identify clients and unreleased work.
* **The ledger database** records per-request token counts, project directories
  and session identifiers.
* **The status line** runs on every refresh inside your shell session.

## Reporting a vulnerability

Please report privately rather than opening a public issue: use GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository.

Include what you did, what happened, and what you expected. A proof of concept
helps. Expect an initial response within a couple of weeks — this is a small
personal project, not a staffed one, so please set expectations accordingly.

## Things that are explicitly in scope

* Path traversal or injection via transcript contents, which are untrusted input
  as far as the parsers are concerned.
* Anything that causes the status line to hang, crash or block Claude Code.
* SQL injection through project paths, model names or session identifiers.
* Writes outside the configured database location, including via `CC_LEDGER_DB`.

## Things that are not

* Reading transcripts you already have read access to.
* The ledger being readable by your own user account.
* Token or cost figures disagreeing with Claude Code's own accounting —
  see `docs/formats.md` §2.8, which documents why that happens and why the
  transcript is an incomplete record of billed usage.

## A note for contributors

If you attach a transcript excerpt to a report, redact it first. See
[CONTRIBUTING.md](CONTRIBUTING.md) for what to strip.
