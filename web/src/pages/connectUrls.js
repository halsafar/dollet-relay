/**
 * Every URL the Connect page hands an operator, built from one base.
 *
 * Pure string work, deliberately: which base to use is the only hard question
 * on that screen, and it is answered once in `BaseUrlPicker`. Everything here
 * takes the answer as an argument, so each combination of profile, output
 * profile and toggle is a table row in the tests rather than a click.
 */

/**
 * Percent-encode one path segment exactly as the server does.
 *
 * Mirrors `urlencode` in `crates/dollet-server/src/api/mod.rs`: RFC 3986's
 * unreserved set (`A-Za-z0-9-._~`) survives and every other byte goes out as
 * uppercase `%XX`. `encodeURIComponent` alone leaves `!'()*` untouched, so a
 * channel profile called `Kids (2)` would be offered here under a path the
 * server's own lineup never advertises — and the two would disagree about a
 * name the user can type. Both sides assert the same bytes for `Living Room`
 * and `Kids (2)`.
 */
export function encodeSegment(value) {
  return encodeURIComponent(String(value)).replace(
    /[!'()*]/g,
    (character) => `%${character.charCodeAt(0).toString(16).toUpperCase()}`,
  );
}

/**
 * The accepted `?tvg_id_source=` values, from `TvgIdSource::from_query` in
 * `crates/dollet-core/src/output/mod.rs`. Anything else falls back to the
 * channel number there, so offering a fourth option would be offering a typo.
 */
export const DEFAULT_TVG_ID_SOURCE = 'channel_number';

export const TVG_ID_SOURCES = [
  { value: DEFAULT_TVG_ID_SOURCE, label: 'Channel number' },
  { value: 'tvg_id', label: 'Guide id (tvg-id)' },
  { value: 'gracenote', label: 'Gracenote station id' },
];

/** Omits the default, because a URL carrying it says nothing the bare one does not. */
function guideSource(value) {
  return value && value !== DEFAULT_TVG_ID_SOURCE ? value : null;
}

/** `?a=1&b=2`, skipping every parameter that is at its default. */
function query(pairs) {
  const set = pairs.filter(([, value]) => value !== null && value !== undefined);
  if (set.length === 0) return '';
  return `?${set.map(([key, value]) => `${key}=${encodeSegment(value)}`).join('&')}`;
}

/** A named channel profile becomes a path segment; `All` is the bare path. */
function scoped(path, channelProfile) {
  return channelProfile ? `${path}/${encodeSegment(channelProfile)}` : path;
}

/**
 * The tuner root Plex is given. Trailing slash: Plex appends `discover.json`
 * to whatever it was handed, and `…/hdhrdiscover.json` is the failure that
 * looks like the server is down.
 */
export function hdhrUrl(base, { channelProfile, outputProfile } = {}) {
  let url = scoped(`${base}/hdhr`, channelProfile);
  if (outputProfile) url += `/output_profile/${encodeSegment(outputProfile)}`;
  return `${url}/`;
}

/** Where the tuner's own identity comes from — same scope, same prefix. */
export function discoverPath({ channelProfile, outputProfile } = {}) {
  return `${hdhrUrl('', { channelProfile, outputProfile })}discover.json`;
}

export function guideUrl(base, { channelProfile, tvgIdSource } = {}) {
  return `${scoped(`${base}/output/epg`, channelProfile)}${query([
    ['tvg_id_source', guideSource(tvgIdSource)],
  ])}`;
}

export function playlistUrl(
  base,
  { channelProfile, outputProfile, direct, cachedLogos, tvgIdSource } = {},
) {
  return `${scoped(`${base}/output/m3u`, channelProfile)}${query([
    ['output_profile', outputProfile ?? null],
    // Both of these are named only when they differ from the server's default:
    // proxied streams, cached logos.
    ['direct', direct ? 'true' : null],
    ['cachedlogos', cachedLogos === false ? 'false' : null],
    ['tvg_id_source', guideSource(tvgIdSource)],
  ])}`;
}

/** The single channel, for pasting into VLC when one looks wrong. */
export function streamUrl(base, uuid, outputProfile) {
  return `${base}/proxy/ts/stream/${encodeSegment(uuid)}${query([
    ['output_profile', outputProfile ?? null],
  ])}`;
}

/**
 * A base URL split the way an Xtream player's form asks for it.
 *
 * The port is explicit even when the URL omits it: the field is mandatory in
 * every player, and leaving the operator to know that https means 443 is how
 * that form gets filled in wrong.
 */
export function xtreamServer(base) {
  let url;
  try {
    url = new URL(base);
  } catch {
    return null;
  }
  return {
    server: `${url.protocol}//${url.hostname}`,
    port: url.port || (url.protocol === 'https:' ? '443' : '80'),
  };
}
