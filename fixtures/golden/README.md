# Wire-format snapshots

What the four output surfaces serve for the sample instance plus the
adversarial rows below, pinned byte for byte: the HDHomeRun payloads Plex reads,
the Xtream Codes actions a player calls, and the M3U and XMLTV anything else
consumes. This directory is the contract with those clients. A diff here is a
change to what a client receives, and it is either deliberate and reviewed or a
regression — the harness tolerates no known differences, so there is no third
case.

## What is here

| | |
|---|---|
| Snapshots | 42 files, one per request, indexed by `manifest.txt` — file, HTTP status, request path |
| Request host | `ipx.test:9191`, so every absolute URL is stable and carries no real hostname |
| Instant | `captured-at.txt` — the clock the guide window and the dummy schedule are computed from |
| Dummy channels | `dummy-channels.txt` — every identifier form of each channel with no guide data: numbers, `tvg_id`s, station ids, row ids, Xtream numbers and names, because each guide names channels differently |
| Integrity | `checksums.tsv` — file, byte length, FNV-1a; a truncated or half-written snapshot fails by name, which `.exists()` alone did not |

## The lineup is small and the hard cases are in it

Twelve channels in four groups is the whole lineup — every one numbered with a
whole number, none hidden, no name with a quote or a comma. A corpus of easy
rows verifies the common path and nothing else: an adversarial review deleted
XMLTV attribute escaping, stopped writing `<sub-title>`, stopped stripping
control characters, disabled `<`/`>` escaping, removed the M3U
newline-injection guard, deleted the entire `write_extra` path and broke the
Xtream number-collision allocator, and a suite over easy rows stayed green
through every one.

So `sample.sql` carries, at ids from 900: a channel whose name holds a quote, a
comma, an `&` and a `<`; a fractional channel number; a channel with no number;
a hidden channel; a station id; a channel profile with two members enabled and
the rest disabled; and a channel in no group, named in Cyrillic, with a guide
of its own. The snapshots describe the instance *with* those rows.

## The programmes are the snapshot's, not the seed's

`/output/epg` serves programmes relative to the real clock, so a seed with
fixed timestamps never reaches the served guide. The programme sections of the
five guide snapshots are therefore a hand-kept XMLTV document that the parser
and serializer round-trip byte for byte: three programmes per real channel,
the dummy blocks `repin_the_dummy_guide_snapshots` regenerates at
`captured-at.txt`, and the programmes a plain-ASCII corpus would never
exercise — accented text, a `<sub-title>`, XML-special characters in a title,
both episode-number forms, a rating, a credit, a control character XML cannot
represent, two programmes that overlap, one of zero length, a description
longer than a line buffer, and titles in Cyrillic, CJK and emoji with Japanese
and right-to-left Arabic descriptions. The server re-pin rewrites only the
`<channel>` elements of these files.

The Xtream endpoints need a user with an `xc_password`, so the sample's
administrator is `fixtureadmin` / `fixturepass`. Timestamps in the adversarial
rows are literals rather than the clock, because `created_at` becomes Xtream's
`added` field and a clock reading would make the snapshots unreproducible.

## Hosts and credentials

The snapshots and `sample.sql` use the same stand-ins, which is what makes a
diff between them meaningful rather than a permanent false positive:

| | Fixture |
|---|---|
| provider host | `provider.example` |
| stream path | `/live/fixtureuser/fixturepass/<id>` |
| logo host | `logos.example` |
| guide host | `guide.example` |

Channel ids, UUIDs, names, group ids and logo ids are unchanged between the two.

`golden.rs` scans every file in this directory on every run, base64 payloads
decoded: no host outside that list, no `/live/<user>/<pass>/` segment that is
not the fixture's, and no path segment shaped like a token. The scan is an
allowlist rather than a list of secrets to search for, because a denylist would
have to name the very strings it guards against.

## How they are checked

`crates/dollet-core/tests/golden.rs`, against the serializers alone:

- Playlists, lineups, discovery, `device.xml` and the Xtream actions, with the
  channel table reconstructed from the snapshots themselves — a lineup entry
  carries the number, name and UUID it was built from.
- The five guides, round-tripped: parsed by our XMLTV reader, written by our
  writer, and compared byte for byte over every channel and every programme.
- The dummy schedule, regenerated for the committed instant and compared block
  for block in every guide and in the Xtream data table.
- The end-to-end path from `sample.sql` through the query layer, which is
  where `effective_*` coalescing, lineup order and the hidden-channel filter
  live.

`crates/dollet-server/src/api/tests.rs`, against the router over `sample.sql`:

- Every path in the manifest is served and answers with the recorded status.
- Playlists, `device.xml` and the channel block of every guide are compared line
  by line; JSON payloads are compared as values, so their formatting is free.

Two things cannot be compared whole. Payloads carrying a clock reading —
`xc-account-info.json`, `xc-panel-api.json`, the short EPG and both data tables
— are compared with the clock substituted and each reading checked for shape.
And the guide's programmes are compared by the round trip rather than through
the server, because `sample.sql` samples the programme table where the
snapshots carry every row.

## Changing a snapshot

A deliberate change to an output fails here first, and the assertion names the
file and the first line that differs. Re-pin, then read the diff — every
changed line is a change to what a client receives:

```sh
cargo test -p dollet-server repin -- --ignored
cargo test -p dollet-core --test golden -- --ignored repin
```

The first rewrites what the router serves: playlists, `device.xml`, both error
bodies, the channel block of every guide, and any JSON payload whose value
changed, pretty-printed. The second rewrites what only the generator can
produce: the dummy channels' programme blocks in every guide, and the dummy
data table. Both recompute `checksums.tsv`. The clock-dependent payloads are
edited by hand, since no run can reproduce the instant they were taken at.

## Dummy EPG

The synthetic guide a channel gets when it has no source. It matters out of
proportion to its size: a channel with no `<programme>` at all is one Plex may
not show, so a schedule that drifts is a channel that quietly stops appearing.
Nine channels here are on one.

What the snapshots pin: three days of four-hour blocks, aligned to the hour,
starting at the hour of the committed instant and continuing across midnight
rather than restarting at it; the title is the channel's own name, escaped
through the same writer as everything else; every block carries a description
naming the channel; and in the Xtream data table `now_playing` is exact — the
first block and only the first, because the run starts at the current hour.
Listing ids name the stream and the slot's start, so a client caching by id
sees the same ids on every fetch and a person debugging one can read them.

## Decisions these snapshots pin

Each is a choice with a reason, and each is enforced by the byte comparison
unless a unit test is named.

- **A `"` in a channel name is written as `&quot;` in an M3U attribute.** M3U
  has no escaping rule, and a raw quote closes the attribute early and makes
  every attribute after it unreadable.
- **A control character XML cannot carry is stripped.** Plex rejects the whole
  document over one byte. The adversarial programme carries one, and no
  snapshot does.
- **`now_playing` is half-open at a programme boundary**, so exactly one
  programme is on at any instant. Listings are ordered by start time, so a
  client taking the first match would otherwise be handed the programme that
  has just finished. The instant here is mid-block, so this is pinned by
  `output::xc::at_a_boundary_exactly_one_programme_is_playing` instead.
- **The unscoped tuner's `DeviceID` is the constant `12345678`**; a scoped
  tuner derives its own from the profile. Plex keys a paired tuner on it.
- **A channel profile name in a URL is percent-encoded** (`Living%20Room`),
  because a space is not a legal URI character. The `DeviceID` keeps the raw
  name; it is not a URL.
- **`server_info.port` is the advertised port**, the same one every other URL in
  the response carries, because an Xtream client builds stream URLs from it.
- **A missing output profile serves the plain lineup and a missing channel
  profile an empty one**, never an error: a tuner Plex has already added must
  keep answering. A missing M3U profile is a 404.
- **Bad Xtream credentials answer with a 404 page rather than a 401.** A client
  that gets a 401 prompts for a password it was never given.
