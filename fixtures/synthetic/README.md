# The synthetic instance

`instance.sql` is the second seed. `fixtures/sample.sql` is the reference
instance — a small lineup shaped like a provider's, with the hard cases the
output snapshots depend on; this one is **designed**, and the design is the
point.

[`docs/TESTING.md`](../../docs/TESTING.md) sets out why. Every serious defect
found on this project lived in a shape the reference instance does not have: the
dummy-EPG generator panicked on `?days=` for months because the sample has no
dummy source, and `hdhr_output_profile_id` was documented and unread because no
channel in the sample uses an output profile. A test cannot fail against data
that has no example of the thing it tests. So this file exists to make **every
supported shape reachable without a test building it first**.

Applied after the migrations exactly as `sample.sql` is, by
`TestApp::synthetic()` — and by `dollet seed`, which is how `scripts/e2e.sh`
stands up the instance its browser journeys run against. Ids start at 1000, so
nothing here can collide with the rows the migrations seed (1–5) or with
`sample.sql`. Hostnames are `provider.example`, `xtream.example`,
`guide.example` and `logos.example`, and every credential is obviously fake;
`the_synthetic_corpus_references_no_host_outside_the_allowlist` in
`crates/dollet-core/tests/fixtures.rs` enforces both on every commit.

**The list below is checked, not just written down.**
`the_synthetic_seed_carries_every_shape_an_output_can_take` in
`crates/dollet-server/src/api/tests.rs` asserts each entry as a query. A row
deleted in a tidy-up fails that test by name instead of quietly making three
other tests pass against nothing.

## Programme times, and `now`

The guide window, the TV-guide grid and the Xtream short EPG are all decided
relative to the current instant, so the interesting programmes are the ones
around it. Those rows are written by SQLite itself —
`strftime('%Y-%m-%d %H:%M:%f', 'now', '+30 minutes')` — at the moment the seed
is applied. A fixed timestamp would be "a programme ending exactly now" for
exactly one day, and a silently useless row after that.

Everything else in the file is a fixed timestamp, because nothing else depends
on the relationship.

| id | relationship to `now` | why |
|---|---|---|
| 1001 | `-60m` → `now` | ends **exactly now**: the boundary `>=` and `>` disagree about |
| 1002 | `now` → `+60m` | starts **exactly now**: what "on now" has to resolve to. Also the only programme carrying `season`, `episode`, `new` and `live` in `custom_properties`, which is where XMLTV's extras live because the table has no column for them |
| 1003 | `+30m` → `+90m` | **overlaps** 1002; providers publish these and a grid that assumes otherwise stacks them |
| 1004 | `+180m` → `+240m` | after a deliberate **gap**: nothing is scheduled between `+90m` and `+180m` |
| 1005 | `-30m` → `+30m` | spans now on a *second* guide channel, so "this channel's listings" is distinguishable from "every listing" |
| 1006 | `+30d` → `+30d 60m` | outside any window a client asks for, so `?days=` bounding the answer is provable |

## Users

`dollet-test-<role>` is the password in every case, hashed in Django's format
at **1,000 iterations** rather than the shipped 1.2 M. The format is what is
under test — the importer carries these hashes across verbatim — and the work
factor is not; a suite that signs in dozens of times cannot afford the real one
in an unoptimised build. The constants live in one place, `mod synthetic` in
`crates/dollet-server/src/api/tests.rs`.

| id | user | why |
|---|---|---|
| 1000 | `synthadmin`, level 10, API key, `xc_password` | the principal most of the API is written for |
| 1001 | `synthstandard`, level 1, API key, `stream_limit = 1`, `xc_password`, restricted to `Living Room` | the only user whose Xtream catalogue is narrowed, and the only one whose `max_connections` comes from a personal cap rather than the provider |
| 1002 | `synthstreamer`, level 0 | the level that may authenticate and may reach almost nothing |
| 1003 | `synthinactive`, level 10, **deactivated**, API key | the "setup cannot be reopened" case: `initialize-superuser` counts admins regardless of `is_active`. The key exists so the authorization matrix can *present* a credential and watch it be refused — sending nothing would only re-test the anonymous row |

## Core

| row | why |
|---|---|
| `user_agent` 1001 `Synth Provider Agent` | named by nothing else, so a resolved agent of `SynthPlayer/1.0` can only have come from the profile below |
| `user_agent` 1002 `Synth Account Agent` | named by the *account*, not by a profile, so the two rungs of the fallback ladder in `sources_for` resolve to different strings and a test can say which one answered |
| `stream_profile` 1001 `Synth Direct` | unlocked, **empty command**, names a user agent. The shipped `ffmpeg` profile also names one, but it carries a command too, so a test using it could not tell "the agent came from the profile" from "the agent came from the command" |
| `stream_profile` 1002 `Synth Account Default` | the account's own default, naming **no** agent, so a stream using it falls through to the account's |
| `output_profile` 1001 `Synth Transcode` | unlocked, so `?output_profile=` and the HDHR `output_profile` scope point at something the API could also have deleted |
| `stream_settings.hdhr_output_profile_id = 1` | the shipped profile, not 1001 — which is what makes "the setting is the fallback and the URL scope wins" two distinguishable answers |
| `network_access = {"M3U_EPG": "127.0.0.0/8"}` | one endpoint class restricted, to loopback: the suite's own peer is admitted and an outside address is not, so both sides of the allowlist are reachable |
| `system_settings.preferred_region = us` | the region bias is a nullable field the Settings page decodes, and a shape whose optional fields are all null pins nothing |

## Providers

| id | account | why |
|---|---|---|
| 1 | `custom` (from the migrations) | holds stream 1000, the hand-added one. A refresh must never touch it and the API must never delete it |
| 1001 | `Synth Standard`, URL **and** a `file_path` that is not on disk, credentials, `max_streams = 2`, server group, its own user agent and stream profile | the ordinary case, and the one carrying the filter and the profiles. It has every optional column filled because the account payload is what the Settings page decodes, and because a configured file that has gone missing has to fall back to the provider rather than fail the refresh |
| 1002 | `Synth Xtream`, `xtream_codes`, credentials, `max_streams = 1` | the catalogue that comes from `player_api.php` rather than a playlist, and the budget that makes the HDHR tuner count and Xtream `max_connections` something other than a default |
| 1003 | `Synth Retired`, **inactive** | the scheduler must register no refresh for it, and its rows must still serve |

| id | provider profile | why |
|---|---|---|
| 1001 | `Synth Standard Default`, `^(.*)$` → `$1` | the identity profile every account gets |
| 1002 | `Synth Standard Rewrite`, `^(.*)/live/(.*)\.ts$` → `$1/hls/$2.m3u8`, `max_streams = 1` | a *real* search/replace with a backreference in the replacement. `$1` → `\1` conversion applies to search patterns only; a fixture whose replacement has no backreference could not catch that being got wrong |
| 1003 | `Synth Xtream Default` | so the Xtream account is not the only one without a profile |

`m3u_filter` 1001 excludes the group `^Synth Adults$` from account 1001's feed,
anchored so it matches one group rather than every group containing the word.

`server_group` 1001 exists because `m3u_account.server_group_id` is a column
the reference instance has zero rows behind, so nothing else reaches it.

## Groups

| id | group | why |
|---|---|---|
| 1001 | `Synth Sports` | the ordinary case |
| 1002 | `Synth News` | has a **number range**, 200–299, which is where auto-sync and a hand-made channel in the group take their numbers from; its account link carries `auto_channel_sync` and the numbering and renaming options the UI writes into `custom_properties` rather than into columns — which is where auto-created channels used to lose the provider's `tvg-chno` |
| 1003 | `Synth Adults` | what the account's filter excludes |
| 1004 | `Synth Empty` | **no streams**: a group that exists because a channel is in it |
| 1005 | `Synth Retired Group` | its account link is **disabled**. Reconcile deletes streams whose group is disabled, which is a different outcome from a group merely absent from one refresh |

## Logos

Three, for the three cases `logos/cleanup/` and the usage count have to tell
apart: 1001 used by exactly one channel, 1002 used by several (including
through an override), 1003 used by **nothing**.

## Guide

| id | source | why |
|---|---|---|
| 1001 | `Synth XMLTV`, URL plus a `file_path` that is not on disk, plus credentials | stored programmes, and the source payload's complete shape — same reasoning as account 1001 |
| 1002 | `Synth Dummy` | generated per request: a channel on it has no `program` rows and still has listings, which is not the same as having nothing on |
| 1003 | `Synth Retired Guide`, **inactive** | no schedule, rows still served |

| id | guide channel | why |
|---|---|---|
| 1001 | `synth.sports`, with an icon | the channel the programmes above hang off |
| 1002 | `synth.news`, **no icon** | the nullable column has to be null somewhere |
| 1003 | `synth.gap` | mapped by a channel, **no programmes at all** |
| 1004 | `synth.dummy` | on the dummy source |
| 1005 | `synth.unmapped` | declared and mapped by nothing: the picker offers it, the programme pass skips it, and it is what the pending match suggestion points at |
| 1006 | `synth.retired` | on the inactive source |

`epg_match_suggestion` holds one row: channel 1002 against guide channel 1005
at 0.62. That is the third outcome of matching — the band upstream resolves
with a model this build deliberately does not carry — and without a row here it
degrades to "no guide" with no explanation.

## Channels

Seventeen. Overrides are applied by the `effective_channel` view, so a channel
is listed by the value an output sees.

| id | channel | why |
|---|---|---|
| 1000 | `Synth One`, number 1 | **three streams across two accounts** in a deliberate failover order: 1001 (standard), 1002 (Xtream), 1000 (hand-added). Two accounts means two stream budgets for `sources_for` to resolve |
| 1001 | `Synth Two & A Half`, number 2.5 | an **override on every overridable column**. Its base row says `Provider Raw Name`, number 99, group `Synth News`, logo 1001, `raw.tvg`, `RAWSTATION`, guide data 1003 and profile 1 — every one of them non-null and every one of them different from the override, so neither half can be mistaken for the other and none may reach an output. Also the **fractional** number |
| 1002 | `Synth Unnumbered`, **number NULL** | SQLite sorts NULLs first on ASC where PostgreSQL sorts them last, so this is the channel that would lead the lineup if the ordering regressed. Also the channel with the pending match suggestion |
| 1003 | `Synth "Quoted" Channel` | a **double quote**, which closes an M3U attribute early and truncates the rest of the line |
| 1004 | `Synth <Angle> & Ampersand` | **angle brackets and an ampersand**, which XMLTV has to escape and M3U does not |
| 1005 | `Synth Ünïcøde Ñoise` | **non-ASCII**, which is where a byte-oriented truncation shows up |
| 1006 | `  Synth Padded  ` | **leading and trailing whitespace**, which a naive trim would silently eat |
| 1007 | `Synth Hidden` | `hidden_from_output`: must appear in the editor and in **no** output |
| 1008 | `Synth Adult` | `is_adult`, and in the group the filter excludes |
| 1009 | `Synth Admin Only` | `user_level = 10` |
| 1010 | `Synth Standard Only` | `user_level = 1`, and `auto_created` |
| 1011 | `Synth Dummy Guide` | mapped to the **dummy** source: no stored programmes, generated listings |
| 1012 | `Synth Gap Guide` | mapped to guide data with **no programmes**: genuinely nothing on. Also, with 1011, the pairing the Channels page reports on — guide data assigned and **no `tvg_id` at all**, which is what auto-matching leaves behind and what a column serialized from the provider's label shows as "no EPG" |
| 1013 | `Synth Redirect` | on the locked `redirect` stream profile, which answers 302 and allocates no ring |
| 1014 | `Synth Streamless` | **no streams**, and the only member of the empty group |
| 1015 | `Synth Catchup Agent` | on `Synth Direct`, so the user agent resolves through a profile; also `is_catchup` with `catchup_days`, and a `tvc_guide_stationid` for the Gracenote `tvg_id_source` |
| 1016 | `Synth Null Url` | its **only** stream has a NULL URL. `sources_for` drops it, so the channel is reachable and unplayable at once — which is what makes switching a stream by *position* pick a different stream than the operator clicked |

## Streams

Twenty-one. One per channel above, plus channel 1000's three, plus three
attached to no channel: 1018 in the group whose account link is disabled, 1019
on the inactive account — a stream the editor can see and no output can reach —
and 1020 in an enabled group on the account the refresh reads, which is what
switching auto-sync on for that group would turn into a channel. The reference
instance has three like 1020: a provider's second copy of a channel already in
the lineup, kept out on purpose. Stream 1017 is the one with a NULL URL.

## Profiles

`All` (id 1) ships with the migrations. `Living Room` (1001) and `Kids` (1002)
are inserted **after** the channels, so the `profile_starts_with_every_channel`
trigger gives each of them every channel enabled; the `UPDATE`s that follow are
what make the membership mixed — `Living Room` carries 1000–1003, `Kids`
carries 1005 and 1006.

Channel 1016 is **deleted** from both rather than disabled in both: "absent
from the profile" and "present and disabled" are different rows and different
query paths, and only one of them is what a user gets by removing a channel.

`synthstandard` is restricted to `Living Room` through `user_channel_profile`.
With no rows there a user sees every profile, so this is the restricted case
the Xtream catalogue narrows on.

## Jobs and events

One job per state the scheduler can leave behind — `idle`, `running`,
`success`, `failed`, `cancelled` — including `m3u_refresh:1002` left
**`running`** by a process that is gone. The two `success` rows carry a message,
a `last_success_at` and a `next_run_at`, because the provider and guide payloads
report `status`, `progress`, `last_message` and `updated_at` *from the job* and
a source whose job never ran reports null for all four. Without `release_orphans` at boot, the
single-flight guard refuses that key forever, which is a failure that survives
a restart and looks like nothing at all.

`last_success_at` is when the unit last had good data, not when it last
stopped, so the `failed` and `cancelled` rows carry a time from the *day
before* the run that is recorded on them: the Sources page shows it as
"Refreshed", and a provider failing nightly must not read as refreshed an hour
ago. `m3u_refresh:1002` is running with none at all, which is the only shape
that should say *Never* while something is in flight.

Four `system_event` rows, two of which leave `channel_uuid`/`channel_name`
null and one of which fills them, so the nullable columns are populated on both
sides.

## Notifications

The operator's inbox — the conditions a background job hit that somebody has to
act on — as distinct from the `system_event` rows above, which record that
something *happened*. Three rows, chosen so every state the list can be in is
reachable without a test raising one first, and so the response shape has no
column pinned as null:

| id | state | why |
|---|---|---|
| 1001 | recurring, unacknowledged | `occurrences = 12`: the nightly refresh has found the same uncompilable filter on `Synth Retired` since August. The row the count column exists for, and the one the Notifications page has to render as "seen 12 times" rather than twelve rows |
| 1002 | recurring, **acknowledged** | the only row filling `acknowledged_at`, which is what pins that column as a string rather than as null in the shape snapshot. It is also the property `raise` has to hold: an acknowledged condition that recurs stays acknowledged, or the bell can never be emptied and the operator learns to ignore it |
| 1003 | fresh | one occurrence, never seen, `severity = info` — the server declined to delete a channel's last stream and is saying so. Not every notification is a fault, and a page that renders them all as red says nothing |

The three `kind`s are the three conditions the code detects: `m3u.filter_broken`
and `auto_sync.range_full` are wired to producers in `api::ingest::m3u`;
`m3u.streams_kept` is still log-only — its row is here so the list, the ordering
and the severity rendering are testable before its producer exists.

`subject` keeps two accounts with the same fault apart, and `(kind, subject)`
is the unique index that makes a recurrence an update rather than a new row.

## Providers, as files

`providers/` holds hand-written provider payloads, driven through the real job
handlers on this seed. `fixtures/ingest/` is a two-run capture of the reference instance's
own provider — real bytes, one provider, one way of writing a playlist — and it
stays as the bootstrap evidence. These are the ways the *other* providers write.

**They are read from disk, not fetched.** `allow_private` is false for every
provider feed, because an EPG or M3U URL is configuration whose *content* the
provider controls, and `http::get` judges a bare address before dialling it —
so the loopback a mock server runs on is refused at hop zero. A file-backed
source is a real, supported configuration and reaches the same reconciler; what
it cannot exercise is the transport, and
`provider_fetches_cannot_reach_loopback` pins why.

| file | what it is for |
|---|---|
| `playlist.m3u` | one entry per attribute form: quoted and unquoted values, `#EXTGRP` instead of `group-title`, `#EXTVLCOPT`/`#KODIPROP` between an `#EXTINF` and its URL, an entry with no attributes at all, `channel-number` as the other spelling of `tvg-chno`, catch-up in both spellings, VLC's `udp://@` multicast syntax, a quote and non-ASCII in a name, the same URL listed twice, an `#EXTINF` with no URL under it, an entry in the group account 1001's filter excludes, and one new entry in the auto-synced group. The first four URLs are the seed's own, verbatim, so a refresh has to resolve them to the rows that exist rather than reporting four new streams and expiring four old ones |
| `xtream-playlist.m3u` | the same account's catalogue as a *file*, which is the only way an Xtream refresh reaches the reconciler in a test — `catalogue` fetches, and a provider fetch cannot reach a mock server. The load-bearing attribute is `stream_id`: `StreamFields::from_entry` reads it into `provider_stream_id`, and that is what `sync::hash` substitutes for the URL on an Xtream account. The first entry is the seed's own Xtream stream, verbatim |
| `xtream-get-live-categories.json` | three categories, one of which no stream is in |
| `xtream-get-live-streams.json` | a numeric `stream_id` beside a quoted one, a `num` that is a string and one that is fractional, an entry with no `name`, one whose name is whitespace, one in a category id nothing declares, and one with no `stream_id` at all — which is skipped rather than given a fabricated id, because a made-up id collides with a real one on the next refresh and merges two streams into one row |
| `guide.xml` | an entity reference in a title, a channel with several `<display-name>`s, a channel with no icon, a guide channel nothing maps to, a channel new to this feed, and three timestamp forms: UTC, a negative offset, and no offset at all |
| `guide.xml.gz`, `guide.xml.xz` | the same document, byte for byte, in the two compressions providers serve interchangeably. Neither the URL extension nor the `Content-Type` says which, so the encoding is sniffed from the leading bytes and a test that only ever sees plain text never reaches the sniffing |
| `guide-utf16.xml` | UTF-16LE with a BOM, which is what a Windows-authored export is. Both parsers assume UTF-8 and a UTF-16 document parses as an *empty* guide — which reads as "the provider published nothing today" and replaces a working guide with nothing, so it is refused by name instead |
| `guide-truncated.xml` | `guide.xml` cut **inside a tag**, which is what a connection dropping mid-download leaves. A cut at an element boundary is a different case and is generated in the test: it is still well-formed as far as the reader is concerned, so it is accepted as a shorter guide — a gap the test pins rather than leaves unsaid |

The credential rotation is not a second pair of files. Through
`player_api.php` the payload does not change when a provider rotates — the
*account* does — so the test runs the same two JSON files under two
username/password pairs and asserts every dedup hash holds while every URL
changes. Through the playlist the credentials are in the URLs, so the test
rewrites them itself and refreshes a second time, asserting the same thing about
rows: same ids, same hashes, same failover list, every URL different. A standard
account in the same position orphans all three, which is what makes the
substitution a choice rather than an accident.
