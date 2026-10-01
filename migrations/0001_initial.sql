-- Initial schema for the 1.0 in-scope feature set.
--
-- Timestamps are TEXT in exactly one format:
-- `YYYY-MM-DD HH:MM:SS.mmm+00:00`, produced by the column defaults below and
-- by `db::sql_timestamp` on the Rust side. This matters because these columns
-- are compared and ordered as text: sqlx's own `DateTime<Utc>` encoding uses a
-- `T` separator, and 'T' sorts after ' ', so mixing the two silently inverts
-- `ORDER BY` within a single day.
--
-- Out-of-scope tables (VOD, DVR/recordings, catch-up/timeshift, plugins,
-- Schedules Direct, Connect webhooks, backups) are deliberately absent rather
-- than present-and-empty: the source instance has zero rows in every one of
-- them, and an empty table invites code that pretends the feature exists.
-- `is_catchup` / `catchup_days` are the exception — cheap columns carried from
-- the first migration so adding catch-up later is not a backfill from JSON.
--
-- Tables are declared in dependency order. Booleans are 0/1 integers with a
-- CHECK, because SQLite will otherwise happily store 'yes'.

-- ---------------------------------------------------------------- accounts

CREATE TABLE user (
    id                INTEGER PRIMARY KEY,
    username          TEXT NOT NULL UNIQUE COLLATE NOCASE,
    email             TEXT,
    -- Django's `pbkdf2_sha256$<iterations>$<salt>$<b64hash>`, stored verbatim.
    -- The importer copies it across unchanged and new passwords are written in
    -- the same shape, so there is exactly one verification path.
    password          TEXT NOT NULL,
    is_active         INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    -- 0 streamer, 1 standard, 10 admin. Numeric so `>=` comparisons work.
    user_level        INTEGER NOT NULL DEFAULT 0,
    -- Clients authenticate with this instead of a JWT. Losing it on migration
    -- breaks every configured client, so it is preserved verbatim too.
    api_key           TEXT UNIQUE,
    -- 0 means unlimited.
    stream_limit      INTEGER NOT NULL DEFAULT 0 CHECK (stream_limit >= 0),
    custom_properties TEXT NOT NULL DEFAULT '{}',
    avatar_config     TEXT NOT NULL DEFAULT '{}',
    first_name        TEXT NOT NULL DEFAULT '',
    last_name         TEXT NOT NULL DEFAULT '',
    last_login        TEXT,
    date_joined       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')
);

-- ------------------------------------------------------------------- core

CREATE TABLE user_agent (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
    user_agent  TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    is_active   INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')
);

-- How a channel's upstream bytes are obtained. An empty `command` means the
-- URL is proxied directly; the locked `redirect` profile is answered with a
-- 302 and never proxied at all.
CREATE TABLE stream_profile (
    id            INTEGER PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE COLLATE NOCASE,
    command       TEXT NOT NULL DEFAULT '',
    parameters    TEXT NOT NULL DEFAULT '',
    -- Shipped with the product; the API refuses to edit or delete these.
    locked        INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0, 1)),
    is_active     INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    user_agent_id INTEGER REFERENCES user_agent(id) ON DELETE SET NULL
);

-- A transcode between the channel ring and the client, selected per request
-- via `?output_profile=<id>`. That is why the streaming engine is keyed by
-- `(channel, output_key)` rather than by channel.
CREATE TABLE output_profile (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    command    TEXT NOT NULL DEFAULT '',
    parameters TEXT NOT NULL DEFAULT '',
    locked     INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0, 1)),
    is_active  INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1))
);

-- Settings are grouped JSON blobs rather than one row per key so the Settings
-- page saves a whole section atomically. That is also how upstream stores
-- them, so the importer reads the same shapes.
CREATE TABLE core_setting (
    -- Surrogate id as well as the key, because the Settings page addresses a
    -- section by id the way upstream's API does.
    id    INTEGER PRIMARY KEY,
    key   TEXT NOT NULL UNIQUE,
    name  TEXT NOT NULL,
    value TEXT NOT NULL DEFAULT '{}'
);

-- The JWT signing key, generated on first start and kept out of `core_setting`
-- so no present or future "list all settings" endpoint can serve it.
CREATE TABLE instance_secret (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    jwt_secret TEXT NOT NULL
);

CREATE TABLE system_event (
    id           INTEGER PRIMARY KEY,
    event_type   TEXT NOT NULL,
    occurred_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    channel_uuid TEXT,
    channel_name TEXT,
    details      TEXT NOT NULL DEFAULT '{}'
);

-- Conditions a background job hit that the operator has to do something about.
--
-- Distinct from `system_event`, which is an append-only log of things that
-- happened: an event is "a refresh ran", a notification is "this is still
-- wrong and nobody has looked". The operator meets these conditions today as
-- "channels are missing from Plex" a week later, because the only place a full
-- group range or an uncompilable filter is recorded is the log.
--
-- `(kind, subject)` is the dedup key. A nightly refresh that finds the same
-- broken filter every night must leave one row with a count on it, not thirty
-- rows nobody reads; `subject` is what the condition is *about*
-- (`account:2`, `group:7`) so two accounts with the same fault stay apart.
--
-- `severity` is validated in Rust rather than by a CHECK. SQLite cannot ALTER
-- a CHECK, the documented workaround is a table rebuild, and its first step is
-- a no-op inside the transaction sqlx runs migrations in -- so a constraint
-- here would make adding a fourth severity a hazard rather than an edit.
CREATE TABLE notification (
    id              INTEGER PRIMARY KEY,
    -- A stable machine key: `m3u.filter_broken`, `auto_sync.range_full`. The
    -- UI links from it, so it is not a sentence.
    kind            TEXT NOT NULL,
    subject         TEXT NOT NULL DEFAULT '',
    -- `info` | `warning` | `error`.
    severity        TEXT NOT NULL,
    title           TEXT NOT NULL,
    message         TEXT NOT NULL,
    -- Producer-specific ids and counts, for the UI to link from.
    detail          TEXT NOT NULL DEFAULT '{}',
    occurrences     INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    -- Null while it still wants attention. A recurrence deliberately does not
    -- clear it: a condition the operator has seen and decided to live with
    -- must not re-announce itself on every refresh.
    acknowledged_at TEXT
);

-- Scheduler state, so a refresh that came due while the process was down still
-- runs, and a job in flight at shutdown is visibly abandoned rather than
-- silently lost. There is no external scheduler to hold this; the table is it.
CREATE TABLE job (
    id               INTEGER PRIMARY KEY,
    -- Single-flight key, e.g. `m3u_refresh:2`. One row per schedulable unit.
    key              TEXT NOT NULL UNIQUE,
    kind             TEXT NOT NULL,
    payload          TEXT NOT NULL DEFAULT '{}',
    interval_seconds INTEGER CHECK (interval_seconds IS NULL OR interval_seconds > 0),
    -- No `pool` and no `enabled`. Upstream routes long work to a second Celery
    -- queue, which is a real distinction there and not one here: every job this
    -- schedules is a provider refresh, they all wait on the same provider HTTP,
    -- and a second semaphore nothing can take is a comment pretending to be a
    -- mechanism. Nor is there a way to pause one job, so a column that is
    -- always 1 would only make `due` look conditional.
    -- `idle`, `running`, `success`, `failed`, `cancelled` — as `State` spells
    -- them, and unconstrained here for the same reason as the two above. No
    -- `queued`: nothing waits. `spawn` either claims the key and runs, or
    -- refuses and tells the caller so.
    state            TEXT NOT NULL DEFAULT 'idle',
    progress         REAL NOT NULL DEFAULT 0,
    message          TEXT,
    next_run_at      TEXT,
    started_at       TEXT,
    -- Set by a successful run only. The Sources page reads it as "Refreshed",
    -- and the end of a failed run would answer a different question.
    last_success_at  TEXT,
    last_error       TEXT
);

-- --------------------------------------------------------------- providers

-- Lets several accounts share one provider's stream budget. Zero rows on the
-- source instance, but M3U accounts carry the foreign key.
CREATE TABLE server_group (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE
);

CREATE TABLE m3u_account (
    id                     INTEGER PRIMARY KEY,
    name                   TEXT NOT NULL UNIQUE COLLATE NOCASE,
    -- Deliberately unconstrained. SQLite cannot ALTER a CHECK, so widening one
    -- means the documented 12-step table rebuild whose first step is
    -- `PRAGMA foreign_keys = OFF` — a no-op inside a transaction, which is
    -- where sqlx runs every migration. `stream` cascades from here and
    -- `channel_stream` cascades from that, so a future migration adding a
    -- third account type would delete the catalogue and report success.
    -- `M3uAccountType` is the real constraint; see CLAUDE.md.
    account_type           TEXT NOT NULL DEFAULT 'standard',
    server_url             TEXT,
    file_path              TEXT,
    username               TEXT,
    password               TEXT,
    -- 0 means unlimited.
    max_streams            INTEGER NOT NULL DEFAULT 0 CHECK (max_streams >= 0),
    is_active              INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    -- The built-in `custom` account holding hand-added streams; refresh must
    -- never touch it and the API must never delete it.
    locked                 INTEGER NOT NULL DEFAULT 0 CHECK (locked IN (0, 1)),
    priority               INTEGER NOT NULL DEFAULT 0,
    server_group_id        INTEGER REFERENCES server_group(id) ON DELETE SET NULL,
    user_agent_id          INTEGER REFERENCES user_agent(id) ON DELETE SET NULL,
    stream_profile_id      INTEGER REFERENCES stream_profile(id) ON DELETE SET NULL,
    refresh_interval_hours INTEGER NOT NULL DEFAULT 24 CHECK (refresh_interval_hours >= 0),
    stale_stream_days      INTEGER NOT NULL DEFAULT 7 CHECK (stale_stream_days >= 0),
    status                 TEXT NOT NULL DEFAULT 'idle',
    last_message           TEXT,
    custom_properties      TEXT NOT NULL DEFAULT '{}',
    created_at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    updated_at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')
);

CREATE TABLE m3u_account_profile (
    id                INTEGER PRIMARY KEY,
    m3u_account_id    INTEGER NOT NULL REFERENCES m3u_account(id) ON DELETE CASCADE,
    name              TEXT NOT NULL,
    is_default        INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1)),
    is_active         INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    max_streams       INTEGER NOT NULL DEFAULT 0 CHECK (max_streams >= 0),
    -- User-authored and PCRE-flavoured: compile with `fancy_regex`, never
    -- `regex`. `replace_pattern` uses JS-style `$1`, converted to `\1` on use.
    search_pattern    TEXT NOT NULL DEFAULT '',
    replace_pattern   TEXT NOT NULL DEFAULT '',
    custom_properties TEXT NOT NULL DEFAULT '{}',
    UNIQUE (m3u_account_id, name)
);

CREATE TABLE m3u_filter (
    id             INTEGER PRIMARY KEY,
    m3u_account_id INTEGER NOT NULL REFERENCES m3u_account(id) ON DELETE CASCADE,
    -- `name`, `group` or `url`, as `FilterTarget` spells them. Unconstrained
    -- for the reason the two above are, and because this one was already
    -- wrong: the ingest path has always mapped `url`, which the CHECK refused
    -- to store, so URL filters could not exist.
    filter_type    TEXT NOT NULL,
    regex_pattern  TEXT NOT NULL,
    exclude        INTEGER NOT NULL DEFAULT 1 CHECK (exclude IN (0, 1)),
    sort_order     INTEGER NOT NULL DEFAULT 0 CHECK (sort_order >= 0)
);

-- ---------------------------------------------------------------- channels

CREATE TABLE channel_group (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    -- Where this group's channels go. A channel created for the group -- by a
    -- refresh or by hand -- takes the next free number inside the range.
    --
    -- The range belongs to the group rather than to a provider link: two
    -- providers feeding one group must not carry two of them.
    number_start REAL,
    number_end   REAL
);

-- Per-account visibility and auto-sync policy for one provider group.
CREATE TABLE channel_group_m3u_account (
    id                      INTEGER PRIMARY KEY,
    channel_group_id        INTEGER NOT NULL REFERENCES channel_group(id) ON DELETE CASCADE,
    m3u_account_id          INTEGER NOT NULL REFERENCES m3u_account(id) ON DELETE CASCADE,
    enabled                 INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    auto_channel_sync       INTEGER NOT NULL DEFAULT 0 CHECK (auto_channel_sync IN (0, 1)),
    auto_sync_channel_start REAL,
    auto_sync_channel_end   REAL,
    is_stale                INTEGER NOT NULL DEFAULT 0 CHECK (is_stale IN (0, 1)),
    last_seen               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    custom_properties       TEXT NOT NULL DEFAULT '{}',
    UNIQUE (channel_group_id, m3u_account_id)
);

CREATE TABLE logo (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL COLLATE NOCASE,
    url  TEXT NOT NULL UNIQUE
);

CREATE TABLE stream (
    id                      INTEGER PRIMARY KEY,
    name                    TEXT NOT NULL COLLATE NOCASE,
    url                     TEXT,
    logo_url                TEXT,
    tvg_id                  TEXT,
    channel_group_id        INTEGER REFERENCES channel_group(id) ON DELETE SET NULL,
    m3u_account_id          INTEGER REFERENCES m3u_account(id) ON DELETE CASCADE,
    stream_profile_id       INTEGER REFERENCES stream_profile(id) ON DELETE SET NULL,
    is_custom               INTEGER NOT NULL DEFAULT 0 CHECK (is_custom IN (0, 1)),
    is_adult                INTEGER NOT NULL DEFAULT 0 CHECK (is_adult IN (0, 1)),
    -- Provider-assigned identifier, distinct from `id`.
    stream_id               INTEGER,
    stream_chno             REAL,
    -- Dedup key, derived per `stream_settings.m3u_hash_key`.
    stream_hash             TEXT,
    last_seen               TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    is_stale                INTEGER NOT NULL DEFAULT 0 CHECK (is_stale IN (0, 1)),
    is_catchup              INTEGER NOT NULL DEFAULT 0 CHECK (is_catchup IN (0, 1)),
    catchup_days            INTEGER NOT NULL DEFAULT 0 CHECK (catchup_days >= 0),
    custom_properties       TEXT NOT NULL DEFAULT '{}',
    -- Last observed codec/resolution/bitrate, refreshed by the proxy.
    stream_stats            TEXT,
    stream_stats_updated_at TEXT,
    updated_at              TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')
);

CREATE TABLE epg_source (
    id                     INTEGER PRIMARY KEY,
    name                   TEXT NOT NULL UNIQUE COLLATE NOCASE,
    -- Unconstrained for the same reason as `m3u_account.account_type`, and
    -- with more at stake: `epg_data` cascades from here and `program` cascades
    -- from that, so a rebuild to add a third source type takes the entire
    -- guide with it. Schedules Direct is the third type that will want adding.
    source_type            TEXT NOT NULL DEFAULT 'xmltv',
    url                    TEXT,
    file_path              TEXT,
    username               TEXT,
    password               TEXT,
    is_active              INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    priority               INTEGER NOT NULL DEFAULT 0 CHECK (priority >= 0),
    refresh_interval_hours INTEGER NOT NULL DEFAULT 24 CHECK (refresh_interval_hours >= 0),
    status                 TEXT NOT NULL DEFAULT 'idle',
    last_message           TEXT,
    custom_properties      TEXT NOT NULL DEFAULT '{}',
    created_at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    updated_at             TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')
);

CREATE TABLE epg_data (
    id            INTEGER PRIMARY KEY,
    epg_source_id INTEGER REFERENCES epg_source(id) ON DELETE CASCADE,
    tvg_id        TEXT COLLATE NOCASE,
    name          TEXT NOT NULL COLLATE NOCASE,
    icon_url      TEXT,
    UNIQUE (epg_source_id, tvg_id)
);

-- Where an EPG match that scored in the ambiguous band waits for a decision.
--
-- `sync::epg` returns three outcomes, not two. A score above the high
-- threshold is assigned outright; below the low one is ignored; in between is
-- a band a language model could resolve and this build deliberately does not
-- carry. Without somewhere to put that middle answer it degrades to "no
-- match", and the user sees a channel with an empty guide and no explanation
-- -- which is the regression, not the missing model.
--
-- One row per channel: the single best candidate. A list of near-misses is a
-- worse question to ask than "is this the right one?".
CREATE TABLE epg_match_suggestion (
    channel_id   INTEGER PRIMARY KEY REFERENCES channel(id) ON DELETE CASCADE,
    epg_data_id  INTEGER NOT NULL REFERENCES epg_data(id) ON DELETE CASCADE,
    score        REAL NOT NULL,
    suggested_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')
);

CREATE TABLE program (
    id                INTEGER PRIMARY KEY,
    epg_data_id       INTEGER NOT NULL REFERENCES epg_data(id) ON DELETE CASCADE,
    tvg_id            TEXT COLLATE NOCASE,
    start_time        TEXT NOT NULL,
    end_time          TEXT NOT NULL,
    title             TEXT NOT NULL COLLATE NOCASE,
    sub_title         TEXT,
    description       TEXT,
    custom_properties TEXT NOT NULL DEFAULT '{}'
);

-- Where an EPG refresh assembles the new guide before any of the old one is
-- thrown away.
--
-- Deleting a channel's programmes and inserting as the feed parses loses the
-- guide three ways, all of them reproduced: a cancel mid-parse, a SIGTERM
-- mid-parse, and a provider serving a truncated file. Each leaves the channel
-- with the fraction read so far and a job row saying `cancelled`, which reads
-- as "nothing happened".
--
-- Staging here makes the swap per guide channel one short transaction --
-- delete the old rows, promote the new ones -- so a channel holds either the
-- whole previous guide or the whole new one, never a prefix. A refresh that
-- dies before the swap has written nothing anyone can see.
--
-- A real table rather than a TEMP one: connections come from a pool, and a
-- TEMP table exists only on the connection that created it.
CREATE TABLE program_incoming (
    id                INTEGER PRIMARY KEY,
    -- Scoped by source so two sources refreshing at once cannot promote each
    -- other's half-written rows.
    epg_source_id     INTEGER NOT NULL REFERENCES epg_source(id) ON DELETE CASCADE,
    epg_data_id       INTEGER NOT NULL REFERENCES epg_data(id) ON DELETE CASCADE,
    tvg_id            TEXT COLLATE NOCASE,
    start_time        TEXT NOT NULL,
    end_time          TEXT NOT NULL,
    title             TEXT NOT NULL COLLATE NOCASE,
    sub_title         TEXT,
    description       TEXT,
    custom_properties TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE channel (
    id                  INTEGER PRIMARY KEY,
    uuid                TEXT NOT NULL UNIQUE,
    channel_number      REAL,
    name                TEXT NOT NULL COLLATE NOCASE,
    logo_id             INTEGER REFERENCES logo(id) ON DELETE SET NULL,
    channel_group_id    INTEGER REFERENCES channel_group(id) ON DELETE SET NULL,
    tvg_id              TEXT,
    tvc_guide_stationid TEXT,
    epg_data_id         INTEGER REFERENCES epg_data(id) ON DELETE SET NULL,
    stream_profile_id   INTEGER REFERENCES stream_profile(id) ON DELETE SET NULL,
    user_level          INTEGER NOT NULL DEFAULT 0,
    is_adult            INTEGER NOT NULL DEFAULT 0 CHECK (is_adult IN (0, 1)),
    hidden_from_output  INTEGER NOT NULL DEFAULT 0 CHECK (hidden_from_output IN (0, 1)),
    auto_created        INTEGER NOT NULL DEFAULT 0 CHECK (auto_created IN (0, 1)),
    is_catchup          INTEGER NOT NULL DEFAULT 0 CHECK (is_catchup IN (0, 1)),
    catchup_days        INTEGER NOT NULL DEFAULT 0 CHECK (catchup_days >= 0),
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')
);

-- User edits layered over a channel whose base fields provider sync keeps
-- rewriting. Every column is nullable and null means "inherit"; nothing but
-- the user writes here. Read through `effective_channel`, never directly.
CREATE TABLE channel_override (
    channel_id          INTEGER PRIMARY KEY REFERENCES channel(id) ON DELETE CASCADE,
    name                TEXT,
    channel_number      REAL,
    channel_group_id    INTEGER REFERENCES channel_group(id) ON DELETE SET NULL,
    logo_id             INTEGER REFERENCES logo(id) ON DELETE SET NULL,
    tvg_id              TEXT,
    tvc_guide_stationid TEXT,
    epg_data_id         INTEGER REFERENCES epg_data(id) ON DELETE SET NULL,
    stream_profile_id   INTEGER REFERENCES stream_profile(id) ON DELETE SET NULL,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00'),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')
);

-- Failover order for a channel: position 0 is tried first.
CREATE TABLE channel_stream (
    channel_id INTEGER NOT NULL REFERENCES channel(id) ON DELETE CASCADE,
    stream_id  INTEGER NOT NULL REFERENCES stream(id) ON DELETE CASCADE,
    sort_order INTEGER NOT NULL DEFAULT 0 CHECK (sort_order >= 0),
    PRIMARY KEY (channel_id, stream_id)
);

CREATE TABLE channel_profile (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE
);

CREATE TABLE channel_profile_membership (
    channel_profile_id INTEGER NOT NULL REFERENCES channel_profile(id) ON DELETE CASCADE,
    channel_id         INTEGER NOT NULL REFERENCES channel(id) ON DELETE CASCADE,
    enabled            INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    PRIMARY KEY (channel_profile_id, channel_id)
);

-- Which profiles a user may see. No rows means all of them.
CREATE TABLE user_channel_profile (
    user_id            INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE,
    channel_profile_id INTEGER NOT NULL REFERENCES channel_profile(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, channel_profile_id)
);

-- A channel missing from a profile is a channel missing from that profile's
-- M3U, XMLTV and HDHR output — a silent disappearance rather than an error.
-- Creating the rows in triggers instead of at every call site is what makes
-- that impossible to forget. Membership starts enabled; the API flips it.
CREATE TRIGGER channel_joins_every_profile
AFTER INSERT ON channel
BEGIN
    INSERT OR IGNORE INTO channel_profile_membership (channel_profile_id, channel_id)
    SELECT id, NEW.id FROM channel_profile;
END;

CREATE TRIGGER profile_starts_with_every_channel
AFTER INSERT ON channel_profile
BEGIN
    INSERT OR IGNORE INTO channel_profile_membership (channel_profile_id, channel_id)
    SELECT NEW.id, id FROM channel;
END;

-- The only channel shape outputs are allowed to read.
--
-- Coalescing here rather than in Rust is load-bearing: `/output/m3u`,
-- `/output/epg`, the HDHR lineup and the Xtream Codes API all sort, filter and
-- paginate on the overridden values, and resolving after the rows come back
-- means ordering on the wrong column.
--
-- COALESCE loses the column's NOCASE collation, so callers that sort by name
-- must say `ORDER BY name COLLATE NOCASE` explicitly. LIKE is unaffected: it
-- is already case-insensitive for ASCII.
CREATE VIEW effective_channel AS
SELECT
    c.id                                                    AS id,
    c.uuid                                                  AS uuid,
    COALESCE(o.channel_number, c.channel_number)            AS channel_number,
    COALESCE(o.name, c.name)                                AS name,
    COALESCE(o.channel_group_id, c.channel_group_id)        AS channel_group_id,
    g.name                                                  AS group_name,
    COALESCE(o.logo_id, c.logo_id)                          AS logo_id,
    l.url                                                   AS logo_url,
    COALESCE(o.tvg_id, c.tvg_id)                            AS tvg_id,
    COALESCE(o.tvc_guide_stationid, c.tvc_guide_stationid)  AS tvc_guide_stationid,
    COALESCE(o.epg_data_id, c.epg_data_id)                  AS epg_data_id,
    COALESCE(o.stream_profile_id, c.stream_profile_id)      AS stream_profile_id,
    c.user_level                                            AS user_level,
    c.is_adult                                              AS is_adult,
    c.hidden_from_output                                    AS hidden_from_output,
    c.is_catchup                                            AS is_catchup,
    c.catchup_days                                          AS catchup_days,
    c.auto_created                                          AS auto_created,
    c.created_at                                            AS created_at,
    (o.channel_id IS NOT NULL)                              AS has_override
FROM channel c
LEFT JOIN channel_override o ON o.channel_id = c.id
LEFT JOIN channel_group g ON g.id = COALESCE(o.channel_group_id, c.channel_group_id)
LEFT JOIN logo l ON l.id = COALESCE(o.logo_id, c.logo_id);

-- ---------------------------------------------------------------- indexes
--
-- SQLite does not index foreign keys automatically, and each of these is
-- either a cascade target or a join on an output path.

CREATE INDEX idx_stream_group ON stream (channel_group_id);
CREATE INDEX idx_stream_account ON stream (m3u_account_id);
CREATE INDEX idx_stream_hash ON stream (stream_hash);
CREATE INDEX idx_stream_name ON stream (name COLLATE NOCASE);

CREATE INDEX idx_channel_group ON channel (channel_group_id);
CREATE INDEX idx_channel_logo ON channel (logo_id);
CREATE INDEX idx_channel_epg ON channel (epg_data_id);
CREATE INDEX idx_channel_number ON channel (channel_number);
CREATE INDEX idx_channel_name ON channel (name COLLATE NOCASE);

CREATE INDEX idx_override_group ON channel_override (channel_group_id);
CREATE INDEX idx_override_logo ON channel_override (logo_id);
CREATE INDEX idx_override_epg ON channel_override (epg_data_id);

CREATE INDEX idx_channel_stream_stream ON channel_stream (stream_id);
CREATE INDEX idx_channel_stream_order ON channel_stream (channel_id, sort_order);

CREATE INDEX idx_membership_channel ON channel_profile_membership (channel_id);
CREATE INDEX idx_user_profile_profile ON user_channel_profile (channel_profile_id);

CREATE INDEX idx_epg_data_name ON epg_data (name COLLATE NOCASE);
CREATE INDEX idx_suggestion_epg ON epg_match_suggestion (epg_data_id);

-- The guide output is a keyset walk over (channel, time); the tvg_id index
-- serves Dummy EPG and unmapped-programme cleanup.
CREATE INDEX idx_program_window ON program (epg_data_id, start_time);
CREATE INDEX idx_program_tvg ON program (tvg_id COLLATE NOCASE, start_time);
CREATE INDEX idx_incoming_source_data ON program_incoming (epg_source_id, epg_data_id);

CREATE INDEX idx_m3u_profile_account ON m3u_account_profile (m3u_account_id);
CREATE INDEX idx_m3u_filter_account ON m3u_filter (m3u_account_id, sort_order);
CREATE INDEX idx_group_account_account ON channel_group_m3u_account (m3u_account_id);

CREATE INDEX idx_system_event_time ON system_event (occurred_at DESC);
CREATE UNIQUE INDEX idx_notification_subject ON notification (kind, subject);
CREATE INDEX idx_notification_open ON notification (acknowledged_at, updated_at DESC);
CREATE INDEX idx_job_due ON job (next_run_at);

-- ------------------------------------------------------------------ seeds

INSERT INTO user_agent (id, name, user_agent, description) VALUES
    (1, 'TiviMate', 'TiviMate/5.1.6 (Android 12)', 'Default for provider fetches'),
    (2, 'VLC', 'VLC/3.0.21 LibVLC/3.0.21', ''),
    (3, 'Chrome', 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.3', '');

-- Parameter strings are written from the ffmpeg, streamlink and VLC command
-- line documentation rather than lifted from upstream's migrations:
-- reimplementing behaviour is fine, copying literal expression is not.
--
-- `{streamUrl}`, `{userAgent}` and `{channelId}` are substituted before the
-- argv is split, so a profile never goes through a shell.
INSERT INTO stream_profile (id, name, command, parameters, locked, user_agent_id) VALUES
    (1, 'ffmpeg', 'ffmpeg',
     '-hide_banner -loglevel error -user_agent {userAgent} -i {streamUrl} -map 0 -c copy -f mpegts pipe:1',
     1, 1),
    (2, 'streamlink', 'streamlink',
     '--stdout --http-header User-Agent={userAgent} {streamUrl} best',
     1, 1),
    -- Empty command: the upstream body is relayed byte-for-byte with no child
    -- process at all. This is the default and by far the cheapest path.
    (3, 'proxy', '', '', 1, NULL),
    -- Also empty, but answered with a 302 to the provider URL. No bytes pass
    -- through this process, so no ring is allocated.
    (4, 'redirect', '', '', 1, NULL),
    (5, 'vlc', 'cvlc',
     '-I dummy --quiet --no-video-title-show --play-and-exit --http-user-agent={userAgent} {streamUrl} --sout=#standard{access=file,mux=ts,dst=-}',
     1, 1);

-- Two transcodes covering the case a remux cannot: a player that will not
-- decode the provider's audio. Video is always copied — re-encoding it would
-- cost more CPU than this project's entire memory budget saves.
INSERT INTO output_profile (id, name, command, parameters, locked) VALUES
    (1, 'AC3 audio (media servers)', 'ffmpeg',
     '-hide_banner -loglevel error -fflags +genpts+discardcorrupt -i pipe:0 -map 0 -c:v copy -c:a ac3 -b:a 384k -f mpegts -mpegts_flags +resend_headers -flush_packets 1 pipe:1',
     1),
    (2, 'AAC stereo (web players)', 'ffmpeg',
     '-hide_banner -loglevel error -fflags +genpts+discardcorrupt -i pipe:0 -map 0 -c:v copy -c:a aac -b:a 192k -ac 2 -f mpegts -mpegts_flags +resend_headers -flush_packets 1 pipe:1',
     1);

-- Defaults are the values observed on the instance being replaced, so a fresh
-- install behaves like the one it stands in for. The exception is the ring
-- window: upstream retains 90 seconds, which is ~90 MB per channel at 8 Mbps
-- and blows the whole memory budget with a single viewer.
INSERT INTO core_setting (id, key, name, value) VALUES
    (1, 'stream_settings', 'Stream Settings',
     '{"default_user_agent":1,"default_stream_profile":3,"m3u_hash_key":"url","hdhr_output_profile_id":null}'),
    (2, 'proxy_settings', 'Proxy Settings',
     '{"buffering_timeout":15,"buffering_speed":1.0,"ring_seconds":15,"ring_max_bytes":37500000,"channel_shutdown_delay":0,"channel_init_grace_period":60,"channel_client_wait_period":5,"new_client_behind_seconds":5}'),
    (3, 'network_access', 'Network Access', '{}'),
    (4, 'system_settings', 'System Settings',
     '{"preferred_region":null,"max_system_events":100}'),
    (5, 'epg_settings', 'EPG Settings',
     '{"epg_auto_match_on_refresh":false,"epg_match_ignore_prefixes":[],"epg_match_ignore_suffixes":[],"epg_match_ignore_custom":[]}');

-- The lineup's numbering policy: how wide a block a group gets when a range is
-- assigned for it, and the spacing between the numbers a range hands out.
-- Without an id, so it lands after the five above and the importer, which
-- patches this table by key, does not have to know about it.
INSERT INTO core_setting (key, name, value) VALUES
    ('numbering_settings', 'Numbering', '{"group_block_size":100,"channel_step":1}');

-- Hand-added streams need an account to hang off, and M3U refresh must never
-- touch them.
INSERT INTO m3u_account (id, name, account_type, locked, max_streams) VALUES
    (1, 'custom', 'standard', 1, 0);

INSERT INTO m3u_account_profile (id, m3u_account_id, name, is_default, search_pattern, replace_pattern) VALUES
    (1, 1, 'custom Default', 1, '^(.*)$', '$1');

-- Outputs address a channel profile by name; without one there is nothing to
-- point Plex at on a fresh install.
INSERT INTO channel_profile (id, name) VALUES (1, 'All');
