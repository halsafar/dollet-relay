# Dispatcharr import

`dispatcharr-backup.zip` is one Dispatcharr backup — `database.dump` in
`pg_dump -Fc` format plus a `metadata.json` declaring format version 2 — which
is exactly what a user hands `dollet import --backup`. It is the only
Dispatcharr artefact in the repository, and the importer is the only thing that
reads it.

The tests import it into an empty database and read back what they expect to
find; there is no expectation file. They are the `importing_the_backup_*` and
`the_imported_*` tests in `crates/dollet-server/src/api/tests.rs`, plus the
reader's own tests in `crates/dollet-core/src/parse/pgdump.rs`.

## What is in it

Twelve channels in four groups, 20 streams (17 linked to channels, three that
no channel plays), 9 logos, 11 guide channels (8 mapped, 3 not) with 240
programmes, one M3U account beside the locked `custom` one, two users. Every
host is `*.example` or `ipx.test`, every stream path is
`/live/fixtureuser/fixturepass/<id>`, every name is an invented word, and
`the_backup_fixture_references_no_host_outside_the_allowlist` reads the dump
through the project's own parser on every run to check that.

Rows at ids from 900 exist because a clean install leaves those tables empty,
and an empty table proves nothing about its mapping:

| Row | What it makes testable |
|---|---|
| a server group | a mapping with no rows at all otherwise |
| `(?<=US: )Meridian` | lookbehind — compiles only because user patterns use `fancy_regex` |
| `(\w+) $1` | a JS-style backreference in a *search* pattern, reported as rewritten |
| `(unclosed` | a pattern that will not compile, reported and kept rather than dropped; the CLI exits 1 on it, deliberately |
| a channel override on 171 | the `effective_*` coalesce every output query sorts on |
| a second user | an API key, a non-admin `user_level`, a `stream_limit`, `custom_properties` |
| a channel profile with 173 disabled | per-user scoping, and that the seeded memberships do not overwrite it |

Channels 171, 213 and 214 carry failover streams (two, three and three), so
`channel_stream.sort_order` has an order to preserve.

## Changing it

Rare: the importer reads format version 2 and nothing else, so this file
changes only if that does. The archive format is `pg_dump`'s and only
`pg_dump` writes it, so editing means a throwaway PostgreSQL:

    unzip dispatcharr-backup.zip
    podman run -d --name pg -e POSTGRES_PASSWORD=x -e POSTGRES_USER=dispatch \
      -e POSTGRES_DB=fixture -v "$PWD:/w:z" docker.io/library/postgres:17-alpine
    podman exec pg pg_restore -U dispatch -d fixture --no-owner --no-acl /w/database.dump
    # edit through `podman exec -it pg psql -U dispatch -d fixture`, then:
    podman exec pg pg_dump -U dispatch -d fixture -Fc --no-owner --no-acl > database.dump
    zip dispatcharr-backup.zip database.dump metadata.json
    podman rm -f pg

The restored database carries no foreign keys, so a row left pointing at a
deleted parent is not refused there — the importer refuses it instead, with a
warning per row, which is what `the_report_names_what_it_could_not_carry`
would then report. Then update the counts the tests read back.
