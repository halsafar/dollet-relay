/**
 * Channel-number semantics, shared by the lineup table and the editor.
 *
 * A channel number is a non-negative float or null. Both halves matter: `2.1`
 * is a real subchannel, and `null` means unnumbered — never 0, which would be
 * channel zero and would lead the lineup.
 */

/**
 * Parses whatever the number input currently holds.
 *
 * Mantine keeps a trailing separator while typing, so the field can hold `'2.'`
 * — a string, which the server's `f64` rejects with an opaque serde error.
 *
 * @param {unknown} value
 * @returns {number | null}
 */
export function parseChannelNumber(value) {
  if (value === '' || value === null || value === undefined) return null;
  const parsed = typeof value === 'number' ? value : Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

/**
 * Orders channel numbers the way `db::channels::ORDERING` does: numerically,
 * with unnumbered channels last on ascending.
 *
 * A total order, unlike coercing null to 0 — which ties an unnumbered channel
 * with a real channel 0 and makes the result depend on input order. TanStack
 * reverses this for descending, which also matches the server's
 * `channel_number IS NULL {dir}`. The admin must be looking at the same order
 * Plex receives, or the lineup they think they curated is not the one served.
 *
 * @type {import('@tanstack/react-table').SortingFn<any>}
 */
export function compareChannelNumber(rowA, rowB, columnId) {
  const left = rowA.getValue(columnId);
  const right = rowB.getValue(columnId);
  const leftMissing = left === null || left === undefined;
  const rightMissing = right === null || right === undefined;

  if (leftMissing && rightMissing) return 0;
  if (leftMissing) return 1;
  if (rightMissing) return -1;
  return left - right;
}
