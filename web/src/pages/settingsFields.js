/**
 * Presentation metadata for the settings the backend is known to store.
 *
 * Keyed `group.field`. `type` and `nullable` are declared rather than inferred
 * from the value's runtime type, because inference cannot see either of the two
 * cases that matter: a nullable number reads as `null` when unset, so it would
 * render as a text box that can never be given a number, and a nullable string
 * reads as a string, so clearing it would post `""` where the server wants
 * `null` and answer 400.
 *
 * A field with no entry here still renders — the control falls back to the
 * runtime type — so a field the server adds shows up without a change here,
 * just with a machine-derived label.
 *
 * @typedef {object} FieldMeta
 * @property {string} [label]
 * @property {string} [help]
 * @property {string} [unit]
 * @property {'number'|'string'|'boolean'|'tags'|'reference'|'hashKeys'} [type]
 * @property {string} [resource] For `reference`: which list the options come from.
 * @property {boolean} [nullable] Empty means `null`, not `0` or `""`.
 * @property {boolean} [hidden] Stored and carried across, but nothing here
 *   reads it; showing it would be a knob that does nothing.
 */
export const FIELD_META = {
  'stream_settings.default_user_agent': {
    type: 'reference',
    resource: 'userAgents',
    nullable: true,
    label: 'Default user agent',
    help:
      'The User-Agent sent to a provider when nothing more specific names one. Playback uses ' +
      "the stream profile's, then the account's, then this; a playlist refresh uses the " +
      "account's, then this; a guide download always uses this. Not set, requests identify " +
      'as dollet-relay.',
  },
  'stream_settings.default_stream_profile': {
    type: 'reference',
    resource: 'streamProfiles',
    nullable: true,
    label: 'Default stream profile',
    help:
      'The profile a stream plays through when neither it, its channel nor its account names ' +
      'one. proxy relays the bytes as they arrive, with no subprocess, and is the right answer ' +
      'for MPEG-TS sources — an HDHomeRun, Xtream .ts URLs. ffmpeg remuxes anything, including ' +
      'HLS, at the cost of one process per channel being watched. redirect sends the player to ' +
      'the provider itself, so there is no failover and no output profile. Not set behaves as ' +
      'proxy.',
  },
  'stream_settings.m3u_hash_key': {
    type: 'hashKeys',
    label: 'Stream identity key',
    help:
      'Which fields identify a stream across refreshes, for every account. m3u_id is the ' +
      'account the stream came from; on an Xtream account, url stands for the provider’s ' +
      'stream id, because its URLs carry credentials that rotate. Changing it makes every ' +
      "existing stream stop matching: each account's next refresh reports its whole catalogue " +
      'as new, a group that syncs channels automatically gets a second channel for each, and ' +
      "the old rows go stale and are deleted after the account's stale period — with every " +
      "channel's failover list pointing at rows that are about to go.",
  },
  'stream_settings.hdhr_output_profile_id': {
    type: 'reference',
    resource: 'outputProfiles',
    nullable: true,
    label: 'HDHR output profile',
    help:
      'Applied to the stream URLs of an HDHomeRun tuner whose URL names no output profile — ' +
      '/hdhr/ and /hdhr/<channel profile>/ — so Plex gets, say, AC3 audio without every other ' +
      'client paying for it. A tuner added under …/output_profile/<id>/ keeps its own, and the ' +
      'M3U and Xtream outputs never use this. Leave unset for no transcoding.',
  },

  'proxy_settings.buffering_timeout': {
    label: 'Buffering timeout',
    unit: 'seconds',
    help:
      "How long a stream profile's command may stay below the buffering speed before the " +
      'channel moves to its next source; a channel with one source restarts it. An output ' +
      'profile has nowhere to move to and is only marked buffering on the Stats page. A source ' +
      'that stops sending altogether is a separate check, with a fixed 20-second limit.',
  },
  'proxy_settings.buffering_speed': {
    label: 'Buffering speed',
    help:
      'The pace, as a multiple of real time, below which a command counts as buffering: 1.0 ' +
      "is real time. Read from ffmpeg's speed= and VLC's buffering messages, so proxy and " +
      'streamlink are never measured. ffmpeg at -loglevel error, as the seeded profiles run ' +
      'it, prints no speed; add -stats to a profile for this to apply to it.',
  },
  'proxy_settings.ring_seconds': {
    label: 'Ring retention',
    unit: 'seconds',
    help:
      'How much of each running stream is held in memory: how far behind live a joining ' +
      'client can start, and how far a slow one can fall behind before it skips ahead. Costs ' +
      'about 1 MB per second at 8 Mbps, per channel, and again for each output profile in use ' +
      'on it. The hard cap does not follow this; raise both together, or the cap binds first ' +
      'on high-bitrate sources.',
  },
  'proxy_settings.ring_max_bytes': {
    label: 'Ring hard cap',
    unit: 'bytes',
    help:
      'Upper bound on a single ring regardless of bitrate. The default, 37,500,000, is 15 ' +
      "seconds of a 20 Mbps source, so an HDHomeRun's ~19.4 Mbps ATSC mux keeps the full " +
      'retention. Wherever retention times bitrate exceeds it, this binds first and the ring ' +
      'holds less time than the retention says.',
  },
  'proxy_settings.channel_shutdown_delay': {
    label: 'Channel shutdown delay',
    unit: 'seconds',
    help:
      'How long a channel keeps its provider connection after its last viewer leaves, so ' +
      'switching back within it rejoins the running stream rather than reconnecting. Zero by ' +
      "default: a connection held open for nobody still counts against the account's stream " +
      'limit.',
  },
  'proxy_settings.channel_init_grace_period': {
    label: 'Source connect timeout',
    unit: 'seconds',
    help:
      'How long a source may take to connect and deliver its first bytes before the attempt ' +
      'counts as failed. Each source gets three attempts before the next one is tried, so a ' +
      'source that hangs costs a viewer several times this.',
  },
  'proxy_settings.channel_client_wait_period': {
    label: 'Client wait period',
    unit: 'seconds',
    help:
      'How long a freshly opened session waits for its first client before it counts as idle ' +
      'and is shut down. The viewer who opened it normally attaches at once; this covers the ' +
      'moment in between, so keep it above zero.',
  },
  'proxy_settings.new_client_behind_seconds': {
    label: 'New client starts behind',
    unit: 'seconds',
    help:
      'How far behind live a joining client begins, so its player has data to decode at once ' +
      'instead of waiting for the next chunk. Capped by what the ring holds; a viewer who ' +
      'starts a channel nobody was watching begins at live.',
  },

  'system_settings.preferred_region': {
    label: 'Preferred region',
    type: 'string',
    nullable: true,
    help:
      'A two-letter country code in lower case (us, not US), compared with the suffix guide ' +
      'ids carry after a dot: us favours vrix.us over vrix.uk, and counts against any guide ' +
      'channel whose id names another country. Leave unset unless your guides cover several ' +
      'countries: a wrong region is a channel showing someone else’s listings.',
  },
  'system_settings.max_system_events': {
    label: 'System events kept',
    help:
      'How many system events are kept. The oldest beyond this are deleted each time a new ' +
      'one is written; the Stats page lists the newest 50.',
  },

  'epg_settings.epg_auto_match_on_refresh': {
    label: 'Auto-match on refresh',
    help:
      'Lets a refresh assign guide data to channels that have none: a guide refresh tries ' +
      'every unmapped channel against that guide, and a playlist refresh tries the channels ' +
      'it just created against every guide. Off by default: on, it runs unattended on every ' +
      'scheduled refresh, and a wrong guide on a channel is harder to notice than no guide at ' +
      'all. Match unmapped channels on the Sources page runs the same match on demand.',
  },
  'epg_settings.epg_match_ignore_prefixes': {
    label: 'Ignored name prefixes',
    help:
      'Removed from the start of a channel or guide name before matching, so "US: VRIX" is ' +
      'compared as "VRIX". Case-sensitive, and only the first entry that matches is removed.',
  },
  'epg_settings.epg_match_ignore_suffixes': {
    label: 'Ignored name suffixes',
    help:
      'Removed from the end of a name the same way, so with FHD listed, "VRIX FHD" is ' +
      'compared as "VRIX". HD, UHD, TV and resolutions like 1080p are ignored already and need ' +
      'no entry.',
  },
  'epg_settings.epg_match_ignore_custom': {
    label: 'Other ignored fragments',
    help:
      'Removed wherever they appear in a name, every occurrence, before matching. Plain text ' +
      'rather than whole words, and case-sensitive: an entry of UK also changes UKTV.',
  },

  'numbering_settings.group_block_size': {
    type: 'number',
    label: 'Group block size',
    help:
      'How wide a number range Assign ranges on the Groups page gives each group that has ' +
      'none. Blocks start above every number already in use, so 100 hands out ranges like ' +
      '300–399 and 400–499. A range already assigned keeps its width.',
  },
  'numbering_settings.channel_step': {
    type: 'number',
    label: 'Channel step',
    help:
      "Spacing between the numbers a group's range hands out, and the grid a renumber lays a " +
      'group out on. 1 appends; 10 leaves nine free numbers between neighbours for a channel ' +
      'that arrives later. Changing it moves no existing channel.',
  },
};

/** Order the sections appear in, most-used first. Unknown groups follow. */
export const GROUP_ORDER = [
  'stream_settings',
  'proxy_settings',
  'epg_settings',
  'numbering_settings',
  'system_settings',
  'network_access',
];

export const GROUP_HELP = {
  proxy_settings:
    'Read once per run of the server, the first time anything uses the streaming engine: a ' +
    'change here takes effect after a restart.',
  epg_settings:
    'These shape automatic guide matching: Match unmapped channels on the Sources page, and ' +
    'matching on refresh when it is on. Only a channel with no guide is matched; one mapped by ' +
    'hand is never changed.',
  network_access:
    'Comma-separated CIDRs or single addresses per endpoint class, checked on every request. ' +
    'An endpoint with no entry is open to everyone; one with entries refuses every other ' +
    'address. The address checked is the connection’s own unless DOLLET_TRUSTED_PROXIES names ' +
    'it as a proxy, so behind a reverse proxy, set that first.',
};

/**
 * Every token `m3u_hash_key` accepts, in the order the server sorts them.
 *
 * The order does not change the hash — the backend sorts the object's keys
 * before hashing it — so storing one canonical order is what stops the same
 * set of fields, picked in a different sequence, reading as a changed setting.
 *
 * Anything else in the stored string is dropped on sight, matching
 * `sync::hash::parse_keys`: a token the backend does not recognise takes no
 * part in the key, so showing it selected would misrepresent what is hashed.
 */
export const HASH_KEY_TOKENS = ['group', 'm3u_id', 'name', 'tvg_id', 'url'];

/** `"url, name"` -> `['name', 'url']`, known tokens only, deduplicated. */
export function parseHashKeys(value) {
  const chosen = new Set(
    String(value)
      .split(',')
      .map((token) => token.trim()),
  );
  return HASH_KEY_TOKENS.filter((token) => chosen.has(token));
}

/** The inverse, in canonical order. */
export function joinHashKeys(tokens) {
  return HASH_KEY_TOKENS.filter((token) => tokens.includes(token)).join(',');
}

/**
 * The endpoint classes `network_access` gates, as `api::auth` and
 * `api::network` name them.
 *
 * Rendered from this list rather than from the stored map's keys, because the
 * map is empty on a fresh or imported instance, and rendering its keys would
 * show the section as nothing but its help line.
 */
export const NETWORK_ENDPOINTS = [
  {
    key: 'UI',
    label: 'Web app and API',
    help:
      'Signing in, every /api/ call made with a login or an API key, and the live /ws feed. ' +
      'The page itself still loads from anywhere.',
    warning:
      'Restricting this to a network you are not on locks you out of this page. The way back ' +
      'in is a browser on an allowed network, or setting network_access back to {} in ' +
      'dollet.sqlite.',
  },
  {
    key: 'M3U_EPG',
    label: 'Playlist, guide and HDHomeRun',
    help:
      '/output/m3u, /output/epg and everything under /hdhr/, which carry no credentials. Plex ' +
      'has to be on an allowed address, or its tuner stops answering.',
  },
  {
    key: 'STREAMS',
    label: 'Streams',
    help: '/proxy/ts/stream/ and the Xtream live URLs — the addresses that actually play video.',
  },
  {
    key: 'XC_API',
    label: 'Xtream Codes API',
    help: 'player_api.php, panel_api.php, get.php and xmltv.php.',
  },
];

/**
 * Why a field cannot be saved, keyed by field name.
 *
 * One rule so far, and it is here rather than left to the server because the
 * server accepts an empty `m3u_hash_key` happily and only refuses much later,
 * when a refresh finds every stream hashing to the same value.
 */
export function fieldErrors(groupKey, draft) {
  const errors = {};
  for (const field of Object.keys(draft)) {
    const meta = FIELD_META[`${groupKey}.${field}`];
    if (meta?.type === 'hashKeys' && parseHashKeys(draft[field]).length === 0) {
      errors[field] =
        'Pick at least one field. With none, every stream in an account hashes alike and the ' +
        'next refresh refuses to run.';
    }
  }
  return errors;
}

/** `ring_max_bytes` -> `Ring max bytes`, for fields with no metadata entry. */
export function humanize(field) {
  const words = field.replace(/[_-]+/g, ' ').trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}
