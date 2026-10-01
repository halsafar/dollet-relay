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
