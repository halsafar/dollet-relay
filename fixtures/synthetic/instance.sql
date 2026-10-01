-- A hand-written instance covering every shape the product supports.
--
-- Applied after the migrations, exactly as `sample.sql` is. Ids start at 1000
-- so nothing here can collide with the rows the migrations seed (1-5) or with
-- `sample.sql`, and the two seeds are never loaded into the same database.
--
-- Every row group is explained in README.md beside this file. A row nobody can
-- explain is a row that will be deleted the next time this file is tidied.
--
-- Hostnames are `provider.example`, `xtream.example`, `guide.example` and
-- `logos.example`; credentials are obviously fake. A test in
-- `crates/dollet-core/tests/import.rs` enforces both.
--
-- Programme times are written relative to `'now'` by SQLite itself, because
-- "a programme ending exactly now" is the shape the guide window, the grid and
-- the Xtream short EPG are all decided by, and a fixed timestamp stops being
-- that shape the day after it is written. Everything else is fixed.

-- ------------------------------------------------------------------ users
--
-- One of each level, plus the deactivated admin that `setup_status` has to
-- keep counting. Hashes are the Django format at 1,000 iterations rather than
-- the shipped 1.2M: a test signs in dozens of times and the format is what is
-- under test, not the work factor.

INSERT INTO user (id, username, email, password, is_active, user_level, api_key, stream_limit, custom_properties) VALUES
    (1000, 'synthadmin', 'admin@example.test',
     'pbkdf2_sha256$1000$syntheticadminXXXXXXXX$M5yz0BhUTUkJqSXBnUvZKdgsqkRWQtWEJQ84Blo/+0Y=',
     1, 10, 'synthetic-admin-api-key-00000000000000', 0,
     '{"xc_password":"synth-xc-admin"}'),
    (1001, 'synthstandard', 'standard@example.test',
     'pbkdf2_sha256$1000$syntheticstandardXXXXX$G5qW8X99AJcWHVUsrXTlKgMn2KRkiYojpLXCl2WUDwI=',
     1, 1, 'synthetic-standard-api-key-0000000000', 1,
     '{"xc_password":"synth-xc-standard"}'),
    (1002, 'synthstreamer', 'streamer@example.test',
     'pbkdf2_sha256$1000$syntheticstreamerXXXXX$0HXMMLQrkbGb2q74sXEopHvPzrJA6AEURhLFlMcQOMA=',
     1, 0, NULL, 0, '{}'),
    -- Deactivated, but still an admin. `initialize-superuser` counts admins
    -- regardless of `is_active`, so this row is what keeps setup shut. The key
    -- exists so a test can *present* a credential and watch it be refused,
    -- rather than proving nothing by sending none.
    (1003, 'synthinactive', 'inactive@example.test',
     'pbkdf2_sha256$1000$syntheticinactiveXXXXX$a7+vRDxBvHsQ6/m3wm+/Gnnque8KQyMy+fNB0tl7BDM=',
     0, 10, 'synthetic-inactive-api-key-0000000000', 0, '{}');

-- ------------------------------------------------------------------- core

-- A user agent nothing else names, so the fallback chain in `sources_for`
-- resolves to a value that can only have come from the profile below.
INSERT INTO user_agent (id, name, user_agent, description) VALUES
    (1001, 'Synth Provider Agent', 'SynthPlayer/1.0 (dollet-test)', 'Named by the synthetic stream profile'),
    -- Named by the *account* rather than by a profile, so the two rungs of the
    -- fallback ladder in `sources_for` resolve to different strings and a test
    -- can say which one answered.
    (1002, 'Synth Account Agent', 'SynthAccount/1.0 (dollet-test)', 'Named by the synthetic provider account');

-- An unlocked proxy profile that names a user agent. The shipped `ffmpeg`
-- profile also names one, but it also carries a command, so a test using it
-- could not tell "the agent came from the profile" apart from "the agent came
-- from the command".
INSERT INTO stream_profile (id, name, command, parameters, locked, is_active, user_agent_id) VALUES
    (1001, 'Synth Direct', '', '', 0, 1, 1001),
    -- The account's default profile. It names no agent, so a stream using it
    -- falls through to the account's own.
    (1002, 'Synth Account Default', '', '', 0, 1, NULL);

-- An unlocked output profile, so `?output_profile=` and the HDHR
-- `output_profile` scope can be exercised against something the API could also
-- have deleted. `hdhr_output_profile_id` points at the shipped id 1 instead,
-- which is what makes "the setting is the fallback, the URL wins" testable.
INSERT INTO output_profile (id, name, command, parameters, locked, is_active) VALUES
    (1001, 'Synth Transcode', 'ffmpeg', '-hide_banner -i pipe:0 -map 0 -c copy -f mpegts pipe:1', 0, 1);

-- Settings the shipped defaults do not cover:
--   * `hdhr_output_profile_id` set, so the lineup fallback is reachable.
--   * `network_access` restricting one endpoint class, to loopback so the
--     suite's own peer is admitted and an outside address is not.
--   * a `preferred_region`, which biases EPG matching towards one country's
--     guide channels and is unset by default.
UPDATE core_setting SET value =
    '{"default_user_agent":1,"default_stream_profile":3,"m3u_hash_key":"url","hdhr_output_profile_id":1}'
    WHERE key = 'stream_settings';
UPDATE core_setting SET value = '{"M3U_EPG":"127.0.0.0/8"}' WHERE key = 'network_access';
UPDATE core_setting SET value =
    '{"preferred_region":"us","max_system_events":100}'
    WHERE key = 'system_settings';

-- -------------------------------------------------------------- providers

-- Several accounts sharing one provider's stream budget. Zero rows on the
-- reference instance, so nothing else reaches this table.
INSERT INTO server_group (id, name) VALUES (1001, 'Synth Server Group');

INSERT INTO m3u_account (id, name, account_type, server_url, file_path, username, password,
                         max_streams, is_active, locked, priority, server_group_id, user_agent_id,
                         stream_profile_id, refresh_interval_hours, stale_stream_days, status,
                         last_message, custom_properties) VALUES
    -- A standard playlist account: a URL, a filter that excludes a group, and
    -- two profiles, one of which rewrites stream URLs.
    -- Carries a `file_path` as well as a URL, and the file is not there. Two
    -- reasons: the account detail payload is what the Settings page decodes and
    -- a shape whose optional fields are all null pins nothing, and a configured
    -- path that has gone missing has to fall back to the provider rather than
    -- fail the refresh.
    (1001, 'Synth Standard', 'standard', 'https://provider.example/get.php?username=synthuser&password=synthpass&type=m3u_plus',
     '/nonexistent/synth-standard.m3u', 'synthuser', 'synthpass', 2, 1, 0, 0, 1001, 1002, 1002, 24, 7, 'success', NULL,
     '{"auto_enable_new_groups_live":true}'),
    -- Xtream Codes: `max_streams = 1`, which is what makes the tuner count and
    -- the Xtream `max_connections` come out as something other than a default.
    (1002, 'Synth Xtream', 'xtream_codes', 'https://xtream.example:8080', NULL,
     'synthxc', 'synthxcpass', 1, 1, 0, 1, NULL, NULL, NULL, 12, 3, 'idle', NULL, '{}'),
    -- Inactive: the scheduler must not register a refresh for it, and its
    -- streams must still serve.
    (1003, 'Synth Retired', 'standard', 'https://provider.example/retired.m3u', NULL,
     NULL, NULL, 0, 0, 0, 2, NULL, NULL, NULL, 24, 7, 'error', 'account disabled by the operator', '{}');

INSERT INTO m3u_account_profile (id, m3u_account_id, name, is_default, is_active, max_streams, search_pattern, replace_pattern) VALUES
    (1001, 1001, 'Synth Standard Default', 1, 1, 2, '^(.*)$', '$1'),
    -- A real search/replace, with its own stream budget. `$1` in the
    -- replacement is deliberate: the conversion to `\1` applies to search
    -- patterns only, and a fixture whose replacement has no backreference
    -- could not catch that being got wrong.
    (1002, 1001, 'Synth Standard Rewrite', 0, 1, 1, '^(.*)/live/(.*)\.ts$', '$1/hls/$2.m3u8'),
    (1003, 1002, 'Synth Xtream Default', 1, 1, 1, '^(.*)$', '$1');

-- Excludes the adult group from this account's feed. PCRE-flavoured and
-- anchored, so it matches one group rather than every group containing the
-- word.
INSERT INTO m3u_filter (id, m3u_account_id, filter_type, regex_pattern, exclude, sort_order) VALUES
    (1001, 1001, 'group', '^Synth Adults$', 1, 0);

-- ----------------------------------------------------------------- groups

INSERT INTO channel_group (id, name, number_start, number_end) VALUES
    (1001, 'Synth Sports', NULL, NULL),
    -- The group with a number range: where auto-sync and a hand-made channel
    -- in the group take their numbers from.
    (1002, 'Synth News', 200, 299),
    (1003, 'Synth Adults', NULL, NULL),
    (1004, 'Synth Empty', NULL, NULL),
    (1005, 'Synth Retired Group', NULL, NULL);

INSERT INTO channel_group_m3u_account (id, channel_group_id, m3u_account_id, enabled, auto_channel_sync,
                                       auto_sync_channel_start, auto_sync_channel_end, is_stale,
                                       last_seen, custom_properties) VALUES
    (1001, 1001, 1001, 1, 0, NULL, NULL, 0, '2026-09-01 00:00:00.000+00:00', '{}'),
    -- Auto channel sync, with the numbering and renaming options the UI writes
    -- into `custom_properties` rather than into columns. The range columns are
    -- upstream's, read by nothing since the range moved to `channel_group`.
    (1002, 1002, 1001, 1, 1, NULL, NULL, 0, '2026-09-01 00:00:00.000+00:00',
     '{"channel_numbering_mode":"provider","channel_numbering_fallback":200,"name_regex_pattern":"^Synth ","name_replace_pattern":""}'),
    (1003, 1003, 1001, 1, 0, NULL, NULL, 0, '2026-09-01 00:00:00.000+00:00', '{}'),
    -- A group with no streams at all: it exists because a channel is in it.
    (1004, 1004, 1001, 1, 0, NULL, NULL, 0, '2026-09-01 00:00:00.000+00:00', '{}'),
    -- Disabled for this account. Reconcile deletes streams whose group is
    -- disabled, which is the difference between this and a group merely absent
    -- from one refresh.
    (1005, 1005, 1001, 0, 0, NULL, NULL, 1, '2026-08-01 00:00:00.000+00:00', '{}'),
    (1006, 1001, 1002, 1, 0, NULL, NULL, 0, '2026-09-01 00:00:00.000+00:00', '{}');

-- ------------------------------------------------------------------ logos
--
-- One used by a single channel, one used by several, one used by none — the
-- three cases `logos/cleanup/` and the usage count have to tell apart.

INSERT INTO logo (id, name, url) VALUES
    (1001, 'Synth Sports Logo', 'https://logos.example/synth-sports.png'),
    (1002, 'Synth Shared Logo', 'https://logos.example/synth-shared.png'),
    (1003, 'Synth Orphan Logo', 'https://logos.example/synth-orphan.png');

-- ------------------------------------------------------------------ guide

INSERT INTO epg_source (id, name, source_type, url, file_path, username, password, is_active,
                        priority, refresh_interval_hours, status, last_message, custom_properties) VALUES
    -- Same as the provider account above: a URL, plus a configured file that
    -- is not on disk, plus credentials — so the source payload has no field
    -- that is null in every row.
    (1001, 'Synth XMLTV', 'xmltv', 'https://guide.example/synth.xml', '/nonexistent/synth.xml', 'synthguide', 'synthguidepass', 1, 0, 24, 'success', NULL, '{}'),
    -- Generated per request rather than stored: a channel on this source has
    -- no `program` rows and still has listings.
    (1002, 'Synth Dummy', 'dummy', NULL, NULL, NULL, NULL, 1, 1, 24, 'idle', NULL, '{}'),
    -- Inactive: no schedule, and its guide data must still serve.
    (1003, 'Synth Retired Guide', 'xmltv', 'https://guide.example/retired.xml', NULL, NULL, NULL, 0, 2, 24, 'error', 'source disabled by the operator', '{}');

INSERT INTO epg_data (id, epg_source_id, tvg_id, name, icon_url) VALUES
    (1001, 1001, 'synth.sports', 'Synth Sports Guide', 'https://logos.example/synth-sports.png'),
    -- No icon: the nullable column has to be null somewhere.
    (1002, 1001, 'synth.news', 'Synth News Guide', NULL),
    -- Mapped by a channel, but the feed carried no programmes for it — which
    -- is not the same as "nothing on", and every caller reading programmes has
    -- to tell the two apart.
    (1003, 1001, 'synth.gap', 'Synth Gap Guide', NULL),
    (1004, 1002, 'synth.dummy', 'Synth Dummy Guide', NULL),
    -- Declared by the feed and mapped by nothing: the picker offers it, the
    -- programme pass skips it.
    (1005, 1001, 'synth.unmapped', 'Synth Unmapped Guide', NULL),
    (1006, 1003, 'synth.retired', 'Synth Retired Guide Channel', NULL);

-- Programmes around `now`, which is the only interesting place for them to be:
-- one ending exactly now, one starting exactly now, one overlapping that, a
-- gap, and one after the gap.
INSERT INTO program (id, epg_data_id, tvg_id, start_time, end_time, title, sub_title, description, custom_properties) VALUES
    (1001, 1001, 'synth.sports',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '-60 minutes') || '+00:00',
     strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00',
     'Synth Just Ended', NULL, 'Ends exactly now.', '{}'),
    (1002, 1001, 'synth.sports',
     strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '+60 minutes') || '+00:00',
          -- The only programme carrying episode numbering, which the guide grid
     -- reads out of `custom_properties` because XMLTV has no column for it.
     'Synth On Now', 'A sub-title', 'Starts exactly now.',
     '{"categories":["Sports"],"rating":"PG","season":2,"episode":5,"new":true,"live":true}'),
    -- Overlaps `Synth On Now`. A provider that publishes overlapping entries is
    -- normal, and a grid that assumes they do not renders one on top of another.
    (1003, 1001, 'synth.sports',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '+30 minutes') || '+00:00',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '+90 minutes') || '+00:00',
     'Synth Overlapping', NULL, 'Starts before the previous one ends.', '{}'),
    -- Then nothing until +3h: the gap.
    (1004, 1001, 'synth.sports',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '+180 minutes') || '+00:00',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '+240 minutes') || '+00:00',
     'Synth After The Gap', NULL, 'Nothing is scheduled between +90m and +180m.', '{}'),
    -- A second guide channel, so a test can tell "this channel's listings"
    -- apart from "every listing".
    (1005, 1002, 'synth.news',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '-30 minutes') || '+00:00',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '+30 minutes') || '+00:00',
     'Synth News At Now', NULL, 'Spans now.', '{}'),
    -- Well outside any window a client asks for, so "the window bounded the
    -- answer" is provable rather than assumed.
    (1006, 1002, 'synth.news',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '+30 days') || '+00:00',
     strftime('%Y-%m-%d %H:%M:%f', 'now', '+30 days', '+60 minutes') || '+00:00',
     'Synth Far Future', NULL, 'A month out.', '{}');

-- ---------------------------------------------------------------- channels
--
-- Seventeen, chosen so that every branch an output can take is reachable
-- without a test building a row first. See README.md for the one-line reason
-- each exists.

INSERT INTO channel (id, uuid, channel_number, name, logo_id, channel_group_id, tvg_id,
                     tvc_guide_stationid, epg_data_id, stream_profile_id, user_level, is_adult,
                     hidden_from_output, auto_created, is_catchup, catchup_days, created_at, updated_at) VALUES
    (1000, 'bbbbbbbb-0000-4000-8000-000000001000', 1.0, 'Synth One', 1001, 1001, 'synth.sports', NULL, 1001, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    -- Every base value here is overridden below; none of them may reach an output.
    (1001, 'bbbbbbbb-0000-4000-8000-000000001001', 99.0, 'Provider Raw Name', 1001, 1002, 'raw.tvg', 'RAWSTATION', 1003, 1, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1002, 'bbbbbbbb-0000-4000-8000-000000001002', NULL, 'Synth Unnumbered', NULL, 1002, NULL, NULL, NULL, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1003, 'bbbbbbbb-0000-4000-8000-000000001003', 3.0, 'Synth "Quoted" Channel', 1002, 1002, NULL, NULL, NULL, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1004, 'bbbbbbbb-0000-4000-8000-000000001004', 4.0, 'Synth <Angle> & Ampersand', NULL, 1002, NULL, NULL, NULL, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1005, 'bbbbbbbb-0000-4000-8000-000000001005', 5.0, 'Synth Ünïcøde Ñoise', NULL, 1001, NULL, NULL, NULL, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1006, 'bbbbbbbb-0000-4000-8000-000000001006', 6.0, '  Synth Padded  ', NULL, 1001, NULL, NULL, NULL, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1007, 'bbbbbbbb-0000-4000-8000-000000001007', 7.0, 'Synth Hidden', NULL, 1001, NULL, NULL, NULL, NULL, 0, 0, 1, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1008, 'bbbbbbbb-0000-4000-8000-000000001008', 8.0, 'Synth Adult', NULL, 1003, NULL, NULL, NULL, NULL, 0, 1, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1009, 'bbbbbbbb-0000-4000-8000-000000001009', 9.0, 'Synth Admin Only', NULL, 1001, NULL, NULL, NULL, NULL, 10, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1010, 'bbbbbbbb-0000-4000-8000-000000001010', 10.0, 'Synth Standard Only', NULL, 1001, NULL, NULL, NULL, NULL, 1, 0, 0, 1, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1011, 'bbbbbbbb-0000-4000-8000-000000001011', 11.0, 'Synth Dummy Guide', NULL, 1002, NULL, NULL, 1004, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1012, 'bbbbbbbb-0000-4000-8000-000000001012', 12.0, 'Synth Gap Guide', NULL, 1002, NULL, NULL, 1003, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1013, 'bbbbbbbb-0000-4000-8000-000000001013', 13.0, 'Synth Redirect', NULL, 1001, NULL, NULL, NULL, 4, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1014, 'bbbbbbbb-0000-4000-8000-000000001014', 14.0, 'Synth Streamless', NULL, 1004, NULL, NULL, NULL, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1015, 'bbbbbbbb-0000-4000-8000-000000001015', 15.0, 'Synth Catchup Agent', 1002, 1001, NULL, 'SYNTHSTATION', NULL, 1001, 0, 0, 0, 0, 1, 3, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1016, 'bbbbbbbb-0000-4000-8000-000000001016', 16.0, 'Synth Null Url', NULL, 1001, NULL, NULL, NULL, NULL, 0, 0, 0, 0, 0, 0, '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00');

-- Every overridable column at once, on one channel, with a different value in
-- each. Nothing in `channel` row 1001 may reach an output.
INSERT INTO channel_override (channel_id, name, channel_number, channel_group_id, logo_id, tvg_id,
                              tvc_guide_stationid, epg_data_id, stream_profile_id) VALUES
    (1001, 'Synth Two & A Half', 2.5, 1001, 1002, 'synth.news', 'SYNTHOVERRIDE', 1002, 3);

-- ---------------------------------------------------------------- streams

INSERT INTO stream (id, name, url, logo_url, tvg_id, channel_group_id, m3u_account_id,
                    stream_profile_id, is_custom, is_adult, stream_id, stream_chno, stream_hash,
                    last_seen, is_stale, is_catchup, catchup_days, custom_properties,
                    stream_stats, stream_stats_updated_at, updated_at) VALUES
    -- Hand-added, on the built-in `custom` account. A refresh must never touch it.
    (1000, 'Synth Hand Added', 'https://provider.example/custom/hand-added.ts', NULL, NULL, 1001, 1, NULL, 1, 0, NULL, NULL, NULL, '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    -- Channel 1000's failover list, in order: standard account, then Xtream,
    -- then the hand-added one. Two accounts, so the limit bookkeeping in
    -- `sources_for` has two budgets to resolve rather than one.
    (1001, 'Synth One HD', 'https://provider.example/live/synthuser/synthpass/1001.ts', 'https://logos.example/synth-sports.png', 'synth.sports', 1001, 1001, NULL, 0, 0, NULL, 1.0, '4c7d70228b0824cf69dd3ac57407924861515e051c0971bc74e4942739a2d29e', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', '{"video_codec":"h264","resolution":"1920x1080"}', '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00'),
    (1002, 'Synth One Backup', 'https://xtream.example:8080/live/synthxc/synthxcpass/2002.ts', NULL, 'synth.sports', 1001, 1002, 1001, 0, 0, 2002, 1.0, 'a2806b5336c94bd935426e2315fc1638c2a73e084074fbc9b67d3297d36a27ee', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1003, 'Synth Two Feed', 'https://provider.example/live/synthuser/synthpass/1003.ts', NULL, 'synth.news', 1002, 1001, NULL, 0, 0, NULL, 2.5, 'c0fc01ce56d517c713f52a9c5784c2dcf20fed69c6b01fa85e9c2466e0bbd004', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1004, 'Synth Unnumbered Feed', 'https://provider.example/live/synthuser/synthpass/1004.ts', NULL, NULL, 1002, 1001, NULL, 0, 0, NULL, NULL, '2038f47bf0ae57e7870bbc79471ce378e36876608f7b15cb411448e2bab47e04', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1005, 'Synth Quoted Feed', 'https://provider.example/live/synthuser/synthpass/1005.ts', NULL, NULL, 1002, 1001, NULL, 0, 0, NULL, 3.0, 'aa004e29f40b47862f9380251b263186e33e45e3d0d804b5ed7b6d1775be16b0', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1006, 'Synth Ampersand Feed', 'https://provider.example/live/synthuser/synthpass/1006.ts', NULL, NULL, 1002, 1001, NULL, 0, 0, NULL, 4.0, 'edb25c106a52353c1aec05bb5d6dfab3c5fd2b18bda0ee8d58b1dd51860ed4a5', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1007, 'Synth Unicode Feed', 'https://provider.example/live/synthuser/synthpass/1007.ts', NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, 5.0, 'ee2c969703a2e447de3dd752a492572a1b57ad6e30c52c9eba6169c3901e48e9', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1008, 'Synth Padded Feed', 'https://provider.example/live/synthuser/synthpass/1008.ts', NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, 6.0, 'c870fe4cf82b152a3a4e84fa21c38f3359b3ecbe7f68c45b39d895cb93e71b62', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1009, 'Synth Hidden Feed', 'https://provider.example/live/synthuser/synthpass/1009.ts', NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, 7.0, '2a81b49f5807da1a65dce093fca78d4289ea440c6ad274bd6710f4193ea209f3', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    -- In the group the account's filter excludes, and flagged adult.
    (1010, 'Synth Adult Feed', 'https://provider.example/live/synthuser/synthpass/1010.ts', NULL, NULL, 1003, 1001, NULL, 0, 1, NULL, 8.0, '97d3ffcc996798543bb1fd4129b9783b1ca3e3c3b6b66fb4a6f91ac8348b71e2', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1011, 'Synth Admin Only Feed', 'https://provider.example/live/synthuser/synthpass/1011.ts', NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, 9.0, 'eae508c5a88eb2f9ce5a6e0cf88e798540e9ddc4f34740cffb87b7691acf69e4', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1012, 'Synth Standard Only Feed', 'https://provider.example/live/synthuser/synthpass/1012.ts', NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, 10.0, '6d226afc8761f7185a236317db061aa890cd6fb469cbaf3feb25bc358cb6bdda', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1013, 'Synth Dummy Feed', 'https://provider.example/live/synthuser/synthpass/1013.ts', NULL, NULL, 1002, 1001, NULL, 0, 0, NULL, 11.0, '4845b35f3ed65ed9c8351033404e0e5d087789db00f2170ef133a65bfd9fed80', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1014, 'Synth Gap Feed', 'https://provider.example/live/synthuser/synthpass/1014.ts', NULL, NULL, 1002, 1001, NULL, 0, 0, NULL, 12.0, 'dd77dbcdc046b7e166bd777456e2e5fc63c614cbc46e00e93e141a078fb0a4e4', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1015, 'Synth Redirect Feed', 'https://provider.example/live/synthuser/synthpass/1015.ts', NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, 13.0, '5437963299574a6798efbab3564d9033120a70c32732f94e3b32f022b25f2fd0', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    (1016, 'Synth Agent Feed', 'https://provider.example/live/synthuser/synthpass/1016.ts', NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, 15.0, 'c4457d101120c7a0da85912cc11e3328c5c81df5141245f29e644238430d5b6e', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    -- The only stream on its channel, and it has no URL. `sources_for` drops
    -- it, so the channel is reachable and unplayable at the same time — which
    -- is what makes switching a stream by *position* pick the wrong one.
    (1017, 'Synth No Url', NULL, NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, 16.0, 'c83b5abd26f2cee22ab60700ca66c91c4b96c62faac05c46cf5d6d53bff2629d', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00'),
    -- Attached to no channel, in the group whose account link is disabled.
    (1018, 'Synth Retired Feed', 'https://provider.example/live/synthuser/synthpass/1018.ts', NULL, NULL, 1005, 1001, NULL, 0, 0, NULL, NULL, '733b4155945244f75f9f89b854358e5944c1bcc6e33ecc52c093e52374701dbf', '2026-08-01 00:00:00.000+00:00', 1, 0, 0, '{}', NULL, NULL, '2026-08-01 00:00:00.000+00:00'),
    -- Attached to no channel, on the inactive account: a stream the editor can
    -- see and no output can reach.
    (1019, 'Synth Retired Account Feed', 'https://provider.example/retired/1019.ts', NULL, NULL, 1002, 1003, NULL, 0, 0, NULL, NULL, '19d32c038d235a20956d359c82562e268f2c9d36ac1a037ee2a31ee1564715f2', '2026-08-01 00:00:00.000+00:00', 1, 0, 0, '{}', NULL, NULL, '2026-08-01 00:00:00.000+00:00'),
    -- Attached to no channel, in a group whose link is enabled, on the account
    -- the refresh reads: what switching auto-sync on for the group would turn
    -- into a channel. The reference instance has three of these — a provider's
    -- second copy of a channel already in the lineup, kept out on purpose.
    (1020, 'Synth Loose Feed', 'https://provider.example/live/synthuser/synthpass/1020.ts', NULL, NULL, 1001, 1001, NULL, 0, 0, NULL, NULL, 'e4cc2beda85e66df19aba75cef2c1c616109d6d512bf3e0c749cae7a1104c728', '2026-09-01 00:00:00.000+00:00', 0, 0, 0, '{}', NULL, NULL, '2026-09-01 00:00:00.000+00:00');

INSERT INTO channel_stream (channel_id, stream_id, sort_order) VALUES
    (1000, 1001, 0), (1000, 1002, 1), (1000, 1000, 2),
    (1001, 1003, 0),
    (1002, 1004, 0),
    (1003, 1005, 0),
    (1004, 1006, 0),
    (1005, 1007, 0),
    (1006, 1008, 0),
    (1007, 1009, 0),
    (1008, 1010, 0),
    (1009, 1011, 0),
    (1010, 1012, 0),
    (1011, 1013, 0),
    (1012, 1014, 0),
    (1013, 1015, 0),
    (1015, 1016, 0),
    (1016, 1017, 0);
-- Channel 1014 deliberately has none.

-- --------------------------------------------------------------- profiles
--
-- `All` (id 1) ships with the migrations. These two are inserted after the
-- channels, so the `profile_starts_with_every_channel` trigger gives each of
-- them every channel enabled; the statements below are what make the
-- membership mixed.

INSERT INTO channel_profile (id, name) VALUES (1001, 'Living Room'), (1002, 'Kids');

-- `Living Room` carries the first four channels and nothing else.
UPDATE channel_profile_membership SET enabled = 0
    WHERE channel_profile_id = 1001 AND channel_id NOT IN (1000, 1001, 1002, 1003);
-- `Kids` carries two of the awkwardly named ones.
UPDATE channel_profile_membership SET enabled = 0
    WHERE channel_profile_id = 1002 AND channel_id NOT IN (1005, 1006);
-- And one channel is a member of neither: not disabled in them, absent from
-- them, which is a different row and a different query path.
DELETE FROM channel_profile_membership
    WHERE channel_profile_id IN (1001, 1002) AND channel_id = 1016;

-- The standard user sees `Living Room` and nothing else. With no rows here a
-- user sees every profile, so this is the restricted case.
INSERT INTO user_channel_profile (user_id, channel_profile_id) VALUES (1001, 1001);

-- ------------------------------------------------------------- suggestions

-- The third outcome of EPG matching: scored in the band that needs a human.
INSERT INTO epg_match_suggestion (channel_id, epg_data_id, score, suggested_at) VALUES
    (1002, 1005, 0.62, '2026-09-01 00:00:00.000+00:00');

-- --------------------------------------------------------- jobs and events
--
-- One row per state the scheduler can leave behind, including the one a
-- previous process left `running` — the single-flight guard refuses that key
-- forever until `release_orphans` clears it at boot.

INSERT INTO job (id, key, kind, payload, interval_seconds, state, progress, message,
                 next_run_at, started_at, last_success_at, last_error) VALUES
    (1001, 'epg_refresh:1001', 'epg_refresh', '{"epg_source_id":1001}', 86400, 'success', 1.0,
     '6 guide channels, 6 programmes for 4 mapped, 0 auto-matched',
     '2026-12-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:11.000+00:00', NULL),
    -- The dummy source generates on demand, so its job never runs; it exists
    -- because the scheduler registers one per source and because a refresh
    -- triggered against it has to find a row rather than 404.
    (1006, 'epg_refresh:1002', 'epg_refresh', '{"epg_source_id":1002}', 86400, 'idle', 0, NULL,
     '2026-12-01 00:00:00.000+00:00', NULL, NULL, NULL),
    -- Cancelled today, succeeded yesterday: `last_success_at` is older than
    -- `started_at`, which is the shape every row takes while a run is in
    -- flight and the one a stamp-on-every-finish column cannot produce.
    (1002, 'epg_refresh:1003', 'epg_refresh', '{"epg_source_id":1003}', NULL, 'cancelled', 0.4, NULL,
     NULL, '2026-09-01 00:00:00.000+00:00', '2026-08-31 00:01:00.000+00:00', 'cancelled'),
    (1003, 'm3u_refresh:1001', 'm3u_refresh', '{"m3u_account_id":1001}', 86400, 'success', 1.0,
     '3 streams: 0 new, 0 updated, 3 unchanged, 0 stale, 0 removed',
     '2026-12-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00', '2026-09-01 00:00:05.000+00:00', NULL),
    -- Running and never yet successful, so the page says Never for a reason
    -- that is true. The account refreshing *over* an earlier success is the
    -- other half, and `m3u_refresh:1003` below has it.
    (1004, 'm3u_refresh:1002', 'm3u_refresh', '{"m3u_account_id":1002}', 43200, 'running', 0.35,
     'reconciling', '2026-12-01 00:00:00.000+00:00', '2026-09-01 00:00:00.000+00:00', NULL, NULL),
    (1005, 'm3u_refresh:1003', 'm3u_refresh', '{"m3u_account_id":1003}', NULL, 'failed', 0.05, NULL,
     NULL, '2026-09-01 00:00:00.000+00:00', '2026-08-31 00:00:02.000+00:00',
     'https://provider.example/retired.m3u returned 500 Internal Server Error');

INSERT INTO system_event (id, event_type, occurred_at, channel_uuid, channel_name, details) VALUES
    (1001, 'm3u_refresh', '2026-09-01 00:00:05.000+00:00', NULL, NULL,
     '{"account_name":"Synth Standard","total_processed":3,"streams_created":0}'),
    (1002, 'epg_refresh', '2026-09-01 00:10:00.000+00:00', NULL, NULL,
     '{"source_name":"Synth XMLTV","channels":5,"programs":6}'),
    -- Carries the channel columns, which the two above leave null.
    (1003, 'stream_switch', '2026-09-01 01:00:00.000+00:00',
     'bbbbbbbb-0000-4000-8000-000000001000', 'Synth One',
     '{"from":1001,"to":1002,"reason":"input stalled"}'),
    (1004, 'logout', '2026-09-01 02:00:00.000+00:00', NULL, NULL, '{"user":"synthadmin"}');

-- ----------------------------------------------------------- notifications
--
-- The operator's inbox, as distinct from the event log above: three rows that
-- between them cover every state the list can be in — fresh, recurring, and
-- acknowledged-but-still-recurring. README.md says why each.

INSERT INTO notification (id, kind, subject, severity, title, message, detail, occurrences,
                          created_at, updated_at, acknowledged_at) VALUES
    -- Recurring and unacknowledged: raised again by every nightly refresh
    -- since it first appeared, which is what `occurrences` is for.
    (1001, 'm3u.filter_broken', 'account:1003', 'warning',
     'Filters skipped on Synth Retired',
     '1 of this account''s filter patterns will not compile, so every refresh keeps more streams than the filters intended: `^(unclosed`. Fix or remove them under the account''s filters.',
     '{"m3u_account_id":1003,"patterns":[{"pattern":"^(unclosed","message":"unclosed group"}]}',
     12, '2026-08-20 03:00:00.000+00:00', '2026-09-01 03:00:00.000+00:00', NULL),
    -- Acknowledged, and still recurring: the row that proves a recurrence does
    -- not un-dismiss what the operator has already decided to live with. It is
    -- also the only row filling `acknowledged_at`, so the response shape pins
    -- that column as a string rather than as null.
    (1002, 'auto_sync.range_full', 'group:1002', 'error',
     'Channel numbers exhausted in Synth News',
     'Auto sync ran out of numbers between 200 and 299, so 3 new streams were stored without a channel and will not appear in any output. Widen the range or remove some channels.',
     '{"channel_group_id":1002,"m3u_account_id":1001,"range_start":200,"range_end":299,"streams_without_a_channel":3}',
     3, '2026-08-25 03:00:00.000+00:00', '2026-09-01 03:00:00.000+00:00', '2026-08-26 09:15:00.000+00:00'),
    -- Fresh: one occurrence, never seen, and `info` rather than a fault — the
    -- server declined to do something destructive and is saying so.
    (1003, 'm3u.streams_kept', 'account:1001', 'info',
     'Streams kept on Synth Standard',
     '2 streams the provider stopped carrying were kept rather than deleted: each is the only stream on a channel, and deleting it would leave that channel unplayable.',
     '{"m3u_account_id":1001,"stream_ids":[1002,1003]}',
     1, '2026-09-01 04:00:00.000+00:00', '2026-09-01 04:00:00.000+00:00', NULL);
