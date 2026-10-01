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
    help: 'The User-Agent sent to a provider when neither the account nor the stream profile names one.',
  },
  'stream_settings.default_stream_profile': {
    type: 'reference',
    resource: 'streamProfiles',
    nullable: true,
    label: 'Default stream profile',
    help:
      'proxy relays the bytes as they arrive, with no subprocess, and is the right answer for ' +
      'MPEG-TS sources — an HDHomeRun, Xtream .ts URLs. ffmpeg remuxes anything, including HLS, ' +
      'at the cost of one process per active stream. A channel or an account can override this.',
  },
  'stream_settings.m3u_hash_key': {
    type: 'hashKeys',
    label: 'Stream identity key',
    help:
      'Which fields identify a stream across refreshes. Changing it makes every existing stream ' +
      'stop matching: the next refresh reports the whole catalogue as new, and the old rows go ' +
      "stale and are deleted after the account's stale period — with every channel's failover " +
      'list pointing at rows that are about to go.',
  },
  'stream_settings.hdhr_output_profile_id': {
    type: 'reference',
    resource: 'outputProfiles',
    nullable: true,
    label: 'HDHR output profile',
    help:
      'Applied to HDHomeRun lineup URLs when the URL names no profile, so Plex gets, say, AC3 ' +
      'audio without every other client paying for it. Leave unset for no transcoding.',
  },

  'proxy_settings.buffering_timeout': {
    label: 'Buffering timeout',
    unit: 'seconds',
    help: 'Seconds without new bytes before the input counts as stalled and failover starts.',
  },
  'proxy_settings.buffering_speed': {
    label: 'Buffering speed',
    help:
      'ffmpeg speed below this means a transcode cannot keep up with real time, which a viewer ' +
      'sees as buffering. 1.0 is real time; held below it for the buffering timeout, the ' +
      'source is failed over.',
  },
  'proxy_settings.ring_seconds': {
    label: 'Ring retention',
    unit: 'seconds',
    help: 'How much of each stream is held in memory. Higher values cost roughly 1 MB per second per channel at 8 Mbps.',
  },
  'proxy_settings.ring_max_bytes': {
    label: 'Ring hard cap',
    unit: 'bytes',
    help: 'Upper bound on a single ring regardless of bitrate.',
  },
  'proxy_settings.channel_shutdown_delay': {
    label: 'Channel shutdown delay',
    unit: 'seconds',
    help: 'Grace period after the last client leaves, so channel surfing does not restart the stream.',
  },
  'proxy_settings.channel_init_grace_period': {
    label: 'Channel init grace period',
    unit: 'seconds',
    help: 'How long a source may take to connect and deliver its first bytes before the next one is tried.',
  },
  'proxy_settings.channel_client_wait_period': {
    label: 'Client wait period',
    unit: 'seconds',
    help: 'How long a freshly opened session waits for its first client before it counts as idle and is shut down.',
  },
  'proxy_settings.new_client_behind_seconds': {
    label: 'New client starts behind',
    unit: 'seconds',
    help: 'How far behind live a joining client begins, so it has something buffered before the first read.',
  },

  'system_settings.preferred_region': {
    label: 'Preferred region',
    type: 'string',
    nullable: true,
    help:
      "Biases guide matching towards one country's channels. Leave unset unless the guide " +
      'covers several countries: a wrong region is a channel showing someone else’s listings.',
  },
  'system_settings.max_system_events': {
    label: 'System events kept',
    help: 'Older events are trimmed past this count.',
  },

  'epg_settings.epg_auto_match_on_refresh': {
    label: 'Auto-match on refresh',
    help:
      'Lets a scheduled guide refresh assign guide data to channels that have none. Off by ' +
      'default: on, a timer rewrites channel-to-guide mappings across the catalogue unattended, ' +
      'and a wrong guide on a channel is harder to notice than no guide at all. The Guide page ' +
      'runs the same match on demand.',
  },
  'epg_settings.epg_match_ignore_prefixes': {
    label: 'Ignored name prefixes',
    help: 'Stripped before fuzzy matching, so "US: VRIX" reaches "VRIX".',
  },
  'epg_settings.epg_match_ignore_suffixes': {
    label: 'Ignored name suffixes',
    help: 'Stripped before fuzzy matching, so "VRIX HD" reaches "VRIX".',
  },
  'epg_settings.epg_match_ignore_custom': {
    label: 'Other ignored fragments',
    help: 'Stripped before fuzzy matching wherever they appear in a name.',
  },

  'numbering_settings.group_block_size': {
    type: 'number',
    label: 'Group block size',
    help: 'How wide a number range a group gets when one is assigned for it on the Groups page.',
  },
  'numbering_settings.channel_step': {
    type: 'number',
    label: 'Channel step',
    help: 'Spacing between the numbers a range hands out. 1 appends; 10 leaves nine free slots between neighbours.',
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
  network_access:
    'Comma-separated CIDRs per endpoint class. An endpoint with no entry is open to everyone.',
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
    help: 'The web app and /api/.',
    warning:
      'Restricting this to a network you are not on locks you out of this page; the way back ' +
      'in is DOLLET_TRUSTED_PROXIES or a direct connection from an allowed address.',
  },
  {
    key: 'M3U_EPG',
    label: 'Playlist, guide and HDHomeRun',
    help: '/output/m3u, /output/epg and the HDHomeRun discovery and lineup, which carry no credentials.',
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
