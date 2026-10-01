# Ingest reconciliation corpus

What a refresh must **store**, as opposed to what the outputs serve.
`fixtures/golden/` covers the output direction; this covers the input one, and
the difference matters: an output bug produces a visibly wrong byte, while an
ingest bug silently corrupts the catalogue. A stream recreated instead of
updated loses every channel assignment hanging off it, and the guide served
afterwards looks perfectly well-formed.

Everything here is hand-written. The provider is `provider.example`, the
credentials are the fixture's, and no real provider is ever contacted.

## What is here

| File | |
|---|---|
| `provider-run1.m3u`, `provider-run2.m3u` | one account's playlist on two consecutive refreshes |
| `provider-epg.xml` | the guide, with `@T0@`..`@T3@` placeholders the harness stamps with fixed instants |
| `provider-filtered.m3u` | a second account, stream ids 2001–2014, for the filter and auto-sync cases |
| `run1-streams.tsv`, `run2-streams.tsv` | the stream rows after each refresh: hash, name, URL, `tvg_id`, group, number, catch-up, stale, logo |
| `channels.tsv` | the channels created from run 1's streams: name, `tvg_id`, matched guide id, number |
| `epg-data.tsv`, `programmes.tsv` | the guide rows: channels, and programmes with their `custom_properties` |
| `fuzzy-scores.tsv` | the best fuzzy score for every channel the guide cannot match |
| `filters.tsv` | the second account's rules, in the order they apply |
| `filtered-streams.tsv` | the entries those rules let through |
| `autosync-run1-channels.tsv`, `autosync-run2-channels.tsv` | the channels auto-sync creates, after one refresh and after two |

The second account has its own stream ids on purpose: the hash key is `url`
alone, so two accounts serving the same URL would land on the same rows and the
reconciliation corpus would move under the filter corpus.

## The source is deliberately hard

Thirteen entries chosen the way the output corpus's adversarial rows were, and
for the same reason — a corpus made of easy cases proves only that easy cases
work:

| Entry | What it exercises |
|---|---|
| `Alpha HD` + `Alpha Duplicate` | the same URL twice: dedup within one run |
| `Charlie Unquoted` | every attribute unquoted |
| `Delta, With Comma` | an unquoted value beside quoted ones, and a comma in the title |
| `Echo "Quoted" & Ampersand` | a quote and an ampersand in a name |
| `Foxtrot Number Collision` | `tvg-chno` colliding with another entry |
| `Golf Vanishes Next Run` | disappears in run 2 — staleness |
| `Hotel URL Changes` | URL rotates in run 2 — a new identity |
| `India Name Changes` | name changes, URL does not — an update in place |
| `Juliet Catchup` | `catchup` / `catchup-days` / `catchup-source` |
| `Kilo No Tvg Id` | no `tvg-id`, no logo |
| `Lima Café Über` | non-ASCII, in a second group |
| `November` | appears only in run 2 |

The guide adds an HTML entity (`&eacute;`, `&nbsp;`, `&mdash;`, `&uacute;`), a
raw `&` inside an otherwise valid description, markup characters in a title, a
timestamp with no offset, a programme with **no stop time**, a channel no stream
maps to, a programme for a channel the guide never declares, and a programme
with two `<title>`s in different languages — the first is stored, whatever its
`lang`, which is what `Alpha Matin` pins.

## Two runs, not one

`run1-streams.tsv` is the initial import; `run2-streams.tsv` is the same account
re-synced against the changed playlist. The second is what proves
reconciliation, and a single-run corpus cannot see it:

| Outcome | Row |
|---|---|
| unchanged → touched, not recreated | nine entries |
| changed name, same URL → **updated in place** | `India Name Changes` → `India Renamed` |
| changed URL → new identity, old marked stale | `Hotel URL Changes` appears twice, one stale |
| vanished → **marked stale, not deleted** | `Golf Vanishes Next Run` |
| appeared | `November Appears This Run` |

Marked rather than deleted is the distinction the whole module exists to
preserve: a provider serving a truncated playlist for an hour costs nothing.
Deletion waits for `stale_stream_days`, and the harness checks the far side of
that boundary too, which two runs an hour apart cannot reach.

## Filters and auto channel sync

`filters.tsv` is the second account's rule set, in the order it applies. Order
is the whole semantics: the first rule that matches decides, and a rule after a
broad exclude is dead.

| # | Pattern | | What it is for |
|---|---|---|---|
| 0 | `^Foxtrot` | include | pins Foxtrot **before** the broad exclude below |
| 1 | `^Fox` | exclude | dead for Foxtrot, live for Foxglove |
| 2 | `^Kilo` | exclude | a plain exclude |
| 3 | `(?<=Mike )Second` | exclude | lookbehind — rejected by Rust's `regex` crate, which is why user patterns compile with `fancy_regex` |
| 4 | `(m)\1` | exclude | a backreference; it finds the `mm` in `Delta, With Comma` |
| 5 | `(\w+) $1` | exclude | a JS-style backreference in a **search** pattern |

`filtered-streams.tsv` is what survives: nine of fourteen.

### `$1` in a search pattern

Rule 5 is written in the JavaScript dialect, where `$1` is a backreference. In
a search pattern `$` is an anchor, so compiled as written the rule can never
match anything and `Papa Echo Echo Repeat` would survive a rule written to
exclude it. `regex_compat::js_backrefs_to_rust` rewrites `$1` to `\1` before
compiling — search patterns only, never replacement templates, whose syntax is
already `$1` — so the rule does what its author meant. The harness asserts
that the rule is inert as written, live through the rewrite, and that it costs
exactly that one stream.

### A rule that will not compile

Adding `(unclosed` as a seventh rule does not take the refresh down: the rules
that did compile still apply, the same nine streams survive, and
`sync::filters::compile` reports the broken one by index, pattern and reason so
the UI can say something. Dropping the whole set on one typo would change which
streams exist, which is indistinguishable from the provider changing its
lineup.

### Numbering and naming

`provider` mode, fallback `100`, range end `103`, rename `^(\w+) (.*)$` →
`$2 [$1]`. The existing lineup already holds 1–5 and 8–14, so 6 and 7 are the
only low numbers free. Nine streams, six numbers, allocated in playlist order:

| Stream | Provider # | Got | Why |
|---|---|---|---|
| `Foxtrot Number Collision` | 1 | **100** | taken → first free in the fallback range |
| `Alpha Kept` | 1 | **101** | taken, and 100 has just gone |
| `Bravo Kept` | 200 | **200** | outside 100–103 and still honoured |
| `Charlie Kept` | — | **102** | unnumbered → fallback |
| `Delta Kept` | — | **103** | the last slot |
| `Echo`/`Golf`/`Hotel Kept` | — | *none* | **exhausted → no channel at all** |
| `Oscar Kept` | 6 | **6** | free → the provider's own number wins, even after the range is exhausted |

Exhaustion produces *no channel*, not a wrapped or colliding number. That is the
behaviour worth pinning: a renumbered channel is one the user's recordings and
favourites no longer point at.

The corpus sets `auto_sync_channel_start` and `channel_numbering_fallback` both
to `100`, so it does not distinguish them.

### Does a second sync double the lineup?

**No.** `autosync-run1-channels.tsv` and `autosync-run2-channels.tsv` are
identical *including the row ids*: a second refresh leaves auto-created
channels alone rather than deleting and recreating them. The ids are the
requirement: same names and numbers with new ids would still be a rebuild, and
a rebuild loses every stream assignment and manual edit hanging off the
channel.

Picking a number and a name is pure and tested here; deciding that a stream
already *has* an auto-created channel is a query, made in the refresh handler
and tested with it in `crates/dollet-server`. The two files state the
requirement on that handler.

## Behaviours this corpus pins

- **An empty hash key collapses a playlist.** `m3u_hash_key` empty selects no
  fields, so every stream hashes `sha256('{}')` and a refresh would fold the
  catalogue into one row. An imported instance can carry exactly that setting,
  which is why the refresh refuses to write in that state; the collapse it
  refuses is pinned here.
- **Unquoted attributes are read.** A scanner that requires quotes drops them
  without a word, and the stream lands in the default group with no id and no
  number. `Charlie Unquoted` and `Delta, With Comma` land in `Adversarial` with
  their ids and numbers.
- **Catch-up is read in both spellings**, `catchup`/`catchup-days` and
  `tv_archive`/`tv_archive_duration`. Catch-up is out of scope for 1.0 and no
  output advertises it, but the row is right.
- **A programme with no stop time is skipped and counted** on
  `XmltvReader::skipped()`: it cannot be placed on a grid, and a guide quietly
  losing programmes has to be visible somewhere.
- **A programme for an undeclared channel is yielded by the parser** and stored
  by nobody, because there is nothing to resolve it to.

## What the harness proves

`crates/dollet-core/tests/ingest.rs`, 20 tests, no database.

- **Hashing**: all 26 rows across both runs reproduce the stored `stream_hash`
  exactly — the shape the importer carries across verbatim — plus the empty-key
  collapse above.
- **Run 1**: the `Plan` inserts exactly the twelve expected hashes, and counts
  the duplicate rather than swallowing it.
- **Run 2**: all four reconciliation outcomes, by name and by hash, and the
  delete-versus-mark boundary on the other side of the retention window.
- **Guide**: channels, programmes, timestamps, `custom_properties` and the
  entity and non-ASCII handling all match the expected rows.
- **Matching**: the fuzzy score for every unmatched channel, to within 1e-9 —
  the scorer decides where a channel lands on the threshold ladder, so a drift
  would move channels across bands without any test of the ladder noticing.
- **Filters**: the nine survivors match rule for rule, the `$1` rewrite is
  shown to be the difference, and a broken rule costs one rule rather than the
  set.
- **Auto sync**: every number and every renamed name, including the three
  streams that got no channel because the range ran out.

### Verified by mutation

| Mutation | Result |
|---|---|
| drop the space in the hashed JSON's separators | caught |
| recreate instead of updating in place | caught |
| delete instead of marking stale | caught |
| break the HTML entity table | caught |
| stop counting skipped programmes | caught |
| last matching filter wins instead of the first | caught |
| stop rewriting `$1` in a search pattern | caught |
| one broken rule drops the whole set | caught |
| the provider's number wins even when taken | caught |
| an exhausted range wraps instead of giving up | caught |
| the fallback ignores its start and runs from 1 | caught |
| convert `$1` in the *replacement* template too | caught |

## Guarding a synthetic corpus anyway

`the_ingest_corpus_references_no_unexpected_host` scans every file here for a
host outside `provider.example` and `ipx.test`. It passes trivially, which is
the point: a guard that never runs is a guard that rots, and this one is in
place before anyone pastes a real provider's URL into the corpus.
