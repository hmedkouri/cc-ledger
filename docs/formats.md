# Claude Code data formats — findings

Source of truth for the parsers in this crate. Everything below was read off this
machine on **2026-09-13** against **Claude Code 2.1.270**, not from documentation.

Provenance markers used throughout:

- **[verified]** — observed in real data on disk, counted.
- **[binary]** — extracted from the payload constructor inside the `claude`
  executable (`~/.local/share/claude/versions/2.1.270`, function `ERs`). Exact,
  but the shape is read from minified JS, so field *presence* is certain and
  field *types* are inferred from use.
- **[inferred]** — reasoned from the above, not directly observed.

---

## 1. Environment

| Item | Value |
| --- | --- |
| Claude Code version | 2.1.270 |
| rustc / cargo | 1.96.1 |
| `statusLine` (current) | `{"type":"command","command":"~/.claude/cc-statusline-rs"}` |
| `cleanupPeriodDays` | **not set** → default 30 days |
| Transcript root | `~/.claude/projects/` |
| Transcript count / size | 19 files, 53 MB |
| Largest transcript | 22.2 MB (a single long-running session) |
| `~/.claude/bin` | does not exist |
| `XDG_DATA_HOME` | unset → ledger goes to `~/.local/share/cc-ledger/` |
| Last log cleanup | `2026-09-13T11:49:15Z` (`~/.claude/.last-cleanup`) — pruning is live |

### 1.1 Pruning has already destroyed most of the history [verified]

Oldest surviving transcript record: `2026-09-02T18:34:10Z`.
Newest: `2026-09-13T11:54:50Z`. **That is 11 days of data, total.**

Meanwhile `~/.claude/stats-cache.json` still lists daily activity from
`2025-12-29` to `2026-02-15` — message/tool counts only, **no token counts**.

Consequence for the stated goal: a backfill can recover roughly 2 481 requests
covering 2026-09-02 onward. Consumption before that date is **unrecoverable** —
no file on this machine carries per-request token counts for it. The ledger is a
forward-looking instrument; "since my subscription started" can only mean "since
today" unless an external source is imported.

---

## 2. Transcript files — `~/.claude/projects/<slug>/<session-id>.jsonl`

- Directory slug = the project cwd with `/` → `-` (e.g.
  `/home/user/projects/Example` → `-home-user-projects-Example`).
  Lossy: a real `-` in a path is indistinguishable from a separator. **Use the
  `cwd` field inside the records, never the directory name.** [verified]
- Filename stem = `sessionId`; every record in a file carries that same
  `sessionId`. [verified, 5 files sampled]
- All 19 files end with a trailing newline (`0x0a`). The last line is still
  assumed to be possibly partial — a live session appends mid-read. [verified]
- **No `subagents/` subdirectories exist anywhere under the transcript root.** [verified]

### 2.1 Record types present [verified]

| count | `type` |
| --- | --- |
| 4942 | `assistant` |
| 2840 | `user` |
| 1671 | `attachment` |
| 717 | `last-prompt` |
| 703 | `permission-mode`, `mode`, `atis-latch` (each) |
| 685 | `ai-title` |
| 462 | `system` |
| 356 | `file-history-snapshot` |
| 151 | `bridge-session` |
| 58 | `file-history-delta` |
| 54 | `queue-operation` |
| 36 | `cost-state` |

Only `type == "assistant"` records carry `message.usage`. All 4942 of them do.

### 2.2 Assistant record — redacted sample [verified]

Content blocks elided; every other field is verbatim shape.

```json
{
  "type": "assistant",
  "uuid": "00000000-0000-4000-8000-000000000001",
  "parentUuid": "00000000-0000-4000-8000-000000000000",
  "sessionId": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
  "session_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
  "requestId": "req_XXXXXXXXXXXXXXXXXXXXXXXX",
  "timestamp": "2026-09-05T13:56:32.140Z",
  "cwd": "/home/user/projects/example",
  "gitBranch": "HEAD",
  "version": "2.1.261",
  "isSidechain": false,
  "userType": "external",
  "entrypoint": "cli",
  "apiBlockIndex": 0,
  "effort": "high",
  "slug": null,
  "message": {
    "id": "msg_XXXXXXXXXXXXXXXXXXXXXXXX",
    "model": "claude-opus-5",
    "role": "assistant",
    "stop_reason": "tool_use",
    "content": [ { "type": "thinking" } ],
    "usage": { "…see 2.4…" }
  }
}
```

Field locations confirmed against the spec's assumptions:

| Wanted | Actual location | Notes |
| --- | --- | --- |
| model | `message.model` | ✔ |
| input tokens | `message.usage.input_tokens` | ✔ |
| output tokens | `message.usage.output_tokens` | ✔ |
| cache create | `message.usage.cache_creation_input_tokens` | ✔ |
| cache read | `message.usage.cache_read_input_tokens` | ✔ |
| message id | `message.id` | ✔ |
| request id | `requestId` (top level) | **null on 604 / 4942 lines** |
| line id | `uuid` (top level) | unique per line |
| timestamp | `timestamp` (top level) | RFC3339, always `Z` / UTC |
| session | `sessionId` **and** `session_id` | always identical [verified] |
| cwd | `cwd` (top level) | ✔ |

Also present and useful: `gitBranch`, `version`, `effort`, `isSidechain`,
`apiBlockIndex`. Optional/sometimes-absent top-level keys seen across records:
`slug`, `perTurnEffort`, `wireIngestContext`, `wireToolInputs`, `promptId`,
`sourceToolAssistantUUID`, `toolUseResult`, `rendered`, `attachment`, `origin`,
`promptSource`, `permissionMode`, `isMeta`, `messageCount`, `durationMs`.

**Every struct field must be `Option<T>`; never `deny_unknown_fields`.** The key
sets vary between records of the same type across versions — 20+ distinct
top-level key combinations were observed.

### 2.3 One API response spans many JSONL lines — the dedupe question [verified]

This is the single most important finding.

A single API response is written as **one line per content block**, distinguished
by `apiBlockIndex` (0, 1, 2, …). Every one of those lines repeats the
**identical, complete `usage` object** — it is not split or incremental.

Example — one response (identifiers redacted) produced 11 lines, 0.7 s apart,
every one carrying an identical usage object:

```
uuid=<line-1>  req=req_REDACTED  ts=…:23.444Z  in=101 out=968  …
uuid=<line-2>  req=req_REDACTED  ts=…:24.302Z  in=101 out=968  …
uuid=<line-3>  req=req_REDACTED  ts=…:24.941Z  in=101 out=968  …
… 8 more, same usage tuple …
```

Counts across all transcripts:

| quantity | count |
| --- | --- |
| lines with `message.usage` | 4942 |
| distinct `message.id` | 2476 |
| distinct `requestId` (non-null) | 2177 |
| distinct `uuid` | 4942 |
| distinct `(message.id, requestId)` pairs | 2476 |
| distinct full usage tuples `(id, in, out, cc, cr)` | **2476** |

The last row is the proof: distinct usage tuples == distinct `message.id`. Every
repeat is a byte-identical duplicate of the same billing event.

**Summing every line would roughly double every number in the ledger** (4942 vs
2476). Highest observed repeat factor for a single response: **25 lines**.

#### Dedupe key decision: `message.id`

- `uuid` — **rejected**, unique per line; dedupes nothing.
- `requestId` — **rejected as sole key**, null on 604/4942 lines (12%).
- `message.id` — **chosen**. Non-null on every usage line, and globally unique:
  counting `(message.id, file)` pairs across all 19 transcripts gives 2481 pairs
  for 2481 distinct ids — **no `message.id` is ever seen in two files or two
  sessions**, so session fork/resume does not duplicate billing events, and a
  single-column primary key is safe. [verified]

> (2476 vs 2481 is an artifact of my own analysis: the 2476 figures come from a
> `cat`-concatenated scratch file, the 2481 from per-file iteration. 2481 is the
> correct figure.)

### 2.4 `message.usage` — full shape [verified]

```json
{
  "input_tokens": 2,
  "cache_creation_input_tokens": 40446,
  "cache_read_input_tokens": 0,
  "output_tokens": 589,
  "output_tokens_details": { "thinking_tokens": 226 },
  "server_tool_use": { "web_search_requests": 0, "web_fetch_requests": 0 },
  "service_tier": "standard",
  "cache_creation": {
    "ephemeral_1h_input_tokens": 40446,
    "ephemeral_5m_input_tokens": 0
  },
  "inference_geo": "not_available",
  "iterations": [
    { "type": "message", "input_tokens": 2, "output_tokens": 589,
      "cache_read_input_tokens": 0, "cache_creation_input_tokens": 40446,
      "cache_creation": { "ephemeral_5m_input_tokens": 0,
                          "ephemeral_1h_input_tokens": 40446 } }
  ],
  "speed": "standard"
}
```

All 4942 usage objects have exactly this key set. Notes:

- `cache_creation` splits cache writes into **1-hour vs 5-minute TTL**, which are
  priced differently (1h ≈ 2× base, 5m ≈ 1.25× base). The flat
  `cache_creation_input_tokens` is their sum. Worth storing both if cost is ever
  computed seriously.
- `output_tokens_details.thinking_tokens` is a **subset of** `output_tokens`, not
  an addition — do not add it to the total.
- `iterations[]` repeats the same numbers for a single-iteration response; it is
  nullable (`null` on synthetic records).

### 2.5 Models seen [verified]

| count | `message.model` |
| --- | --- |
| 4694 | `claude-opus-5` |
| 229 | `claude-fable-5-1` |
| 14 | `claude-sonnet-5` |
| 4 | `claude-fable-5` |
| 1 | `<synthetic>` |

`<synthetic>` records have `requestId: null` and an all-zero usage object — they
are local placeholders, not API calls. **Skip records where `model` starts with
`<`** (they contribute nothing but would pollute the model breakdown).

### 2.6 The `[1m]` context-variant problem [verified]

`cost-state` records name models as:

```
claude-opus-5, claude-opus-5[1m], claude-fable-5[1m],
claude-fable-5-1[1m], claude-haiku-4-5-20251001
```

But `message.model` on assistant records **never carries the `[1m]` suffix** —
only the bare id. The 1M-context variants are priced substantially higher than
the standard-context ones.

This machine is configured `"model": "opus[1m]"`, so effectively all traffic is
the expensive variant while the transcript records it as plain `claude-opus-5`.

**Any pricing table keyed on `message.model` alone will systematically
under-estimate cost, and there is no per-request field that disambiguates.**
See §5.

### 2.7 Subagents / sidechains [verified]

- `isSidechain` is `false` on **all** 4942 usage records.
- **Zero `Task` tool calls** across every transcript (tool-use histogram: Bash
  2157, Read 136, Write 50, Edit 41, ToolSearch 10, WebFetch 9, WebSearch 5,
  Monitor 2, AskUserQuestion 2, Skill 1 — no `Task`).
- No `subagents/` directory exists anywhere under the transcript root.

So subagent token accounting **cannot be tested against real data on this
machine**. `sourceToolAssistantUUID` (2413 records) is unrelated — it links tool
results to the assistant message that requested them, and appears on `user`-type
records.

### 2.8 `cost-state` records — useful cross-check [verified]

```json
{
  "type": "cost-state",
  "sessionId": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
  "totalCostUSD": 12.3456789,
  "totalAPIDuration": 3902912,
  "totalToolDuration": 618131,
  "totalLinesAdded": 188,
  "totalLinesRemoved": 10,
  "totalDuration": 16650889,
  "startTime": 1788000000000,
  "modelUsage": {
    "claude-opus-5[1m]": {
      "inputTokens": 17426, "outputTokens": 238122, "thinkingTokens": 106231,
      "cacheReadInputTokens": 47312579, "cacheCreationInputTokens": 1556117,
      "webSearchRequests": 1, "costUSD": 12.3456789
    }
  },
  "hasUnknownModelCost": false
}
```

These are **cumulative session snapshots**, re-emitted periodically — the last
one per `sessionId` is the most current. They are Claude Code's own accounting,
they carry the `[1m]` variant name, and they include a real `costUSD`.

**They are not a reliable equality oracle.** Reconciling our deduped per-request
sums against the final `cost-state` across all 12 sessions that have one:

| behaviour | sessions |
| --- | --- |
| ours slightly **under** cost-state, `thinkingTokens` matching exactly | 10 |
| ours slightly under on both output and thinking | 1 |
| ours **4.7× over** cost-state | 1 |

Two distinct effects, both confirmed:

1. **The transcript is an incomplete record of billed usage.** No non-`assistant`
   record carries `usage` anywhere on disk, and no top-level `usage` key exists.
   Requests that never produced an `assistant` record — retries, aborted turns,
   auxiliary generations — are billed and counted by `cost-state` but are absent
   from the JSONL entirely. This is why our sums run a few percent under, while
   `thinkingTokens` (produced only by recorded assistant turns) matches exactly.
2. **`cost-state` can silently drop an entire model.** The one inverted session
   (project and session redacted) used two models —
   `claude-fable-5-1` (23 requests, 24 521 output) and `claude-opus-5` (20
   requests, 12 168 output) — but its `modelUsage` names only
   `claude-fable-5-1[1m]`, at 7 736 output, with `hasUnknownModelCost: false`.
   Opus is missing outright, and the model it *does* list is under-counted
   roughly threefold.

   This is **not** a stalled snapshot: every transcript on disk carries exactly
   two `cost-state` records resolving to exactly one distinct total, so two
   identical records is the normal steady state, not evidence of an early
   freeze. It is Claude Code's own accounting losing a model, most likely across
   a mid-session model switch.

   The detectable precondition: the oracle is trustworthy only when the
   `modelUsage` key set (with `[1m]` stripped) covers every model the transcript
   actually used. Where it does, the numbers reconcile — including the sessions
   that used two and three models respectively.

Conclusion for tests: assert the oracle against a **fixture** with a known-good
`cost-state`, not against live transcripts. On live data the only sound
invariants are `deduped_sum <= naive_line_sum` and, where `modelUsage` covers
the transcript's models, `thinking_ours == thinkingTokens`.

Worth noting for a future feature: because effect (1) means `cost-state` sees
tokens the per-request rows never will, those records are the *only* trace of
that consumption. Ingesting them as a per-session reconciliation row would
capture it — at the cost of no time resolution. Not built.

---

## 3. Status line payload

Extracted from the constructor in the `claude` binary [binary]. Presented as a
schema rather than a capture; a live capture should confirm it before release.

### 3.1 Base fields, common to all hook payloads

```
hook_event_name, session_id, transcript_path, cwd, scratchpad_dir,
prompt_id, permission_mode, agent_id, agent_type, served_call,
caller_session_id, effort
```

`transcript_path` is the field the ingest path depends on. `session_id` and `cwd`
are also present at top level.

### 3.2 Status-line specific fields

```jsonc
{
  "session_id": "…", "transcript_path": "/home/…/projects/<slug>/<id>.jsonl",
  "cwd": "/home/…", "hook_event_name": "Status",

  "session_name": "…",                    // optional
  "model":     { "id": "claude-opus-5", "display_name": "Opus 5" },
  "workspace": { "current_dir": "…", "project_dir": "…", "added_dirs": [],
                 "git_worktree": {…},     // optional
                 "repo": {…} },           // optional
  "version": "2.1.270",
  "output_style": { "name": "default" },

  "cost": { "total_cost_usd": 45.2, "total_duration_ms": 0,
            "total_api_duration_ms": 0,
            "total_lines_added": 0, "total_lines_removed": 0 },

  "context_window": {
      "total_input_tokens": 0,    // input + cache_creation + cache_read
      "total_output_tokens": 0,
      "context_window_size": 200000,
      "current_usage": { …a usage object as in §2.4… },
      "used_percentage": 0.0,
      "remaining_percentage": 100.0
  },
  "exceeds_200k_tokens": false,

  "prompt_cache": {                       // optional
      "warm": true, "caching_observed": true, "ttl": "1h",
      "expires_at": 1788616252, "requests": 0, "misses": 0,
      "expected_rebuilds": 0, "hit_ratio": 0.0,
      "cache_write_tokens": 0, "miss_recache_tokens": 0,
      "last_miss_at": null,
      "last_miss_cause": { "causes": [], "tools_added": 0,
                           "tools_removed": 0, "system_char_delta": 0 },
      "miss_causes": [], "recache_tokens_if_cold": 0
  },

  "fast_mode": false,
  "effort":   { "level": "high" },        // optional, effort-capable models only
  "thinking": { "enabled": true },

  "rate_limits": {                        // optional — see §5
      "five_hour":   { "used_percentage": 12.5, "resets_at": 1788616252 },
      "seven_day":   { "used_percentage": 44.0, "resets_at": 1789616252 },
      "spend_limit": { "used_percentage": 0.0,  "resets_at": 1789616252 }
  },

  "vim":      { "mode": "INSERT" },       // optional
  "agent":    { "name": "…" },            // optional
  "remote":   { "session_id": "…" },      // optional
  "pr":       { "number": 1, "url": "…", "review_state": "…", "kind": "…" },
  "worktree": { "name": "…", "path": "…", "branch": "…",
                "original_cwd": "…", "original_branch": "…" }
}
```

Note `used_percentage` in `context_window` is **already computed** — the status
line does not need to derive it (khoi's implementation recomputes it by hand from
`current_usage`, which is redundant but harmless).

`resets_at` values are **Unix seconds** (the binary multiplies them by 1000).

### 3.3 Invocation behaviour [binary]

The status line is **not** invoked once per message. It re-runs when any of
`tokenUsage, permissionMode, vimMode, mainLoopModel, fastMode, effortValue,
thinkingEnabled, prStatus` change, when the last assistant message id changes, on
a `refreshInterval` timer if configured, and it is scheduled to re-fire when a
rate-limit or prompt-cache window is due to reset.

**Implication:** invocations are frequent and bursty, several per assistant turn,
and concurrent across sessions. The ingest path must be cheap and must tolerate
another process holding the write lock. `INSERT OR IGNORE` on a stable dedupe key
makes a lost or repeated pass harmless.

---

## 4. Reference implementation (khoi/cc-statusline-rs, MIT)

Cloned to scratch. 369 lines in `src/lib.rs`, 9 in `src/main.rs`.

Worth lifting: `fish_shorten_path`, `truncate_middle`, `format_tokens`,
`format_cost`, the colour thresholds and the bar.

Context-percentage thresholds:

| threshold | colour |
| --- | --- |
| ≥ 90 % | red `\x1b[31m` |
| ≥ 70 % | orange `\x1b[38;5;208m` |
| ≥ 50 % | yellow `\x1b[33m` |
| else | grey `\x1b[90m` |

Bar: width 10, `█` filled / `░` empty. Separator: `\x1b[90m • \x1b[0m`.
Icons: model `\u{e26d}`, context `\u{f49b}`, branch `\u{f02a2}`, cost `\u{f155}`.

Confirmed problems to not inherit: `reqwest` is declared in `Cargo.toml` and
never used; the render path is untyped `serde_json::Value` lookups throughout;
`get_session_duration` reads the **entire** transcript into a `String` (22 MB on
this machine) and then contains a bug — it `break`s out of the first loop after
the first record regardless, and the `?` on `parse_timestamp` silently abandons
the whole function on one unparseable timestamp; `get_git_branch` shells out to
`git` on every single render.

`format_tokens` has a rendering wart worth fixing: `>= 100.0` and the `else`
branch both round, so 1 234 → `1.2k` but 99 999 → `100k` and 100 001 → `100k`.
Acceptable; keep the API, keep the tests.

---

## 5. Consequences for the design — items needing a decision

1. **Backfill recovers 11 days, not a subscription's worth.** Per-request token
   history before 2026-09-02 does not exist on this machine in any form.
2. **Cost estimation is structurally unreliable here** because `message.model`
   drops the `[1m]` suffix while the account runs `opus[1m]`. A `pricing.toml`
   keyed on the bare model id will under-report. The `cost-state` records carry
   both the true variant and Claude Code's own `costUSD` — if cost matters, that
   is the honest source, not a hand-maintained price table.
3. **The payload already carries the subscription's real budget signal** —
   `rate_limits.five_hour.used_percentage` and `seven_day.used_percentage`, with
   reset timestamps. For a subscription user this is strictly more actionable
   than an estimated dollar figure, and it is free (already in the payload, no
   ledger query, no pricing table).
4. **Subagent support cannot be validated.** No sidechain records, no `Task`
   calls, no `subagents/` directories exist. The `isSidechain` field is one line
   to capture; the `subagents/*.jsonl` directory glob is speculative code against
   a layout never observed.
5. **`cleanupPeriodDays` is unset**, so the 30-day default is actively deleting
   the secondary record. Raising it is the single highest-value change available
   and costs nothing but disk.
