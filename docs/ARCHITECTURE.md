# Architecture

Why the design is what it is. Read it before changing a decision.
[`TESTING.md`](TESTING.md) is the companion: the rules the suite is held to.

dollet-relay ingests M3U and Xtream Codes providers and XMLTV guide data, lets
the operator curate channels, and re-serves them as an HDHomeRun tuner, an M3U
playlist, an XMLTV guide and an Xtream Codes server, with a streaming proxy in
front. The primary use case is **Live TV in Plex through HDHomeRun emulation**.
Everything else is in service of that path staying up.

## Memory is the budget

RAM is expensive, and a TV relay has no business spending it. So: **one process,
one container, one SQLite file.** No external cache or broker, no task queue,
no worker pool, no language runtime. The whole thing is a single Rust binary
with the SPA embedded in it.

The resident set is a formula rather than a number, because every term of it is
a thing an operator can change:

```
RSS ≈ base + Σ_rings(min(ring_seconds × bitrate, ring_max_bytes))
           + sessions × chunk_size×3
           + clients  × ~chunk_size×2
           + Σ_ffmpeg(~60 MB each)
```

Every term is measured except the bitrate a provider actually delivers, which is
theirs to vary:

| Term | Measured |
|---|---|
| base: the idle process, in-container, with a full-size instance imported | 10.8 MB resident; 13.6 MB from a cold image |
| a session's fixed cost: the packetizer's `BytesMut` at `chunk_size × 2` plus the pump's read buffer | tied to `chunk_size` |
| a client's cost: the chunk its cursor is holding | tied to `chunk_size` |
| one ffmpeg running the seeded audio-only profile against a real 1080p 8 Mbps stream | 62 MiB peak |
| the engine's soak harness (`dollet-stream/src/soak.rs`): 4 channels × 3 clients, one output profile, one channel failing over | 23.2 MiB accounted against 33.1 MiB of heap; the gap is the fixed terms, ~1.09× at production ring sizes |

Idle has two honest numbers, and they measure different things. `/health`
reports the process's resident set: the ~11 MB above. `podman stats` reports
the container's cgroup charge, about 30 MB on a live instance, because the
cgroup also counts the page cache for the database and everything else the
container has read. The README quotes the container figure, since that is what
an operator sees; the targets below are in the same terms.

The ring is the term that dominates. At 8 Mbps, 90 seconds of retention is
~90 MB per channel, so **`ring_seconds` defaults to 15 with a hard byte cap**,
and `ring_max_bytes` is derived from a 20 Mbps sizing bitrate because an ATSC
mux from an HDHomeRun is ~19.4 Mbps and a flat 32 MiB cap would bind.

Targets, absolute, as `podman stats` reports them:

- **Idle, which is the state the process sits in almost all the time: under
  60 MB.**
- **Three concurrent streams with one output profile active: under 250 MB**,
  ffmpeg included. The projection is 60 MB of rings, 3 MB of sessions, 2.4 MB
  of clients, the base, and one ffmpeg: about 136 MB.

About 60 MB is the most the whole process has been observed using in service.
That is a figure under load, not the idle target it happens to share a number
with. The audio-only profile copies video and encodes only audio; a profile
that re-encoded video would cost substantially more, and none of the seeded
ones do. Measure with `podman stats`, which counts ffmpeg, because ffmpeg is
real. `/health` reports resident memory and WAL size without authentication, so
the number this project is built around is visible on every deployment rather
than measured once.

## Decisions

| Area | Decision |
|---|---|
| Backend | Rust, tokio + axum + sqlx + dashmap |
| Database | **SQLite only**, WAL |
| Cache / broker | **None.** `DashMap` + `tokio::sync::watch` |
| Task queue | **None.** One in-process pool of 20, and a per-key guard |
| Frontend | React 19 + Vite + Mantine 8 + TanStack Table + zustand, embedded via `rust-embed` |
| Search | `LIKE '%x%'` with `ESCAPE`. No FTS5 |
| Regex | `fancy-regex` for user-authored patterns, `regex` internally |
| Transcoding | Shell out to `ffmpeg` / `vlc` / `streamlink` |
| Build | Podman, multi-stage, `cargo chef` |

**No FTS5.** Search on the list pages is substring matching: `HD` must find
`SPORTSHD`. FTS5 is token-prefix matching, so it would silently stop doing that
on the busiest UI surface, and it adds sync triggers to every bulk upsert.
Case-insensitivity is SQLite's default `LIKE`, which covers ASCII only; the
search term's `%` and `_` are escaped, and nothing sets `NOCASE` on the
searched columns. Against a few hundred rows anything more is ceremony. Revisit
only if a measurement ever says `LIKE` is slow.

**Naming.** `dollet-relay`: crates `dollet-core` / `dollet-stream` /
`dollet-server`, binary `dollet`, image `ghcr.io/halsafar/dollet-relay`,
source `github.com/halsafar/dollet-relay`, which is the URL the XMLTV
`generator-info-url` carries. The name is the Dollet Communication Tower from
Final Fantasy VIII, the relay repaired to carry the first broadcast in
seventeen years once the airwaves failed. A relay station in broadcasting is a
transmitter that rebroadcasts another station's signal, which is what this
program does.

## Layout

```
dollet-relay/
├── crates/{dollet-core,dollet-stream,dollet-server}/
├── migrations/          # 0001, frozen from 1.0; numbered changes go beside it
├── web/                 # React + Vite SPA, embedded via rust-embed
├── fixtures/            # snapshot corpora, the sample and synthetic seeds
├── docs/                # this file, TESTING.md, the logo
├── docker/Dockerfile
├── scripts/{build,release,dev,test,smoke,e2e}.sh and lib/serve.sh
└── .github/workflows/ci.yml   # one file; runs unchanged on Forgejo and GitHub
```

Three crates. `dollet-core` holds the domain types, the database layer, the
parsers, the serializers, settings and auth. `dollet-stream` is the streaming
engine and takes no database dependency. `dollet-server` wires both into axum
and owns every HTTP handler, the job scheduler and the importer. Split further
only when incremental compile time measurably hurts.

`crates/dollet-core/src/domain.rs` is the contract between the layers: the
database maps rows into those types, the parsers parse into them, and the
serializers serialize out of them.

## The streaming engine

```rust
DashMap<(Uuid, OutputKey), Arc<Session>>   // OutputKey ∈ Raw | Profile(id)
```

**The engine takes no database.** Its whole input is an ordered list of URLs, a
user agent, a command template and tuning constants; resolving a channel UUID to
its stream list happens in `dollet-server`. That is what makes the ring buffer,
the client cursors, the failover state machine, the subprocess handling and the
output-profile rings testable against a synthetic transport stream with nothing
else in place, and it is why the engine was the first thing built rather than
the last.

**The unit is `(channel, output_key)`, not `channel`.** Every HDHomeRun lineup
URL can name an output profile, and a profile is a transcode whose output is
itself a buffer that clients read: a consumer that is also a producer, a ring
downstream of a ring. Building around one ring per channel and retrofitting
that later would be the rework this document exists to prevent.

Each session owns:

- **An input**: a streaming HTTP body, or a child process running the stream
  profile's command with `{streamUrl}`, `{userAgent}` and `{channelId}`
  substituted. Reads accumulate to a chunk of `188 × 1361` bytes, which at
  8 Mbps is about four publishes a second.
- **A ring**: `parking_lot::RwLock` over a `VecDeque` of chunks, each carrying
  its index, its bytes and the instant it arrived, bounded by both bytes and
  duration. The arrival instant makes "start a client N seconds behind live" a
  local binary search.
- **A `watch::Sender<u64>`** carrying the newest index, so clients await
  instead of polling.

Why `watch` and not `broadcast`, written down so it is not "cleaned up" later:
`broadcast` receivers only observe values sent *after* subscribing, so it cannot
serve a client that starts behind live or one that has to jump forward to the
oldest chunk still available. **The ring is the source of truth; `watch` is
only a wakeup.** And **the sender is owned by the session, not the input task**:
stream death is the failover path, and clients must not be disconnected while
URLs swap underneath them.

Two invariants that produce bugs no one can diagnose later:

- **Never hold a buffer guard across an `.await`.** With `parking_lot` that is
  nearly a compile-time discipline; with an async lock it eventually deadlocks
  the producer.
- **Reset the packetizer on a stream switch.** Otherwise the old input's
  partial transport-stream packet is concatenated onto the new one's first
  bytes, which breaks decoder sync in a way that looks like a bad provider.

Backpressure is hyper's: the response body is polled only when the socket
accepts, so a slow client stalls its own cursor, falls out of the window and
skips forward. Each client has its own cursor, so there is no head-of-line
blocking, and lock contention is a non-issue at four write-locks a second per
channel.

### Failover

The engine walks a channel's sources in order. Each gets three attempts with a
short capped backoff (250 ms steps to 3 s) before the next is tried; a full
pass through the list waits a cooldown before wrapping to the top, and that
cooldown starts at 5 seconds, doubles per pass to a 60 second ceiling, and is
reset by sustained playback. One number was being asked two questions: a
single failed pass is weak evidence that could be one bad moment catching every
source at once, while the fifth is a provider-wide outage. A flat wait has to
be sized for the outage, so a viewer paid a full minute for a glitch that
cleared in seconds.

**A session that still has viewers never gives up.** A fixed switch ceiling was
tried first, and its consequence is concrete: a single-source channel, the
common case, died after three failed reconnects inside a second, so a
ten-second provider blip became a dead channel and every client was dropped.
The bounds that end a failing session are demand-shaped instead, and already
exist: a client that sees no data hits its keepalive cap and leaves, and a
session with no clients is reaped. Neither can spin forever, and both stop
because nobody is watching rather than because a counter ran out while someone
was. Two exceptions: a channel that has **never delivered a byte** still fails
fast, because that is misconfiguration rather than bad luck; and a lone source
retries at the capped backoff rather than the rotation cooldown, because a
cooldown exists to stop a full pass hammering a provider, which does not apply
when there is one.

A session outlives its last client by `channel_shutdown_delay`, so a viewer
channel-surfing back inside the window rejoins the running session rather than
paying for a reconnect. Per-account and per-profile `max_streams` are counted
per session, not per client, because one provider connection serves every
viewer of a channel. A stream profile can also be a redirect, answering 307
with the provider URL and opening no session at all.

The engine also parses the stderr of ffmpeg, VLC and streamlink for the codec,
resolution, frame rate and bitrate badges on the Stats page, and detects a
transcode that cannot keep up. Streamlink reports throughput and no nominal
bitrate, so it cannot distinguish a healthy 2 Mbps stream from an 8 Mbps one at
quarter speed; a streamlink profile is left with `stream_timeout` alone.

## The data layer

sqlx, with pragmas on every connection: `journal_mode=WAL`,
`synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`.

Queries are the runtime `sqlx::query` family rather than the compile-time
macros. The macros want a schema reachable at build time, which would put a
database in the container build, and several queries here compose a validated
column name into their SQL, which the macros cannot express. What catches a
wrong column instead is the integration suite: every handler runs against a
real SQLite seeded from the real schema, so a query that does not match the
migration fails a test rather than a request.

WAL means readers never block writers, but that is not the risk worth
defending against. The real one is **checkpoint starvation**: the WAL cannot
checkpoint while any reader holds an older snapshot, and a live streaming
server always has readers. Three cheap defences, warranted regardless of data
size: hold no read transaction across an `.await`; commit bulk work in batches
rather than one long transaction; and expose WAL size on `/health`. Batched
commits stay sufficient three orders of magnitude from the volumes this runs at.

**`ChannelOverride` and the `effective_*` coalesce layer are a first-class
schema concern.** A refresh rewrites the row a provider owns; the operator's
edits live in an override row beside it; and every output query sorts and
filters on the coalesced value *in SQL*, so the lineup, the guide and the
playlist all agree without any of them resolving overrides on the way out.
Every output query depends on it, which is why it was designed in rather than
retrofitted.

**A group owns a number range.** A new channel takes the next multiple of the
lineup's channel step above the group's own highest number, stepping over
anything another group holds there, and only once the end of the range is
reached does a free grid slot get used. Filling from the bottom would hand a
gap the operator left on purpose to whatever the provider adds next. The step
and the group block size are settings; 1 and 100 give a dense lineup, 10 and
1000 leave room between neighbours. Nothing in a refresh ever moves a number,
because a renumbered channel is one the operator's recordings and favourites no
longer point at; renumbering is an explicit action that shows its plan first.

**Order is the operator's, in two sizes.** A renumber compacts a group onto the
grid, walking it by its current numbers, its channel names, its guide names or
its `tvg-id`; digit runs compare as numbers, so `TSN2` precedes `TSN10`, and a
channel whose key is missing keeps its place and goes last. That re-scans a
whole group in Plex, so moving one channel is a separate thing: dragging it
between two others gives it a number from the gap the step leaves, writing one
number and moving nothing else. The gap is spent whole numbers first, nearest
its middle: `channel_number` is REAL so that an OTA `5.1` can sit beside `5.2`,
but halving every gap turns a lineup into 20047.5 and then 20048.75, and a
channel number is something a person reads off a guide. A fraction is what a
gap narrower than 1 costs. When a pair has no room left between them the drop
is refused rather than pushing everything below it down: the gap is the
budget, and renumbering the group is how it is refilled.

The base schema is one file, `migrations/0001_initial.sql`, frozen from 1.0:
the development migrations were folded into it before release so that 1.0
ships with no history to carry. Every change from there is a new numbered file
beside it, including a fix to a mistake in the one before. sqlx checksums the
file text, so even a comment edit makes existing databases refuse to open.

SQLite cannot alter a `CHECK` constraint, and the documented table rebuild's
first step, `PRAGMA foreign_keys=OFF`, is a no-op inside the transaction sqlx
runs every migration in, so a rebuild of a table with cascading children
deletes them and reports success. The Rust enum is the real constraint;
`CHECK (x IN (...))` is avoided on any table with children.

### Backups

A backup is a zip of two entries: `dollet.sqlite`, and `backup.json`, which
records the build that wrote it, when, why (by hand, on the schedule, uploaded,
or before a restore) and the newest migration applied. The database is the
whole instance; the cache beside it is rebuilt on demand, so nothing else
travels. Every byte of it is streamed, through the zip writer, the upload and
the download, because a database is the one thing here that can run to
hundreds of megabytes.

**The snapshot is `VACUUM INTO`, never a file copy.** This process always has
writers, and a copy of a WAL database taken while one commits is torn: the
main file and the WAL disagree about which pages are current. `VACUUM INTO`
reads one snapshot and writes a compacted, self-contained file without
stopping anyone. It is one statement, so the snapshot is held for as long as
the copy takes and no longer.

Backups are named by the server, `dollet-backup-<UTC stamp>-<why>.zip`, and
the API addresses them by that name, which is parsed before it is ever joined
onto a path. An upload or a restore is checked before anything is kept or
replaced: exactly the two entries, read by name and never used as a path; a
SQLite header; `PRAGMA integrity_check`; and a migration history this build
can boot. A newer migration, or one whose checksum differs, would make the
boot that applies it fail after the old database is already gone, so both are
refused. An older schema is fine, because that boot migrates it.

**A restore is a restart, not a swapped pool.** The pool is held by every
handler, stream session and job, so the file under it cannot be changed while
the process runs. A restore takes a backup of the current instance first, and
restores nothing if that fails; stages the chosen database beside the live one;
and asks for the same graceful shutdown SIGTERM gets, so streams end and jobs
drain. The next boot moves the staged file into place before the pool opens,
removing the old WAL and shared-memory files first: left beside the new
database, SQLite would replay the old instance's pages onto it. A staged file
that cannot be moved stops the boot rather than serving the database the
operator was told had been replaced.

That makes a restarting supervisor a requirement. The process exits 0, which
Compose's `restart: unless-stopped` answers by starting it again; a bare
process, or a supervisor that restarts only on failure, stays stopped, and the
UI says so. Restoring another instance's backup also replaces the key that
signs sessions, so everyone is signed out.

**Retention counts scheduled backups only.** After each scheduled run the
oldest scheduled ones beyond the configured count are deleted. A backup taken
by hand, uploaded, or taken before a restore was made on purpose, and a timer
is not what should delete it. A count of zero is refused: it would delete each
scheduled backup as it was written, a schedule that protects nothing while
looking switched on.

## The scheduler

One in-process pool of twenty, a per-key guard so each account and source
refreshes single-flight, and a `CancellationToken` per job with progress pushed
over the WebSocket. Twenty is wide because every job is a provider refresh
waiting on someone else's HTTP rather than on this process.

One pool rather than one per class: a second would exist so that a three-hour
ingest cannot occupy a slot a user-facing action needs, and there are no
user-facing jobs here to protect. Work is on-demand except provider and guide
refreshes and the scheduled backup, which run on their own intervals. A newly registered job's first run
is scheduled a full interval out, so a fresh or imported instance is refreshed
once by hand.

The job row carries one timestamp about outcomes, `last_success_at`, which the
account and guide payloads carry as `updated_at` and the Sources page shows as
"Refreshed": three names for it, and the wire one is the oldest. It records
when the unit last had good data rather than when it last stopped. Claiming a
job leaves it alone, so a refresh in flight still reports the one before it;
a failed or cancelled run leaves it alone too, so a provider failing nightly
reads as a week stale instead of as refreshed an hour ago. When a failed run
ended is not recorded: a failure is diagnosed from `state`, `last_error` and
the logs, and a second timestamp beside this one would only be the wrong one
to read.

Conditions a job hits that an operator has to decide about (a full number
range, a filter that will not compile, a hash key that selects nothing) are
raised as notifications upserted on `(kind, subject)` rather than logged and
forgotten: one row however many nights the condition recurs, cleared by the
producer when it next looks and the condition has gone.

## The HTTP surface

`/api/` nests `accounts`, `channels`, `core`, `epg` and `m3u`, with
notifications and session control merged alongside them. Everything a client
outside this project dials sits outside that prefix, because those paths are
the contract; that is the test, not "unauthenticated" and not "about streams":

- `/output/{m3u,epg}`, with an optional channel profile;
- `/hdhr/` in four scopes (bare, by channel profile, by output profile, and
  both), each serving `discover.json`, `lineup.json`, `lineup_status.json`
  and `device.xml`;
- `/proxy/ts/stream/<uuid>`, the playback URL those lineups carry;
- `player_api.php`, `panel_api.php`, `get.php`, `xmltv.php` and the bare
  `/<user>/<pass>/<id>` route Xtream clients dial;
- `/ws`.

The SPA is served for everything else, and an unrouted `/api` path is a 404
rather than the page, so an API rename fails at the right layer. Stopping a
session, evicting a viewer, switching a source and reading live stats are
admin actions this project's own UI makes, so they are `/api/proxy/...` like
everything else it calls.

axum resolves routes by specificity and panics at startup on a conflicting
insert, so the route table has a test rather than an eyeball, and the bare
three-segment Xtream route is proven not to shadow the SPA.

**Every HDHomeRun lineup URL is absolute.** The host and scheme come from the
request as the client saw it, so `X-Forwarded-Proto` and `X-Forwarded-Host`
handling is load-bearing and `DOLLET_ADVERTISED_BASE_URL` overrides it; getting
this wrong fails as "discovery works, playback doesn't". Those headers are
honoured only from a peer `DOLLET_TRUSTED_PROXIES` names, which defaults to
nothing, because the `network_access` allowlist and the advertised origin both
read the same address. Artwork URLs can be rebased separately with
`DOLLET_ARTWORK_BASE_URL`: a guide icon is fetched by the browser rendering the
guide, not by the Plex server.

**The WebSocket filters per receiver.** Stats payloads carry channel UUIDs
usable against the anonymous stream endpoint, provider URLs and client
addresses, and are admin-only.

**Every outbound fetch goes through one guarded HTTP client**, whose resolver
refuses loopback, link-local including the cloud metadata address, multicast
and reserved space, resolving first and pinning the connection to the validated
address so DNS rebinding cannot defeat check-then-fetch. Every URL here is
attacker-influenced: provider playlists, guide icons, artwork. The engine
cannot depend on `dollet-core`, so it takes the client by injection. Provider
streams are fetched with private space allowed, because a tuner on the LAN is a
supported source; fetches whose URL came from provider *content* rather than
reviewed configuration are not.

Provider artwork is served with a sandboxing content-security policy and
`nosniff`, because an SVG from a logo URL is same-origin script beside the SPA's
tokens.

### Regex compatibility

User-authored patterns (M3U profile search and replace, group filters, bulk
rename) are persisted and may have been written for a PCRE-flavoured engine or
in the JavaScript dialect. Rust's `regex` has no backreferences and no
lookaround by design, so those patterns compile with `fancy-regex`, with a step
limit standing in for a timeout, and the importer compiles every stored pattern
on the way in and reports the ones that will not.

**`$1` becomes `\1` in search patterns only, never in replacement templates.**
In a search pattern `$` followed by a digit can never match, since `$` is an
anchor, so it is always the JavaScript spelling of a backreference and is
rewritten. Rust's replacement syntax is *already* `$1`, so rewriting there would
emit the literal text `\1` and silently break every rename.

### Output caching

Plex re-fetches the guide constantly. `/output/epg` and `/output/m3u` are
rendered to the data directory's `cache/` and served from disk with a short
TTL behind a single-flight mutex map, so a burst of clients renders once. The
disk-backed form costs the same to write as an in-memory one and does not
degrade if a full XMLTV feed ever arrives.

## Shape decisions

Free now, expensive later, and not speculative abstractions:

1. **`(channel, output_key)` rings**, so output profiles, and any future fMP4
   or HLS output, are not a rewrite.
2. **`ChannelOverride` and `effective_*` in SQL**, because every output query
   depends on it.
3. **A client is a cursor receiving `Bytes`**, so a recorder is a client that
   writes to a file.
4. **Catch-up fields on `Channel` and `Stream` from the first migration**, so
   they are never backfilled out of a JSON blob.
5. **One artwork proxy and disk cache**, shared by logos and guide posters.
6. **One guarded HTTP client** for every outbound fetch.
7. **A typed Xtream Codes client** rather than ad-hoc JSON at each call site.
   Only the live actions are implemented, since VOD and series are out of
   scope, but auth, the base URL and error handling are shared, so another
   action is a function rather than a second client.

Dropped: HTTP `Range` on the live proxy. Live transport streams are unbounded
and never range-served, and a file server handles ranges off the shelf.

## Behaviour decisions, and why

Each of these is pinned by a test, and each has a reason that is not "someone
else does it this way".

- **A programme with no stop time is skipped and counted.** It cannot be placed
  on a grid, and a guide quietly losing programmes must be visible somewhere.
- **Unquoted M3U attributes are read.** Playlists in the wild carry them, and a
  scanner that requires quotes drops a stream's id, number and group without a
  word.
- **`now_playing` is half-open at a programme boundary.** Listings are ordered
  by start time, so a client taking the first current match would otherwise be
  handed the programme that has just finished.
- **A control character XML cannot carry is stripped.** Plex rejects the whole
  guide over one byte.
- **A `"` in a channel name is written as `&quot;` in an M3U attribute.** M3U
  has no escaping rule, and a raw quote closes the attribute early and makes
  every attribute after it unreadable.
- **Whitespace in an XML attribute is written as a character reference.** A tab
  or newline there is legal to write and lossy to read back, because parsers
  normalise it.
- **A newline in provider data never reaches the M3U.** Otherwise a provider
  can forge a playlist entry.
- **Xtream's `num` is stable**, the lowest free integer with a fractional
  number truncated when free, because a client stores it as a channel's
  identity and any other scheme renumbers every channel in every client.
- **A dummy listing's id is the stream id and the slot's start**, so it is
  stable across fetches and can be read when debugging a client.
- **`server_info.port` is the advertised port**, the one every other URL in the
  same response carries, because an Xtream client builds stream URLs from it.
- **Bad Xtream credentials answer with a 404 page rather than a 401.** A client
  that gets a 401 prompts for a password it was never given.
- **A missing output profile serves the plain lineup and a missing channel
  profile an empty one**, never an error: a tuner Plex has already added must
  keep answering.
- **A filter rule that will not compile costs one rule, not the set.** Dropping
  every rule on one typo changes which streams exist, which is
  indistinguishable from the provider changing its lineup.
- **A hash key that selects nothing refuses to write.** Every stream would
  hash the same, one would match, and the retention window would turn the rest
  into a delete that cascades every channel assignment away.
- **A vanished stream is marked stale, not deleted**, until `stale_stream_days`
  have passed. A provider serving a truncated playlist for an hour costs
  nothing.
- **EPG matching is fuzzy only.** A language-model tiebreaker for the 50–80
  score band would mean ONNX Runtime plus ~90 MB of weights, most of what this
  project exists to avoid. The band is surfaced in the UI for a one-click
  decision instead. On the instance this was first measured against, that
  band was empty.
- **Auto-matching runs only when asked**, and over exactly the channels a
  refresh just created when matching on refresh is on. A timer that rewrites
  `epg_data_id` across the catalogue every night would overwrite hand
  assignments.

## Testing

[`TESTING.md`](TESTING.md) is the policy. The short form:

- Unit tests beside the code; integration tests drive the axum `Router` through
  `tower::ServiceExt::oneshot`, no sockets, fully parallel, a temp SQLite per
  test seeded from `fixtures/sample.sql` or the designed
  `fixtures/synthetic/instance.sql`.
- **Coverage is a ratchet: it may never decrease.** There is no percentage
  target except for one list that must be 100%: the M3U parser, the XMLTV
  parser, the failover state machine and the four serializers. Pure functions
  where total coverage is cheap and bugs are expensive.
- **The snapshot corpus is the real deliverable.** `fixtures/golden/` pins
  every output surface byte for byte; `fixtures/ingest/` pins every
  reconciliation decision row for row; `fixtures/import/` pins the migration.
  A coverage number says code ran; a snapshot says the bytes are right.
- Two layers run outside the suite on purpose: `scripts/smoke.sh` asks a built
  image for every client route, and `scripts/e2e.sh` drives a real browser
  against two real servers.

## Build and release

Multi-stage: `node:22-alpine` builds the SPA; `rust:1.98-trixie` with
`cargo-chef` caches dependencies separately from source and builds the binary;
`debian:trixie-slim` with ffmpeg and the VA-API drivers carries the binary, the
embedded SPA, a non-root user and the `/data` volume. One container, one
process, tini as PID 1 so a killed pipeline's grandchildren are reaped.

`scripts/build.sh` builds and tags locally; `scripts/test.sh` runs the suite in
the builder image; `scripts/release.sh <version>` is the whole release: it
writes the version into `Cargo.toml`, `web/package.json` and both lock files,
builds the image from that tree, smoke-tests it, and only then commits, tags
and pushes. CI publishes the image from the pushed tag, after the suite passes
against it. Nothing enters history until the image has passed, and nothing is
installed on the host, except that `release.sh` calls host `cargo` and `npm`
to rewrite their own lock files, which is why it checks for both before it
writes anything.

## Licence

AGPL-3.0-only.
