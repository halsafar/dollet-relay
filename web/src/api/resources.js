import { api, rootApi } from './client.js';
import { discoverPath } from '../pages/connectUrls.js';

/**
 * Every endpoint the UI talks to, in one file.
 *
 * Verified against `crates/dollet-server/src/api/`. Paths are stable, so a
 * client configured against an earlier deployment keeps working:
 *
 * ```
 * GET    /api/accounts/initialize-superuser/                  -> {superuser_exists}
 * POST   /api/accounts/initialize-superuser/ {username, password}
 *                                                             -> {access, refresh}
 * POST   /api/accounts/token/            {username, password} -> {access, refresh}
 * POST   /api/accounts/token/refresh/    {refresh}            -> {access}
 * GET    /api/accounts/users/me/                              -> User
 * GET    /api/accounts/users/                                 -> List<User>
 * POST   /api/accounts/users/            NewUser              -> User (201)
 * PATCH  /api/accounts/users/{id}/       Partial<User>        -> User
 * DELETE /api/accounts/users/{id}/                            -> 204
 * GET    /api/core/settings/                                  -> List<SettingGroup>
 * PATCH  /api/core/settings/{key}/       {value: Partial}     -> SettingGroup
 * GET    /api/core/version/                                   -> {version}
 * GET    /api/core/settings/env/                              -> {version,
 *                                                                 advertised_base_url,
 *                                                                 artwork_base_url}
 * GET    /api/core/origins/                                   -> List<SeenOrigin>
 * GET    /api/core/system-events/?limit=                      -> List<SystemEvent> (+)
 * GET    /api/core/jobs/                                      -> List<Job>
 * POST   /api/core/jobs/{key}/cancel/                         -> {cancelling}
 * GET    /api/notifications/                                  -> List<Notification>
 * GET    /api/notifications/count/                            -> {unacknowledged}
 * POST   /api/notifications/{id}/acknowledge/                 -> Notification
 * POST   /api/notifications/acknowledge-all/                  -> {acknowledged}
 * DELETE /api/notifications/{id}/                             -> 204
 *
 * GET    /api/m3u/accounts/                                   -> List<M3uAccount>
 * POST   /api/m3u/accounts/           AccountBody             -> M3uAccount (201)
 * PATCH  /api/m3u/accounts/{id}/      AccountBody             -> M3uAccount
 * DELETE /api/m3u/accounts/{id}/                              -> 204
 * GET    /api/m3u/accounts/{id}/profiles/                     -> List<M3uProfile>
 * POST   /api/m3u/accounts/{id}/profiles/                     -> M3uProfile (201)
 * PATCH  /api/m3u/accounts/{a}/profiles/{id}/                 -> M3uProfile
 * DELETE /api/m3u/accounts/{a}/profiles/{id}/                 -> 204
 * GET    /api/m3u/accounts/{id}/filters/                      -> List<M3uFilter>
 * POST   /api/m3u/accounts/{id}/filters/                      -> M3uFilter (201)
 * DELETE /api/m3u/accounts/{a}/filters/{id}/                  -> 204
 * GET    /api/m3u/accounts/{id}/groups/                       -> List<GroupLink>
 * POST   /api/m3u/accounts/{id}/groups/  GroupBody            -> GroupLink
 * POST   /api/m3u/refresh/                                    -> {started: string[]}
 * POST   /api/m3u/refresh/{id}/                               -> {started, job}
 *
 * GET    /api/epg/sources/                                    -> List<EpgSource>
 * POST   /api/epg/sources/            SourceBody              -> EpgSource (201)
 * PATCH  /api/epg/sources/{id}/       SourceBody              -> EpgSource
 * DELETE /api/epg/sources/{id}/                               -> 204
 * POST   /api/epg/refresh/                                    -> {started: string[]}
 * POST   /api/epg/refresh/{id}/                               -> {started, job}
 * GET    /api/epg/ambiguous/                                  -> List<AmbiguousMatch>
 * GET    /api/epg/epgdata/?search=&limit=                     -> List<EpgData> (+)
 * POST   /api/epg/match/                       -> {matched, need_a_decision}
 * GET    /api/epg/grid/?from=&to=&channel_profile=             -> Grid
 * POST   /api/epg/suggestions/{channel_id}/                    -> {epg_data_id}
 * DELETE /api/epg/suggestions/{channel_id}/                    -> 204
 *
 * GET    /api/proxy/stats/                                    -> SessionStats[] (*)
 * POST   /api/proxy/ts/stop/{uuid}                            -> {stopped: n}
 * POST   /api/proxy/ts/stop_client/{uuid}?client_id=          -> {disconnected: true}
 * POST   /api/proxy/ts/next_stream/{uuid}                     -> {switched, source_index}
 * POST   /api/proxy/ts/change_stream/{uuid}?stream_id=        -> {switched, source_index}
 *
 * Mounted at the root rather than under /api, because a client outside this
 * project holds the URL:
 * WS     /ws                        subprotocol ['auth.jwt', <access token>]
 * GET    /api/core/streamprofiles/                            -> List<StreamProfile>
 * GET    /api/core/useragents/                                -> List<UserAgent>
 * GET    /api/core/outputprofiles/                            -> List<OutputProfile>
 * GET    /hdhr/discover.json                                  -> {FriendlyName,
 *                                                                 DeviceID, ...}
 *
 * GET    /api/channels/channels/?all=true                     -> List<Channel>
 * GET    /api/channels/channels/?page=&page_size=             -> List<Channel>
 * POST   /api/channels/channels/         ChannelBody          -> Channel (201)
 * PATCH  /api/channels/channels/{id}/    ChannelBody          -> Channel
 * DELETE /api/channels/channels/{id}/                         -> 204
 * POST   /api/channels/channels/bulk-delete/  {ids}           -> {deleted}
 * GET    /api/channels/channels/{id}/streams/            -> List<Stream> failover order
 * PUT    /api/channels/channels/{id}/streams/ {ids}           -> Stream[] (*)
 * POST   /api/channels/channels/{id}/move/ {after, before}    -> {id, channel_number}
 * GET    /api/channels/groups/                                -> List<ChannelGroup>
 * POST   /api/channels/groups/           GroupBody           -> ChannelGroup (201)
 * PATCH  /api/channels/groups/{id}/      GroupBody           -> ChannelGroup
 * DELETE /api/channels/groups/{id}/                          -> 204
 * POST   /api/channels/groups/{id}/renumber/  ?order         -> {renumbered, channels}
 * POST   /api/channels/groups/plan-ranges/   {order: id[]}     -> {block_size, ranges}
 * POST   /api/channels/groups/assign-ranges/ {ranges}          -> {assigned}
 * POST   /api/channels/groups/plan-renumber/  ?order          -> {groups, skipped}
 * POST   /api/channels/groups/renumber-all/   ?order          -> {renumbered, groups, skipped}
 * GET    /api/channels/profiles/                              -> List<ChannelProfile>
 * POST   /api/channels/profiles/{id}/channels/bulk-update/
 *                                        {channel_ids, enabled} -> {updated}
 * GET    /api/channels/streams/?page=&page_size=&search=&channel_group=&ordering=
 *                                                             -> List<Stream>
 * POST   /api/channels/streams/bulk-delete/   {ids}           -> {deleted}
 * GET    /api/channels/logos/?all=true                        -> List<Logo>
 * GET    /api/channels/logos/?page=&page_size=&search=&ordering= -> List<Logo>
 * POST   /api/channels/logos/           {name, url}          -> Logo (201)
 * PATCH  /api/channels/logos/{id}/      {name?, url?}        -> Logo
 * DELETE /api/channels/logos/{id}/                           -> 204
 * POST   /api/channels/logos/bulk-delete/ {ids}              -> {deleted}
 * POST   /api/channels/logos/cleanup/                        -> {deleted}
 * ```
 *
 * `List<T>` is one shape, everywhere: `{results: T[], count, page, pages}`.
 * A list that does not paginate still reports `page: 1, pages: 1`, so nothing
 * has to ask which kind it is looking at. `count` is the size of the whole
 * collection, not of `results` — on a paginated list those differ, and that
 * difference is the only thing a pager can be built from.
 *
 * `?all=true` is the single way to ask for a whole collection, on every
 * endpoint that allows one.
 *
 * One list refuses it. `/api/channels/streams/` can hold tens of thousands of
 * rows, so `?all=true` there is a 400 whose `detail` says so; this client never
 * sends it, because every screen that reads streams pages them.
 *
 * (*) marks the one body that is a bare JSON array: stats is a snapshot of
 * what is playing right now rather than a collection to page through, so an
 * envelope would carry a count nobody reads. The grid is not one either — it
 * is a single object whose `channels` is a plain array, because it is a
 * window rather than a collection.
 *
 * `count` is the size of the collection on every enveloped list, including
 * the two that cap their results (`epgdata` and `system-events`). Those
 * report `pages > 1` when there is more, so a caller can tell a full answer
 * from a truncated one.
 *
 * Settings are whole JSON blobs per section, matching `dollet_core::settings`:
 * `stream_settings`, `proxy_settings`, `network_access`, `system_settings`,
 * `epg_settings`, `numbering_settings`. A PATCH carries only the changed
 * fields; the server merges them into the stored group.
 *
 * @typedef {object} User
 * @property {number} id
 * @property {string} username
 * @property {string | null} email
 * @property {boolean} is_active
 * @property {number} user_level See `USER_LEVELS`.
 * @property {string | null} api_key
 * @property {number} stream_limit 0 means unlimited.
 * @property {{Record<string, unknown>}} [custom_properties] Free-form per-user
 * values. `xc_password` lives here and is what an Xtream player signs in with;
 * the Users page sets it and Connect reads it.
 * @property {number[]} channel_profiles
 *
 * @typedef {object} SettingGroup
 * @property {number} id
 * @property {string} key
 * @property {string} name
 * @property {Record<string, unknown>} value
 *
 * A channel arrives as three views of itself: the base row provider sync
 * writes, the `override` the user writes, and the `effective_*` values every
 * output reads. Display the effective ones; edit the base ones.
 *
 * @typedef {object} Channel
 * @property {number} id
 * @property {string} uuid
 * @property {string} name Base row, before overrides.
 * @property {number | null} channel_number Base row. A float — 2.1 is a real
 *   channel number — and null means "unnumbered", never 0.
 * @property {number | null} channel_group_id
 * @property {number | null} logo_id
 * @property {number | null} epg_data_id
 * @property {number | null} stream_profile_id
 * @property {boolean} hidden_from_output
 * @property {number[]} streams Stream ids in failover order.
 * @property {object | null} override Null when nothing is overridden.
 * @property {string} effective_name
 * @property {number | null} effective_channel_number
 * @property {string | null} effective_tvg_id
 * @property {number | null} effective_epg_data_id
 * @property {string | null} epg_name Name of the guide channel the mapping
 *   points at. Null when the channel is mapped to nothing — which is a
 *   different question from `effective_tvg_id`, a provider label that says
 *   nothing about whether listings exist.
 * @property {string | null} group_name
 * @property {string | null} logo_url
 *
 * @typedef {object} Stream
 * @property {number} id
 * @property {string} name
 * @property {string | null} url
 * @property {number | null} channel_group_id
 * @property {number | null} m3u_account_id
 * @property {boolean} is_custom
 *
 * @typedef {object} GroupLink
 * @property {number} m3u_account_id
 * @property {boolean} enabled Whether that provider's streams in the group are imported.
 * @property {boolean} auto_channel_sync Whether its new streams become channels at refresh.
 * @property {'range'|'provider'} numbering_mode Where that provider's channels get their numbers.
 *
 * @typedef {object} ChannelGroup
 * @property {number} id
 * @property {string} name
 * @property {number | null} number_start Where the group's channels are numbered from.
 * @property {number | null} number_end Inclusive; null means unbounded.
 * @property {number} channel_count
 * @property {number} stream_count
 * @property {GroupLink[]} links One per provider that has ever offered the group.
 *
 * @typedef {object} Logo
 * @property {number} id
 * @property {string} name
 * @property {string} url
 * @property {number} channel_count Channels using it, counting overrides.
 * @property {boolean} is_used
 */

/**
 * Levels are the integers the database stores, so `>=` works in SQL and in the
 * UI. An unknown value rounds down, matching `dollet_core::auth::level_from_i64`.
 */
export const USER_LEVELS = { STREAMER: 0, STANDARD: 1, ADMIN: 10 };

export const USER_LEVEL_LABELS = {
  [USER_LEVELS.STREAMER]: 'Streamer',
  [USER_LEVELS.STANDARD]: 'Standard',
  [USER_LEVELS.ADMIN]: 'Administrator',
};

/**
 * Reads the `{results, count, page, pages}` envelope every `/api/` list sends.
 *
 * Throws on anything else, and that is the whole point: a reader that returns
 * `[]` for a body it does not recognise makes "the collection is empty" and
 * "the body was not what I expected" the same blank table. A renamed field, a
 * 200 carrying an error object, a proxy returning something else entirely: all
 * of them would read as a working screen with nothing in it.
 *
 * @template T
 * @param {unknown} body
 * @param {string} endpoint Named in the message, since the throw is what the
 *   user will see and "a list came back wrong" is not actionable.
 * @returns {{results: T[], count: number, page: number, pages: number}}
 */
function listing(body, endpoint) {
  if (
    body &&
    typeof body === 'object' &&
    Array.isArray(body.results) &&
    typeof body.count === 'number'
  ) {
    return body;
  }
  throw shapeError(endpoint, body, '{results, count, page, pages}');
}

/** The rows alone, for the lists nothing pages. */
function rows(body, endpoint) {
  return listing(body, endpoint).results;
}

/**
 * The bodies that are a bare JSON array rather than an envelope.
 *
 * `/api/proxy/stats/` stays one: it is a snapshot of what is playing now, not
 * a collection to page through. The grid's `channels` is a field rather than
 * a collection.
 *
 * An envelope is accepted here too, so a server one shape ahead of this client
 * does not blank the page. Anything that is neither still throws.
 *
 * @template T
 * @param {unknown} body
 * @param {string} endpoint
 * @returns {T[]}
 */
function bareRows(body, endpoint) {
  if (Array.isArray(body)) return body;
  if (body && typeof body === 'object' && Array.isArray(body.results)) {
    return body.results;
  }
  throw shapeError(endpoint, body, 'a JSON array');
}

/**
 * A plain `Error`, not an `ApiError`: the request succeeded. There is no HTTP
 * status that means "the body was not what the contract says", and borrowing
 * one would make this look like a server failure in every log that reads it.
 */
function shapeError(endpoint, body, expected) {
  return new Error(`${endpoint}: expected ${expected}, got ${describe(body)}.`);
}

/** Enough of the body to tell a rename from an outage, and no payload with it. */
function describe(body) {
  if (Array.isArray(body)) return 'an array';
  if (body === null) return 'null';
  if (typeof body !== 'object') return typeof body;
  const keys = Object.keys(body);
  return keys.length === 0 ? 'an empty object' : `an object with {${keys.join(', ')}}`;
}

export const auth = {
  /**
   * Whether this instance already has an administrator.
   *
   * Unauthenticated by necessity: on an empty database there is nobody to
   * authenticate as, and without this the login form 401s forever and the only
   * way in is curl.
   *
   * @returns {Promise<{superuser_exists: boolean}>}
   */
  setupStatus: () => api.get('/accounts/initialize-superuser/', { auth: false }),

  /**
   * Creates the first administrator and signs in as them.
   *
   * Refused once one exists, so this cannot be used to mint a second.
   *
   * @returns {Promise<{access: string, refresh: string}>}
   */
  bootstrap: (username, password) =>
    api.post('/accounts/initialize-superuser/', { username, password }, { auth: false }),

  /** @returns {Promise<{access: string, refresh: string}>} */
  login: (username, password) =>
    api.post('/accounts/token/', { username, password }, { auth: false }),

  /** @returns {Promise<User>} */
  me: () => api.get('/accounts/users/me/'),
};

export const channels = {
  /**
   * The count for the sidebar badge. `page_size=1` turns the paginated envelope
   * on and asks for one row, so this stays a badge query rather than pulling
   * the whole lineup to call `.length` on it.
   *
   * @returns {Promise<number | null>}
   */
  count: async () => {
    try {
      const body = await api.get('/channels/channels/', { query: { page_size: 1 } });
      return listing(body, '/api/channels/channels/').count;
    } catch {
      return null;
    }
  },

  /**
   * The whole lineup, which is what lets the table sort and filter client-side.
   *
   * `all=true` is the ask. Dropping it would not fail — it would return the
   * first fifty channels and a table that looks complete.
   *
   * @returns {Promise<Channel[]>}
   */
  list: async () =>
    rows(
      await api.get('/channels/channels/', { query: { all: 'true' } }),
      '/api/channels/channels/',
    ),

  /** @returns {Promise<Channel>} */
  create: (payload) => api.post('/channels/channels/', payload),
  /** @returns {Promise<Channel>} */
  update: (id, payload) => api.patch(`/channels/channels/${id}/`, payload),
  remove: (id) => api.delete(`/channels/channels/${id}/`),

  /** @returns {Promise<{deleted: number}>} */
  bulkDelete: (ids) => api.post('/channels/channels/bulk-delete/', { ids }),

  /** In failover order. @returns {Promise<Stream[]>} */
  streams: async (id) =>
    rows(
      await api.get(`/channels/channels/${id}/streams/`),
      '/api/channels/channels/{id}/streams/',
    ),

  /**
   * Replaces the failover list. Order is the whole payload: position 0 is the
   * stream the engine tries first, and every later one is a fallback.
   *
   * Read loosely through `bareRows` so a server one shape behind this client
   * still works rather than going blank.
   *
   * @returns {Promise<Stream[]>}
   */
  setStreams: async (id, ids) =>
    bareRows(
      await api.request(`/channels/channels/${id}/streams/`, {
        method: 'PUT',
        body: { ids },
      }),
      'PUT /api/channels/channels/{id}/streams/',
    ),

  /**
   * Put the channel between two others, by giving it a number in the gap
   * between theirs. Writes one number and moves nothing else; refuses when
   * the pair has no room left, rather than pushing the rest of the group down.
   *
   * `after` and `before` are the rows either side of the drop, and either may
   * be null at the ends of a page — the server finds the real neighbour.
   *
   * @param {number} id
   * @param {{after: number|null, before: number|null}} between
   * @returns {Promise<{id: number, channel_number: number}>}
   */
  move: (id, between) => api.post(`/channels/channels/${id}/move/`, between),
};

export const channelGroups = {
  /** @returns {Promise<ChannelGroup[]>} */
  list: async () => rows(await api.get('/channels/groups/'), '/api/channels/groups/'),
  /** `{name, number_start?, number_end?}`. */
  create: (payload) => api.post('/channels/groups/', payload),
  /** Any of `name`, `number_start`, `number_end`; an explicit null clears a bound. */
  update: (id, payload) => api.patch(`/channels/groups/${id}/`, payload),
  remove: (id) => api.delete(`/channels/groups/${id}/`),

  /**
   * Walk the group's channels through its range. The one call that moves
   * numbers already assigned; Plex needs a re-scan afterwards.
   *
   * `order` decides what the walk follows: omitted or `current` keeps the
   * lineup's own order and only compacts it, `name`, `guide` and `tvg_id`
   * re-sort the group.
   *
   * @param {number} id
   * @param {'current'|'name'|'guide'|'tvg_id'} [order]
   * @returns {Promise<{renumbered: number, channels: {id: number, channel_number: number}[]}>}
   */
  renumber: (id, order) =>
    api.post(`/channels/groups/${id}/renumber/`, undefined, { query: { order } }),

  /**
   * Blocks for the groups that have no range, in the order given, without
   * writing them. `assignRanges` takes the result back once the operator has
   * seen it.
   *
   * @param {number[]} order
   * @returns {Promise<{block_size: number, ranges: {id: number, name: string, number_start: number, number_end: number}[]}>}
   */
  planRanges: (order) => api.post('/channels/groups/plan-ranges/', { order }),
  /** @returns {Promise<{assigned: number}>} */
  assignRanges: (ranges) => api.post('/channels/groups/assign-ranges/', { ranges }),
  /**
   * Every group `renumberAll` would lay out, and the ones it would leave
   * alone with the reason, without writing anything.
   */
  planRenumber: (order) =>
    api.post('/channels/groups/plan-renumber/', undefined, { query: { order } }),
  /** @returns {Promise<{renumbered: number, groups: number, skipped: object[]}>} */
  renumberAll: (order) =>
    api.post('/channels/groups/renumber-all/', undefined, { query: { order } }),
};

export const channelProfiles = {
  /** @returns {Promise<{id: number, name: string, channels: object[]}[]>} */
  list: async () => rows(await api.get('/channels/profiles/'), '/api/channels/profiles/'),

  /** @returns {Promise<{updated: number}>} */
  setMembership: (profileId, channelIds, enabled) =>
    api.post(`/channels/profiles/${profileId}/channels/bulk-update/`, {
      channel_ids: channelIds,
      enabled,
    }),
};

export const streams = {
  /**
   * Server-paginated, and filtered only the ways the server actually supports:
   * `search` matches stream name or group name, `channel_group` narrows by
   * group, and `ordering` is one of name/group_name/tvg_id/id with an optional
   * `-` prefix.
   *
   * Always paged. This is the one list that refuses `?all=true`, so there is no
   * parameter here to ask for everything — the refusal is a 400, and a control
   * that guarantees one is not a control.
   *
   * @returns {Promise<{results: Stream[], count: number, page: number,
   *                    pages: number}>}
   */
  list: async ({ page = 1, pageSize = 50, search, channelGroup, ordering } = {}) =>
    listing(
      await api.get('/channels/streams/', {
        query: {
          page,
          page_size: pageSize,
          search,
          channel_group: channelGroup,
          ordering,
        },
      }),
      '/api/channels/streams/',
    ),

  /** @returns {Promise<{deleted: number}>} */
  bulkDelete: (ids) => api.post('/channels/streams/bulk-delete/', { ids }),
};

export const logos = {
  /** Every logo at once, which the channel editor's picker needs. */
  all: async () =>
    rows(
      await api.get('/channels/logos/', { query: { all: 'true' } }),
      '/api/channels/logos/',
    ),

  /**
   * One page. `search` matches name or URL; `ordering` is name/url/id with an
   * optional `-` prefix.
   *
   * @returns {Promise<{results: Logo[], count: number, page: number,
   *                    pages: number}>}
   */
  list: async ({ page = 1, pageSize = 50, search, ordering } = {}) =>
    listing(
      await api.get('/channels/logos/', {
        query: { page, page_size: pageSize, search, ordering },
      }),
      '/api/channels/logos/',
    ),

  /** @returns {Promise<Logo>} */
  create: (name, url) => api.post('/channels/logos/', { name, url }),
  /** @returns {Promise<Logo>} */
  update: (id, payload) => api.patch(`/channels/logos/${id}/`, payload),
  remove: (id) => api.delete(`/channels/logos/${id}/`),

  /** @returns {Promise<{deleted: number}>} */
  bulkDelete: (ids) => api.post('/channels/logos/bulk-delete/', { ids }),

  /** Drops every logo no channel references. @returns {Promise<{deleted: number}>} */
  cleanup: () => api.post('/channels/logos/cleanup/'),
};

export const streamProfiles = {
  /** @returns {Promise<{id: number, name: string}[]>} */
  list: async () =>
    rows(await api.get('/core/streamprofiles/'), '/api/core/streamprofiles/'),
};

export const userAgents = {
  /** @returns {Promise<{id: number, name: string, user_agent: string}[]>} */
  list: async () => rows(await api.get('/core/useragents/'), '/api/core/useragents/'),
};

export const outputProfiles = {
  /** @returns {Promise<{id: number, name: string}[]>} */
  list: async () =>
    rows(await api.get('/core/outputprofiles/'), '/api/core/outputprofiles/'),
};

/**
 * What the deployment itself is configured with, as distinct from what is
 * stored in its database.
 *
 * `advertised_base_url` is the one the Connect page turns on: when it is set,
 * every client is handed that base no matter which address it arrived on.
 * `artwork_base_url` splits logo URLs off from it, because those are resolved by
 * whatever browser renders the guide rather than by the server that fetched it.
 *
 * @returns {Promise<{version: string, advertised_base_url: string | null,
 *                    artwork_base_url: string | null}>}
 */
export const environment = {
  get: () => api.get('/core/settings/env/'),
};

/**
 * The addresses clients have actually fetched a lineup, a playlist or a guide
 * on, most recent first.
 *
 * The server cannot know how a client reaches it, so this is the evidence the
 * Connect page offers instead of a guess. Admin-only: it is the list of names
 * this deployment answers to.
 *
 * @typedef {object} SeenOrigin
 * @property {string} base_url
 * @property {('hdhr'|'m3u'|'epg')[]} kinds
 * @property {string} first_seen
 * @property {string} last_seen
 * @property {number} requests
 */
export const origins = {
  /** @returns {Promise<SeenOrigin[]>} */
  list: async () => rows(await api.get('/core/origins/'), '/api/core/origins/'),
};

/**
 * The HDHomeRun tuner's own identity for one scope.
 *
 * Fetched from this browser's origin and without a token, deliberately: it is
 * unauthenticated for Plex's sake, and `FriendlyName` and `DeviceID` do not
 * depend on the base the operator is looking at — only on the channel profile
 * and output profile in the path. Asking the server beats reimplementing its
 * slug rules here and having the two drift.
 */
export const hdhr = {
  /**
   * @param {{channelProfile?: string, outputProfile?: number}} [scope]
   * @returns {Promise<{FriendlyName: string, DeviceID: string,
   *                    TunerCount: number}>}
   */
  discover: (scope) => rootApi.get(discoverPath(scope), { auth: false }),
};

export const users = {
  /** @returns {Promise<User[]>} */
  list: async () => rows(await api.get('/accounts/users/'), '/api/accounts/users/'),
  /** @returns {Promise<User>} */
  create: (payload) => api.post('/accounts/users/', payload),
  /** @returns {Promise<User>} */
  update: (id, payload) => api.patch(`/accounts/users/${id}/`, payload),
  remove: (id) => api.delete(`/accounts/users/${id}/`),
};

/**
 * One entry per `core_setting` row.
 *
 * `name` falls back to `key` because a group the server has not given a display
 * name to should still render as something rather than as an empty accordion
 * header. `value` falls back to `{}` for the same reason — the form underneath
 * reads fields off it.
 *
 * @param {unknown} body
 * @returns {SettingGroup[]}
 */
export function asSettingGroups(body) {
  return rows(body, '/api/core/settings/').map((entry) => ({
    id: entry.id,
    key: entry.key,
    name: entry.name ?? entry.key,
    value: entry.value ?? {},
  }));
}

export const settings = {
  /** @returns {Promise<SettingGroup[]>} */
  list: async () => asSettingGroups(await api.get('/core/settings/')),

  /**
   * The server merges `value` into the stored blob, so sending only the changed
   * fields is what keeps two people editing different sections from clobbering
   * each other.
   *
   * @param {string} key Group key, e.g. `proxy_settings`.
   * @param {Record<string, unknown>} changes
   * @returns {Promise<SettingGroup>}
   */
  update: (key, changes) => api.patch(`/core/settings/${key}/`, { value: changes }),
};

/**
 * Unauthenticated, and only feeds the version label in the sidebar, so a
 * failure is swallowed rather than surfaced.
 *
 * @returns {Promise<string | null>}
 */
export async function fetchVersion() {
  try {
    const body = await api.get('/core/version/', { auth: false });
    return body?.version ?? null;
  } catch {
    return null;
  }
}

/**
 * Live session statistics.
 *
 * Admin-only server-side, and for good reason: every entry carries a channel
 * UUID that works against the anonymous stream endpoint, the upstream URL
 * behind it, and the IP of every client watching.
 *
 * @typedef {object} SessionStats
 * @property {string} channel Channel UUID.
 * @property {{kind: 'raw' | 'profile', profile_id: number | null}} output
 *   Which byte stream this is. Written out server-side rather than derived,
 *   so a consumer reads a field instead of branching on the shape of the
 *   value — see `OutputKey`'s `Serialize` in `dollet-stream`.
 * @property {'connecting'|'streaming'|'buffering'|'switching'|'failed'|'stopped'} phase
 * @property {boolean} healthy
 * @property {number} source_index Position in the failover list, zero-based.
 * @property {number | null} source_id
 * @property {string | null} url The upstream currently being read.
 * @property {number} switches Failovers since the session started.
 * @property {string | null} last_error
 * @property {string} started_at
 * @property {number} total_bytes
 * @property {{chunks: number, bytes: number, head: number, oldest: number | null,
 *             seconds: number}} buffer
 * @property {{input_format: string | null, video_codec: string | null,
 *             width: number | null, height: number | null,
 *             source_fps: number | null, pixel_format: string | null,
 *             video_bitrate_kbps: number | null, audio_codec: string | null,
 *             sample_rate: number | null, audio_channels: string | null,
 *             audio_bitrate_kbps: number | null,
 *             quality: string | null}} media `quality` is the label a source
 *   advertised for itself, verbatim. Only streamlink reports one.
 * @property {{speed: number | null, fps: number | null, actual_fps: number | null,
 *             bitrate_kbps: number | null}} progress
 * @property {{id: string, ip: string | null, user_agent: string | null,
 *             connected_at: string, bytes_sent: number, internal: boolean}[]} clients
 *   `internal` marks the transcode reading this channel rather than a viewer.
 * @property {{state: 'programme'|'gap'|'unmapped'|'unknown', generated?: boolean,
 *             title?: string, sub_title?: string | null,
 *             description?: string | null, start?: string, stop?: string,
 *             elapsed_seconds?: number, remaining_seconds?: number,
 *             duration_seconds?: number}} now_playing
 *   Always present and never null, so absence cannot be read as "still
 *   loading". `gap` means the channel is mapped but nothing covers this
 *   instant; `unmapped` means it has no guide at all, which is something the
 *   user can go and fix; `unknown` means the channel itself has been deleted.
 *   The elapsed and remaining seconds are computed server-side — a browser
 *   clock minutes out of true puts a visibly wrong marker on a half-hour
 *   programme.
 */
export const stats = {
  /** @returns {Promise<SessionStats[]>} */
  get: async () => bareRows(await api.get('/proxy/stats/'), '/proxy/stats/'),

  /**
   * Ends every session for a channel, raw and profiles alike. Everyone
   * watching is disconnected.
   *
   * @returns {Promise<{stopped: number}>}
   */
  stopChannel: (uuid) => api.post(`/proxy/ts/stop/${uuid}`),

  /**
   * Evicts one viewer and leaves the channel up.
   *
   * 404 means the row was stale — the client had already gone. 409 means it
   * was the transcode behind an output profile, which the engine refuses to
   * evict because that would strand the encoder with no input.
   *
   * @returns {Promise<{disconnected: boolean}>}
   */
  stopClient: (uuid, clientId) =>
    api.post(`/proxy/ts/stop_client/${uuid}`, undefined, {
      query: { client_id: clientId },
    }),

  /**
   * Moves a live session to the next source in the channel's failover order.
   *
   * Not a failure: the engine spends no retry and records no error, so an
   * operator reaching for this because a source looks bad does not leave the
   * channel closer to giving up than before.
   *
   * @returns {Promise<{switched: boolean, source_index: number}>}
   */
  nextSource: (uuid) => api.post(`/proxy/ts/next_stream/${uuid}`),

  /**
   * Moves a live session to a specific source.
   *
   * `index` is a position in the channel's failover order, which is the order
   * the Channels editor sets. Switching overrides it for this session only; it
   * does not re-order anything.
   *
   * @returns {Promise<{switched: boolean, source_index: number}>}
   */
  changeSource: (uuid, index) =>
    api.post(`/proxy/ts/change_stream/${uuid}`, undefined, {
      query: { stream_id: index },
    }),
};

/**
 * @typedef {object} SystemEvent
 * @property {number} id
 * @property {string} event_type
 * @property {string} occurred_at
 * @property {string | null} channel_uuid
 * @property {string | null} channel_name
 * @property {unknown} details
 */
export const systemEvents = {
  /** @returns {Promise<SystemEvent[]>} */
  list: async (limit = 100) =>
    rows(
      await api.get('/core/system-events/', { query: { limit } }),
      '/api/core/system-events/',
    ),
};

/**
 * A condition a background job hit that somebody has to act on.
 *
 * The neighbour of `systemEvents` and not the same thing: an event is a record
 * that something happened, a notification is something that is *still true* and
 * has not been looked at. They are deduplicated on the server by `(kind,
 * subject)`, so a fault that recurs nightly arrives as one row with
 * `occurrences` on it rather than as thirty.
 *
 * @typedef {object} Notification
 * @property {number} id
 * @property {string} kind Stable machine key, e.g. `m3u.filter_broken`.
 * @property {string} subject What it is about, e.g. `account:2`.
 * @property {'info' | 'warning' | 'error'} severity
 * @property {string} title
 * @property {string} message
 * @property {Record<string, unknown>} detail Producer-specific ids and counts.
 * @property {number} occurrences
 * @property {string} created_at
 * @property {string} updated_at
 * @property {string | null} acknowledged_at Null while it still wants attention.
 */
export const notifications = {
  /** Unacknowledged first, newest first within each half. @returns {Promise<Notification[]>} */
  list: async () => rows(await api.get('/notifications/'), '/api/notifications/'),

  /**
   * The badge, asked for on mount and every minute after.
   *
   * Its own endpoint rather than `list().length`: the list carries a message
   * and a detail blob per row and this needs one integer. Null on failure, the
   * way the channel count is — a sidebar that throws takes the whole app down
   * over a number.
   *
   * @returns {Promise<number | null>}
   */
  count: async () => {
    try {
      const body = await api.get('/notifications/count/');
      return typeof body?.unacknowledged === 'number' ? body.unacknowledged : null;
    } catch {
      return null;
    }
  },

  /** @returns {Promise<Notification>} */
  acknowledge: (id) => api.post(`/notifications/${id}/acknowledge/`),

  /** @returns {Promise<{acknowledged: number}>} */
  acknowledgeAll: () => api.post('/notifications/acknowledge-all/'),

  /** Gone rather than dismissed: the next refresh that still finds the
   * condition raises it again from one occurrence. */
  remove: (id) => api.delete(`/notifications/${id}/`),
};

/**
 * A provider account, as `m3u::serialize` emits it.
 *
 * The password is **never returned** — only `has_password`. It reaches this
 * server once and leaves it only towards the provider, so there is nothing to
 * leak into a screenshot or a bug report.
 *
 * `status`, `progress`, `last_message`, `updated_at` and `next_run_at` come
 * from the refresh job rather than the account row, which is why an account
 * that has never run reports `idle` with no timestamps.
 *
 * @typedef {object} M3uAccount
 * @property {number} id
 * @property {string} name
 * @property {'standard'|'xtream_codes'} account_type
 * @property {string | null} server_url
 * @property {string | null} file_path
 * @property {string | null} username
 * @property {boolean} has_password
 * @property {number} max_streams
 * @property {boolean} is_active
 * @property {boolean} locked
 * @property {number} priority
 * @property {number} refresh_interval_hours
 * @property {number} stale_stream_days 0 disables automatic deletion.
 * @property {'idle'|'running'|'success'|'failed'|'cancelled'} status
 *   The last refresh's job state, in the same words `/api/core/jobs/` uses.
 * @property {number} progress 0 to 1.
 * @property {string | null} last_message What the last refresh did, or why it failed.
 * @property {string | null} updated_at
 *   When it last refreshed **successfully**. A run in flight or one that failed
 *   leaves it on the last good one; null means there has never been one.
 * @property {string | null} next_run_at
 */
/**
 * `POST /{resource}/refresh/` and `POST /{resource}/refresh/{id}/`.
 *
 * M3U and EPG spell these identically, so one helper serves both and a page
 * that triggers a refresh does not have to know which kind it is.
 *
 * @param {'m3u'|'epg'} resource
 */
function refreshers(resource) {
  return {
    /** @returns {Promise<{started: boolean, job: string}>} */
    refresh: (id) => api.post(`/${resource}/refresh/${id}/`),
    /** @returns {Promise<{started: string[]}>} */
    refreshAll: () => api.post(`/${resource}/refresh/`),
  };
}

export const m3uAccounts = {
  /** @returns {Promise<M3uAccount[]>} */
  list: async () => rows(await api.get('/m3u/accounts/'), '/api/m3u/accounts/'),
  create: (payload) => api.post('/m3u/accounts/', payload),
  update: (id, payload) => api.patch(`/m3u/accounts/${id}/`, payload),
  remove: (id) => api.delete(`/m3u/accounts/${id}/`),

  ...refreshers('m3u'),

  profiles: async (id) =>
    rows(
      await api.get(`/m3u/accounts/${id}/profiles/`),
      '/api/m3u/accounts/{id}/profiles/',
    ),
  createProfile: (id, payload) => api.post(`/m3u/accounts/${id}/profiles/`, payload),
  updateProfile: (accountId, id, payload) =>
    api.patch(`/m3u/accounts/${accountId}/profiles/${id}/`, payload),
  removeProfile: (accountId, id) =>
    api.delete(`/m3u/accounts/${accountId}/profiles/${id}/`),

  filters: async (id) =>
    rows(
      await api.get(`/m3u/accounts/${id}/filters/`),
      '/api/m3u/accounts/{id}/filters/',
    ),
  createFilter: (id, payload) => api.post(`/m3u/accounts/${id}/filters/`, payload),
  removeFilter: (accountId, id) =>
    api.delete(`/m3u/accounts/${accountId}/filters/${id}/`),

  /** Provider groups and whether this account imports them. */
  groups: async (id) =>
    rows(await api.get(`/m3u/accounts/${id}/groups/`), '/api/m3u/accounts/{id}/groups/'),
  setGroup: (id, payload) => api.post(`/m3u/accounts/${id}/groups/`, payload),
};

/**
 * @typedef {object} EpgSource
 * @property {number} id
 * @property {string} name
 * @property {'xmltv'|'dummy'} source_type
 * @property {string | null} url
 * @property {string | null} file_path
 * @property {string | null} username
 * @property {boolean} has_password
 * @property {boolean} is_active
 * @property {number} priority
 * @property {number} refresh_interval_hours
 * @property {'idle'|'running'|'success'|'failed'|'cancelled'} status
 *   The last refresh's job state, in the same words `/api/core/jobs/` uses.
 * @property {number} progress
 * @property {string | null} last_message
 * @property {string | null} updated_at
 *   When it last refreshed **successfully**, as on {@link M3uAccount}.
 * @property {string | null} next_run_at
 */
export const epgSources = {
  /** @returns {Promise<EpgSource[]>} */
  list: async () => rows(await api.get('/epg/sources/'), '/api/epg/sources/'),
  create: (payload) => api.post('/epg/sources/', payload),
  update: (id, payload) => api.patch(`/epg/sources/${id}/`, payload),
  remove: (id) => api.delete(`/epg/sources/${id}/`),

  ...refreshers('epg'),

  /**
   * The channels the matcher scored into the band it refuses to decide.
   *
   * This is the list behind "3 channels need a guide decision". A count alone
   * goes stale the moment somebody resolves one and cannot say *which*
   * channels — the exact failure the three-outcome matcher exists to avoid.
   *
   * Read-only. Accepting one is a PATCH of the channel's `epg_data_id`, which
   * the TV Guide already does per row.
   *
   * @typedef {object} AmbiguousMatch
   * @property {number} channel_id
   * @property {string} channel_name Provider-supplied. Render as text.
   * @property {number} epg_data_id
   * @property {string} candidate_name Provider-supplied. Render as text.
   * @property {string | null} candidate_tvg_id
   * @property {number} score Percent, 0 to 100, as the guide's badge shows it.
   * @returns {Promise<AmbiguousMatch[]>}
   */
  ambiguous: async () => rows(await api.get('/epg/ambiguous/'), '/api/epg/ambiguous/'),

  /**
   * Fills in the guide for channels that have none, on demand.
   *
   * Never an overwrite: a channel already mapped — by the matcher or by hand —
   * is left alone, and a candidate the scorer is unsure of becomes a row in
   * `ambiguous` rather than an assignment. Deliberately a button and not a step
   * of every refresh, because a wrong mapping is harder to notice than a
   * missing one.
   *
   * Whole-catalogue: the endpoint takes an `epg_source`, and nothing here sends
   * one, because "match against one source and not the others" is not a
   * question this screen asks.
   *
   * @returns {Promise<{matched: number, need_a_decision: number}>}
   */
  match: () => api.post('/epg/match/'),
};

/**
 * How many guide channels one search returns.
 *
 * The server clamps at 10,000 and a full guide runs to thousands of rows, so
 * the ceiling is the picker's own: fifty is more than anyone reads before
 * typing another letter, and the search is what narrows the list.
 */
const GUIDE_SEARCH_LIMIT = 50;

/**
 * The guide channels themselves, as distinct from the sources they came from.
 *
 * @typedef {object} EpgData
 * @property {number} id
 * @property {number | null} epg_source_id
 * @property {string} name Provider-supplied. Render as text.
 * @property {string | null} tvg_id The label the feed published it under —
 *   often how an operator recognises the row, and searched alongside the name.
 * @property {string | null} icon_url
 */
export const epgData = {
  /**
   * Guide channels matching `search` by name or by `tvg_id`, ordered by name.
   *
   * Server-side search rather than a whole-collection fetch: this is the one
   * table on the instance with thousands of rows and no page in the UI that
   * shows them all.
   *
   * @param {string} [search]
   * @returns {Promise<EpgData[]>}
   */
  list: async (search) =>
    rows(
      await api.get('/epg/epgdata/', { query: { search, limit: GUIDE_SEARCH_LIMIT } }),
      '/api/epg/epgdata/',
    ),
};

/**
 * Background work, one row per schedulable unit.
 *
 * `running` is the scheduler's own view rather than the row's: after a crash
 * the row still says `running` and this does not.
 *
 * @typedef {object} Job
 * @property {string} key Single-flight key, e.g. `m3u_refresh:2`.
 * @property {string} kind
 * @property {Record<string, unknown>} payload Carries `m3u_account_id` or `epg_source_id`.
 * @property {string} state
 * @property {number} progress
 * @property {boolean} running
 * @property {string | null} message
 * @property {string | null} last_error
 * @property {string | null} next_run_at
 */
export const jobs = {
  /** @returns {Promise<Job[]>} */
  list: async () => rows(await api.get('/core/jobs/'), '/api/core/jobs/'),

  /**
   * Cooperative: the handler notices at its next checkpoint, so a true here
   * means the job was there to ask, not that it has stopped.
   *
   * @returns {Promise<{cancelling: boolean}>}
   */
  cancel: (key) => api.post(`/core/jobs/${encodeURIComponent(key)}/cancel/`),
};

/**
 * The whole guide for a window, in one response.
 *
 * Shaped by channel rather than as a flat programme list: the flat shape makes
 * the browser join thousands of programmes onto dozens of channels by
 * `tvg_id`. Channels
 * come from `effective_channel`, so the guide is ordered exactly like the
 * lineup Plex receives.
 *
 * A channel with no guide still gets a row with an empty `programs` array —
 * a missing row would look like the channel itself was gone.
 *
 * @typedef {object} GuideProgram
 * @property {number | string} id A row id, or `dummy-<epoch>` for a listing a
 * dummy source generated, which has no row. Stable within the hour, so a
 * re-fetch does not shift every block on screen.
 * @property {string} start_time
 * @property {string} end_time
 * @property {string} title
 * @property {string | null} sub_title
 * @property {string | null} description
 * @property {number | null} season
 * @property {number | null} episode
 * @property {boolean} is_new
 * @property {boolean} is_live
 * @property {boolean} is_premiere
 *
 * @typedef {object} GuideChannel
 * @property {number} id
 * @property {string} uuid
 * @property {string} name
 * @property {number | null} channel_number
 * @property {string | null} logo_url
 * @property {string | null} group_name
 * @property {number | null} epg_data_id
 * @property {GuideProgram[]} programs
 * @property {{epg_data_id: number, name: string | null, score: number} | null}
 *   epg_suggestion The candidate the fuzzy matcher scored into the band it
 *   refuses to decide, carried so a human can decide it once.
 */
export const guide = {
  /**
   * @param {{from?: Date, to?: Date, channelProfile?: number}} [window]
   * @returns {Promise<{start: string, end: string, channels: GuideChannel[]}>}
   */
  grid: async ({ from, to, channelProfile } = {}) => {
    const body = await api.get('/epg/grid/', {
      query: {
        from: from?.toISOString(),
        to: to?.toISOString(),
        channel_profile: channelProfile,
      },
    });
    return {
      start: body?.start ?? null,
      end: body?.end ?? null,
      channels: bareRows(body?.channels, 'the `channels` in /api/epg/grid/'),
    };
  },

  /** Assigns the suggested guide id and clears the suggestion. */
  acceptSuggestion: (channelId) => api.post(`/epg/suggestions/${channelId}/`),

  /** Clears the suggestion without assigning anything. */
  dismissSuggestion: (channelId) => api.delete(`/epg/suggestions/${channelId}/`),
};
