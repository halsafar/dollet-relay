/**
 * Rendering values that several pages show the same way.
 *
 * One copy, so every page guards an unparseable timestamp the same way rather
 * than one of them rendering `Invalid Date`.
 */

/**
 * An absolute instant in the viewer's locale.
 *
 * Absolute rather than relative: these appear on pages nobody reloads, and
 * "3 minutes ago" on a screen left open since lunch is a lie. Where a relative
 * form is wanted alongside, the Notifications page pairs the two.
 *
 * An unparseable value comes back as itself. It is a server timestamp, so a
 * value this cannot read is a bug worth seeing rather than hiding behind
 * `Invalid Date`.
 *
 * @param {string | null | undefined} value
 * @param {string} [empty] What to show when there is no value at all.
 */
export function absoluteTime(value, empty = '') {
  if (!value) return empty;
  const at = new Date(value);
  return Number.isNaN(at.getTime()) ? String(value) : at.toLocaleString();
}

/**
 * A byte count in binary units: `1.5 MB` below ten of a unit, `12 MB` above,
 * and `0 B` for nothing or for a value that is not a number.
 *
 * @param {number} bytes
 */
export function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const power = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** power;
  return `${value >= 10 || power === 0 ? Math.round(value) : value.toFixed(1)} ${units[power]}`;
}
