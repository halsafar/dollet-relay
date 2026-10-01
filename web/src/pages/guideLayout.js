/**
 * Geometry for the guide grid.
 *
 * Pure functions, because every visible way a guide looks broken is an
 * arithmetic mistake: a film that started before the window vanishing instead
 * of being clipped, a now-line one hour off, a row height that drifts from the
 * scroll offset used to place it.
 */

/** Horizontal scale. One hour of programming is this many pixels wide. */
export const PX_PER_HOUR = 320;

/** Fixed, because virtualization needs to place a row without measuring it. */
export const ROW_HEIGHT = 74;

const MS_PER_HOUR = 3_600_000;

export const toPx = (ms) => (ms / MS_PER_HOUR) * PX_PER_HOUR;

/**
 * Where a programme sits in the lane, clipped to the window.
 *
 * A programme overlapping either edge is clipped rather than dropped: a
 * three-hour film that started before the window must still show, anchored at
 * the left edge, or the guide reads as a hole where the current programme is.
 *
 * @returns {{left: number, width: number, clippedStart: boolean,
 *            clippedEnd: boolean} | null} Null when it does not overlap at all.
 */
export function placeProgram(program, windowStart, windowEnd) {
  const start = new Date(program.start_time).getTime();
  const end = new Date(program.end_time).getTime();
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;

  // A programme that does not end after it starts is malformed, not short.
  // Drawing it puts an unreadable sliver in the middle of the lane.
  if (end <= start) return null;

  // Touching at a boundary is not overlapping: a programme that ends exactly
  // when the window opens belongs to the previous window.
  if (end <= windowStart || start >= windowEnd) return null;

  const visibleStart = Math.max(start, windowStart);
  const visibleEnd = Math.min(end, windowEnd);

  return {
    left: toPx(visibleStart - windowStart),
    // A zero-length programme would otherwise be invisible and unclickable.
    width: Math.max(toPx(visibleEnd - visibleStart), 2),
    clippedStart: start < windowStart,
    clippedEnd: end > windowEnd,
  };
}

/**
 * Which rows to actually render.
 *
 * A lineup times a 24-hour window is a lot of DOM, and most of it is off
 * screen. Rows are a fixed height, so the visible range is arithmetic rather
 * than measurement.
 *
 * @returns {{start: number, end: number, offsetY: number, totalHeight: number}}
 */
export function visibleRows(scrollTop, viewportHeight, rowCount, overscan = 3) {
  const totalHeight = rowCount * ROW_HEIGHT;
  if (rowCount === 0) return { start: 0, end: 0, offsetY: 0, totalHeight: 0 };

  const first = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - overscan);
  const visible = Math.ceil(viewportHeight / ROW_HEIGHT) + overscan * 2;
  const last = Math.min(rowCount, first + visible);

  return { start: first, end: last, offsetY: first * ROW_HEIGHT, totalHeight };
}

/**
 * Hour marks across the top.
 *
 * Starts at the first whole hour at or after the window start, so the labels
 * read `7:00pm` rather than `6:47pm`.
 */
export function hourMarks(windowStart, windowEnd) {
  const marks = [];
  const first = new Date(windowStart);
  first.setMinutes(0, 0, 0);
  if (first.getTime() < windowStart) first.setHours(first.getHours() + 1);

  for (let t = first.getTime(); t < windowEnd; t += MS_PER_HOUR) {
    marks.push({ time: t, left: toPx(t - windowStart) });
  }
  return marks;
}

/** Where the now-line goes, or null when now is outside the window. */
export function nowOffset(now, windowStart, windowEnd) {
  if (now < windowStart || now > windowEnd) return null;
  return toPx(now - windowStart);
}

/** True while a programme is on air. */
export function isOnAir(program, now) {
  const start = new Date(program.start_time).getTime();
  const end = new Date(program.end_time).getTime();
  return start <= now && now < end;
}
