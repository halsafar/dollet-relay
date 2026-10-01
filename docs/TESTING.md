# Testing

The rules the suite is held to, and why each one exists.
[`ARCHITECTURE.md`](ARCHITECTURE.md) is the reasoning behind the design; this
is the reasoning behind the evidence.

## The lesson these come from

Every significant defect on this project arrived through a test that looked
like it checked something and was actually coupled to something incidental. A
green test asserting that a data wipe was correct behaviour. Full coverage over
lines that proved nothing. A corpus a byte-diff called verified, because the
data contained none of the hard cases. A fixture whose stream hashes were
computed the wrong way, in the file written to catch exactly that.

None of those were caught by writing more tests. They were caught by changing
what a test is allowed to rest on. That is what the rules below are.

## Axioms

**1. A test written for a fix is shown to fail without the fix.** Revert the
fix, watch the test fail, restore it. A test that passes both ways is asserting
something incidental, and it will keep passing while the thing it names breaks.

**2. No test may pass only because the data lacks the case.**
`fixtures/sample.sql` is one instance and its shape is not the product's.
`/output/epg?days=4000000000` panicked in the dummy-EPG generator and no test
noticed, because the sample has no dummy source so the generator was never
reached; `stream_settings.hdhr_output_profile_id` was documented and unread,
because no channel in the sample uses an output profile. Every supported shape
lives in `fixtures/synthetic/instance.sql`, with a line in its README saying
why. Add a missing shape there rather than building it inside a test.

**3. Output snapshots are compared whole.** The four surfaces under
`fixtures/golden/` are diffed as entire documents, and nothing tolerates a
known difference. A tolerated difference is a place a new difference can hide,
which is the same failure as an over-specified assertion wearing a better
disguise. A deliberate output change is re-pinned with the ignored `repin`
tests and reviewed as a diff, because that diff *is* the change to what every
client receives:

```text
cargo test -p dollet-core --test golden -- --ignored repin   # dummy blocks, checksums
cargo test -p dollet-server repin -- --ignored               # everything the router serves
INSTA_UPDATE=always cargo test -p dollet-core --lib output   # the serializer unit snapshots
```

**4. Coverage is a ratchet, never a target.** It may not decrease. There is no
percentage to hit, except one list that must be 100%: the M3U parser, the XMLTV
parser, the failover state machine, and the four serializers, pure functions
where total coverage is cheap and bugs are expensive. Coverage says a line ran.
Every defect the reviews here have found was inside a line that coverage
already called covered.

**5. Fixtures are guarded by a scan, not by care.** Every fixture is edited by
hand, and hand editing is the moment a real URL gets pasted in from a browser.
So a test fails if a host outside the allowlist, a credential-shaped path
segment, or a high-entropy token appears in any fixture, including inside
compressed and base64 payloads, which a scan of the raw bytes would walk
straight past. An allowlist rather than a search for known secrets: a denylist
has to name the provider's hostname in order to look for it, which puts the
very string it guards against into the repository.

**6. A layer may only rest on what its row below allows.** A test that reaches
past it proves less than its name claims.

## Guardrails: what each layer may depend on

| Layer | Seeds from | May depend on |
|---|---|---|
| Pure functions (`parse`, `output`, `sync`, `settings`, `auth`, `http`) and the streaming engine | inputs built in the test; the engine generates its own transport stream | nothing outside the test |
| Wire-format snapshots (`fixtures/golden`, `tests/golden.rs`) | the sample instance | the four output formats' contract with clients, which is the point |
| Migration fidelity (`fixtures/import`, `importing_the_backup_writes_the_expected_rows` in `api/tests.rs`) | one Dispatcharr backup zip, imported and read back | Dispatcharr's schema, the one place another product's shape is a legitimate dependency |
| Ingest reconciliation (`fixtures/ingest`, `tests/ingest.rs`) | a hand-written two-run feed | one feed's shape |
| Integration: every HTTP handler, the response-shape snapshots (`api/tests.rs`) | `fixtures/sample.sql` or `fixtures/synthetic/instance.sql` | **not** the sample instance's shape; see axiom 2 |
| Web (`web/src/**/*.test.jsx`) | mocked API responses; `web/route-manifest.json` ties the paths it calls to the server | nothing |
| Browser end-to-end (`web/e2e`, `scripts/e2e.sh`) | `fixtures/synthetic/instance.sql` through `dollet seed`, plus an empty instance beside it | a real browser and a real server, which is the point |

The last row is the one nothing else stands in for: it proves the *built* SPA,
served by the real fallback, works in a real browser. Both of the most recent
user-found bugs lived exactly there: a hard refresh on `/settings` downloaded
the page, because the fallback typed itself from the request, and the Network
Access section rendered nothing for `{}`, with a jsdom test asserting that was
correct. `scripts/smoke.sh` is the same idea one layer down: it asks a built
*image* for every client route, and `scripts/release.sh` runs it before any
push. Neither is inside `scripts/test.sh`, because the builder image has no
browser and a browser run is too slow to precede every `cargo test`; CI runs
the browser job as a second stage after the gate.

## What the evidence is

- **Every output surface is pinned byte for byte**: the M3U, the XMLTV across
  116 programmes, the HDHR payloads and the Xtream actions, both through the
  serializers alone and through the router over a seeded database.
- **Ingest is pinned row for row** against a two-run feed that covers
  reconciliation rather than just import: unchanged entries touched rather than
  recreated, a rotated URL becoming a new identity with the old row marked
  stale, vanished entries marked rather than deleted.
- **The migration is pinned against Dispatcharr's own schema**, a database in
  their shape, in the archive format their backup carries, down to the
  report's warnings and pattern outcomes.
- **The streaming engine runs against a synthetic transport stream**: failover,
  backoff, the shutdown delay, rings, cursors and eviction, with the memory
  formula checked by a soak harness.
- **The composition between them is where the defects were.** The serializers
  were proven with channel lists reconstructed *from the snapshots*, which said
  nothing about whether our queries produce that list. The end-to-end tests
  close that, and caught three real bugs on their first run.

## The synthetic instance

`fixtures/synthetic/instance.sql` is one hand-written, deterministic seed, with
a README beside it saying **why each row exists**; a row nobody can explain is
a row that will be deleted the next time the file is tidied. Ids start at 1000
so nothing collides with `sample.sql`. Everything below must be present, so a
test can reach any supported shape without building it first.

**Accounts.** The seeded `custom` account with one hand-added stream. A
`standard` M3U account with a URL, a filter that excludes a group, and a profile
with a search/replace pattern. An `xtream_codes` account with server URL,
username, password and `max_streams = 1`. One inactive account.

**Guide.** An XMLTV source with `epg_data` rows and programmes spanning *now*:
one ending exactly now, one starting exactly now, an overlap, a gap. A `dummy`
source. An inactive source. One `epg_match_suggestion` row.

**Channels.** Integer, fractional and `NULL` numbers; `hidden_from_output`;
`is_adult`; `user_level` 10 and 1; `auto_created`; catch-up flagged; one with
an override on every overridable column; one with three streams across both
accounts in a deliberate failover order; one with no streams; one whose only
stream has a `NULL` URL; one on the dummy source; one mapped to guide data with
no programmes; one on the `redirect` profile; one on a profile naming a user
agent. Names carrying quotes, ampersands, angle brackets, non-ASCII, and
leading or trailing whitespace.

**Profiles.** `All` plus two more with mixed enabled and disabled membership,
and one channel in neither.

**Users.** An admin. A standard user with `xc_password`, an API key,
`stream_limit = 1`, and a restriction to one channel profile. A streamer. An
inactive admin, the "setup cannot be reopened" case.

**Groups.** Five: one with a disabled account link, one with
`auto_channel_sync` and numbering options in `custom_properties`, one with no
streams.

**Logos.** Used by one channel, by several, by none.

**Settings.** `hdhr_output_profile_id` set to a seeded output profile;
`network_access` restricting one endpoint class to a CIDR; a non-UTC
`time_zone`.

**Jobs and events.** A job in each state, including one left `running` by a
previous process; a handful of system events.

**Providers.** `fixtures/synthetic/providers/` holds feeds served through the
real job handlers: an Xtream provider with string and numeric ids, a missing
name, an empty category and a credential rotation between runs; an M3U with
every attribute form and a group the filter excludes; XMLTV as gzip and xz; a
UTF-16 file; a truncated file; a provider that fails mid-refresh.

## Coverage

The ratchet has two halves, enforced differently.

**Web** is enforced: the thresholds in `web/vite.config.js` fail `npm run
test`, which `scripts/test.sh` and CI both run. Raise them when coverage rises;
never lower them to make a build pass.

**Rust** is checked by hand, because `cargo llvm-cov` needs `--local` and CI
runs the plain gate: `scripts/test.sh --local --coverage` before and after a
body of work, with the `TOTAL` line compared against the floor below. The
numbers are not evidence that anything is *right*, that is what the snapshots
are for, only that nothing stopped being reached. Measure in a throwaway
worktree when comparing against another commit, so the branch under test never
leaves the working tree.

The floor, from `cargo llvm-cov --workspace --all-features --summary-only`.
Replace the row when it rises:

| Commit | Date | Regions | Functions | Lines |
|---|---|---|---|---|
| `6d8ab0a` | 2026-10-01 | 92.18% | 95.25% | 95.08% |

Of the must-be-100% list, the M3U parser, the XMLTV parser, and the M3U, HDHR
and Xtream serializers are at 100% region, function and line. Three are not,
and the gap is the work, not a tolerance: the XMLTV serializer at 99.15%
regions, the failover state machine at 98.48%, and `sync/epg.rs` at 99.61%.
Every other file under `sync/` is at 100%.
