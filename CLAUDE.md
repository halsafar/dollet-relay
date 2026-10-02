# dollet-relay

Ingests M3U / Xtream Codes providers and XMLTV guide data, curates channels,
and re-serves them as HDHomeRun, M3U, XMLTV, and Xtream Codes outputs with a
streaming proxy in front.

**Primary use case: Live TV to Plex via HDHomeRun emulation.**

`docs/ARCHITECTURE.md` holds the reasoning behind every architectural decision.
Read it before changing one.

## Memory is the budget

RAM is expensive, and a TV relay has no business spending it. This is **one
process**: no external cache, broker, task queue or worker pool. `/health`
reports resident memory; anything that adds a long-lived allocation is measured
against it rather than argued about.

## Design principles

Binding. Code that violates them gets sent back in review.

- **Clean and human-readable.** Comments explain *why*, never *what*. No
  doc-comment boilerplate on self-evident functions.
- **No speculative abstraction.** No traits with one implementor, no config
  knobs nobody asked for, no extension points for features that do not exist.
- **Scope is the enumerated list.** See "Scope" below. A deferred feature is a
  documented gap, never a silent omission or a stub that fakes success.
- **Clients are the contract.** What binds us is what clients consume: Plex
  reading `lineup.json`, an Xtream player reading `player_api.php`, anything
  reading the M3U and XMLTV. Those bytes are pinned under `fixtures/golden/`.
  Everything else is ours to decide, and a decision is justified by a client's
  behaviour, by a measurement, or by a reason written down. When a value is
  arbitrary, say so.

## Stack

Rust (tokio, axum, sqlx, dashmap) + React 19 (Vite, Mantine 8, TanStack Table,
zustand), one binary with the SPA embedded in it. **SQLite only; nothing here
speaks PostgreSQL at runtime.** No external cache, broker or task queue:
`DashMap` plus `tokio::sync` for the first two, one in-process job pool for the
third. Dependencies are declared once, in `[workspace.dependencies]`, and
inherited with `workspace = true`.

## Working with other agents

Several agents may work on this project at once. The rule that keeps them
apart is **one task, one branch, one worktree**: no two agents ever share a
checkout. An orchestrating agent coordinates, reviews and merges from the main
checkout; it creates a worktree for every subagent it spawns (the Agent tool's
`isolation: "worktree"`, or `git worktree add` under `.claude/worktrees/`,
which `.gitignore` already covers) and points the subagent at it.

A branch lands on `main` as **one squash commit**: a short title and a few
lines saying what changed and why. Nothing else from the branch's history
survives the merge, so commit freely while working, and put the proposed title
and description in the final report.

## Migrations

The schema is one file, `migrations/0001_initial.sql`, and **it is frozen**.
The development migrations were folded into it before 1.0 so that the release
ships a schema with no history to carry; instances already run on it, so the
freeze is in effect now, not at the version bump.

**Every change is a new numbered file**, including a fix to a mistake in the
previous one, and including a comment: sqlx checksums the file text, so
an edit makes every existing database refuse to open with *"migration N was
previously applied but has been modified"*. A dev database that stops booting
is a deleted volume; a curated instance that stops booting has no export and no
way to replay a month of edits.

**Prefer not to need a table rewrite.** SQLite cannot `ALTER` a `CHECK`
constraint, the documented workaround is a 12-step rebuild, and its first step
— `PRAGMA foreign_keys=OFF` — is a **no-op inside a transaction**, which is
where sqlx runs every migration. So a rebuild of a table with cascading
children deletes them and reports success. Avoid `CHECK (x IN (...))` on any
table with children; the Rust enum is the real constraint.

## Hard invariants

Each of these, violated, produces a bug that is very hard to diagnose later.

**Database**

- Never hold a read transaction across an `.await`. SQLite's WAL cannot
  checkpoint while a reader holds an older snapshot, and this process always
  has readers, so a held snapshot starves the checkpointer and the WAL grows
  without bound.
- Commit bulk work in batches, never one long transaction.

**Streaming engine (`dollet-stream`)**

- The crate takes no database dependency. Its input is an ordered list of URLs,
  a user agent, a command template and constants. That is what makes it
  testable against a synthetic transport stream.
- The registry is keyed by `(channel, OutputKey)`, not by channel: an output
  profile is a ring downstream of a ring.
- The ring is the source of truth; `watch` is only a wakeup. `broadcast` cannot
  serve a client starting N seconds behind live.
- The `watch` sender is owned by the session, not the input task — stream death
  is the failover path, and clients must survive it.
- Never hold a buffer guard across an `.await`.
- Reset buffer position on stream switch, or a partial packet from the old
  process is spliced onto the new one's first bytes and breaks decoder sync.

**HTTP**

- Every outbound fetch uses a client from `dollet_core::http::client`, whose
  resolver refuses blocked address space. Never construct a bare
  `reqwest::Client`. `dollet-stream` cannot depend on `dollet-core`, so it takes
  the client by injection from `dollet-server`.
- **Pass `allow_private = true` for provider streams.** The resolver blocks
  RFC1918 by default and LAN tuners are a supported source, so without it every
  such stream fails to open. Leave it off for URLs that came from provider
  *content* — guide icons, artwork — rather than reviewed configuration.
- User-authored regexes (M3U profile search/replace, group filters, bulk
  rename) compile with `fancy_regex`, never `regex`: they may use lookaround or
  backreferences.
- **`$1` → `\1` conversion applies to search patterns only, never to
  replacement templates.** In a search pattern `$` followed by a digit can never
  match, so it is always a JavaScript-dialect backreference. Rust's replacement
  syntax is *already* `$1`, so converting there emits the literal text `\1` and
  silently breaks every rename.
- HDHR lineup URLs are absolute, so `X-Forwarded-Proto`/`X-Forwarded-Host`
  handling is load-bearing. Wrong values fail as "discovery works, playback
  doesn't".
- The WebSocket filters per receiver. Stats payloads carry channel UUIDs usable
  against the anonymous stream endpoint, provider URLs and client IPs, and are
  admin-only.

## Testing

```bash
scripts/test.sh --local              # fast: host toolchain
scripts/test.sh                      # canonical: in the builder image
scripts/test.sh --local --coverage   # cargo llvm-cov
scripts/smoke.sh <image>             # a built image, over HTTP
scripts/e2e.sh                       # browser journeys against two real servers
```

Unit tests sit beside the code. The server's integration tests live in
`crates/dollet-server/src/api/tests.rs` and drive the axum `Router` through
`tower::ServiceExt::oneshot` — no sockets, fully parallel — against a temp
SQLite seeded from `fixtures/sample.sql` or `fixtures/synthetic/instance.sql`;
`crates/dollet-core/tests/` holds the snapshot, fixture and ingest suites.
`docs/TESTING.md` sets out what each layer may depend on.

- **The snapshot corpus is the real deliverable.** M3U, XMLTV, HDHR JSON and XC
  JSON are compared whole against `fixtures/golden/`; nothing tolerates a known
  difference. A deliberate output change is re-pinned with the ignored `repin`
  tests and reviewed as a diff.
- **Coverage is a ratchet: it may never decrease.** The web thresholds in
  `web/vite.config.js` enforce it; the Rust floor is a table in
  `docs/TESTING.md`, compared by hand, since nothing in CI runs `llvm-cov`.
  No percentage target except one list that must reach 100%: the M3U parser,
  the XMLTV parser, the failover state machine, and the four serializers. The
  XMLTV serializer and the failover state machine are not there yet. That is
  open work, not a tolerance.
- **No test may pass only because `fixtures/sample.sql` lacks the case.** That
  is one instance; `fixtures/synthetic/instance.sql` is every shape the product
  supports. Add a missing shape there with a line in its README saying why,
  rather than building it ad hoc in the test.
- **A test written for a fix is shown to fail without the fix.** Revert it,
  watch the test fail, restore it. A test that passes both ways is asserting
  something incidental, which is how every significant defect here survived a
  green suite.
- **Every fixture passes a leak scan on every run.** `golden.rs`, `fixtures.rs`
  and `ingest.rs` enforce a host allowlist, because hand editing is when a
  real URL gets pasted in.
- `smoke.sh` and `e2e.sh` stay outside `test.sh` deliberately: the builder image
  has no browser, and a browser run is too slow to precede every `cargo test`.
  Both have caught bugs a green suite missed.

## Build

```bash
scripts/build.sh                     # local image
scripts/release.sh 1.0.0             # bump, build, smoke, commit, tag, push; CI publishes
scripts/dev.sh                       # server, host toolchain
scripts/dev.sh web                   # vite dev server
```

Release builds and the canonical test run go through podman. Inside a
container or sandbox, install whatever the work needs. On the host, prefer the
podman paths so nothing lands on it; `--local` and `dev.sh` are the two that
need the host's own cargo and npm.

## Scope

**In:** channels, streams, groups, channel profiles, logos; M3U and Xtream
Codes ingest; XMLTV ingest, EPG matching, Dummy EPG; the streaming proxy with
failover and output profiles; HDHR, `/output/m3u`, `/output/epg`, Xtream Codes
server API (live actions); users, auth, API keys, settings; backup and restore
of this instance; the web UI.

**Out** — each a documented gap, rebuilt later if wanted: plugins, VOD, DVR,
catch-up/timeshift, Schedules Direct, Comskip, webhooks, HLS and fMP4 output.

EPG matching is fuzzy only. A language-model tiebreaker for the ambiguous band
would mean ONNX Runtime plus ~90 MB of weights; the band is surfaced in the UI
for a one-click decision instead.

## Licensing

AGPL-3.0-only.
