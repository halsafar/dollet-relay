# Fixtures

A test here needs up to three things: a **seed** (a database in this project's
schema), an **input** (something the product reads), and an **expectation**
(what it should store or serve). This directory holds all three, grouped by
the flow they test.

| Path | Role | What it is |
|---|---|---|
| `sample.sql` | seed | the reference instance: twelve channels in four groups, shaped like a provider's, plus the hard cases the snapshots depend on |
| `synthetic/instance.sql` | seed | the designed instance: every shape the product supports, so no test has to build one |
| `synthetic/providers/` | input | the M3U, XMLTV and Xtream feeds that seed points at |
| `ingest/` | input + expectation | a provider's playlist and guide over two refreshes, and the rows a sync must store |
| `golden/` | expectation | the bytes the four outputs serve from `sample.sql`: M3U, XMLTV, HDHomeRun, Xtream Codes |
| `import/dispatcharr-backup.zip` | input | one Dispatcharr backup, exactly as a user hands it to the importer |

Each directory's README says what its rows are for and which test checks
them.

## Everything here is hand-kept

Nothing is generated. Every host is `*.example` or `ipx.test`, every
credential is `fixtureuser` / `fixturepass`, every name is an invented word —
and the leak scans in `crates/dollet-core/tests/{fixtures,golden,ingest}.rs`
fail if any file mentions a host outside that list, because hand editing is
exactly when a real URL gets pasted in.

The fixtures are small on purpose: a test proves a shape, not a volume, and a
corpus of easy rows proves only the easy path. Each directory's README lists
the hard cases it carries and why.

To cover a new shape, add rows to `synthetic/instance.sql` with a line in its
README saying why.

## The Dispatcharr backup

The one binary here, and the only place another product's shape appears. The
tests import it and read values back; nothing compares it to an expectation
file, and nothing else in the tree depends on it. `import/README.md` says what
is in it and how to change it, which needs a throwaway PostgreSQL because the
archive format is `pg_dump`'s.

## Re-pinning the outputs

A deliberate change to what a client receives is re-pinned and reviewed as a
diff, because that diff *is* the change:

    cargo test -p dollet-server repin -- --ignored
    cargo test -p dollet-core --test golden -- --ignored repin
