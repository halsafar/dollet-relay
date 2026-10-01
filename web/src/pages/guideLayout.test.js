import { describe, expect, it } from 'vitest';

import {
  PX_PER_HOUR,
  ROW_HEIGHT,
  hourMarks,
  isOnAir,
  nowOffset,
  placeProgram,
  toPx,
  visibleRows,
} from './guideLayout.js';

const HOUR = 3_600_000;
const WINDOW_START = Date.parse('2026-02-10T18:00:00Z');
const WINDOW_END = WINDOW_START + 24 * HOUR;

const program = (startHours, endHours) => ({
  start_time: new Date(WINDOW_START + startHours * HOUR).toISOString(),
  end_time: new Date(WINDOW_START + endHours * HOUR).toISOString(),
});

const place = (p) => placeProgram(p, WINDOW_START, WINDOW_END);

describe('placing a programme', () => {
  it('positions one that fits entirely inside the window', () => {
    const box = place(program(2, 3));

    expect(box.left).toBe(2 * PX_PER_HOUR);
    expect(box.width).toBe(PX_PER_HOUR);
    expect(box.clippedStart).toBe(false);
    expect(box.clippedEnd).toBe(false);
  });

  it('clips a film that started before the window rather than dropping it', () => {
    // A three-hour film that began an hour before the window opened. This is
    // the most visible way a guide looks broken: the current programme just
    // missing from the row.
    const box = place(program(-1, 2));

    expect(box).not.toBeNull();
    expect(box.left).toBe(0);
    expect(box.width).toBe(2 * PX_PER_HOUR);
    expect(box.clippedStart).toBe(true);
    expect(box.clippedEnd).toBe(false);
  });

  it('clips one running past the end of the window', () => {
    const box = place(program(23, 26));

    expect(box.left).toBe(23 * PX_PER_HOUR);
    expect(box.width).toBe(PX_PER_HOUR);
    expect(box.clippedStart).toBe(false);
    expect(box.clippedEnd).toBe(true);
  });

  it('clips one that spans the whole window at both ends', () => {
    const box = place(program(-5, 30));

    expect(box.left).toBe(0);
    expect(box.width).toBe(24 * PX_PER_HOUR);
    expect(box.clippedStart).toBe(true);
    expect(box.clippedEnd).toBe(true);
  });

  it('drops one that ends exactly as the window opens', () => {
    // Touching is not overlapping; it belongs to the previous window.
    expect(place(program(-2, 0))).toBeNull();
  });

  it('drops one that starts exactly as the window closes', () => {
    expect(place(program(24, 26))).toBeNull();
  });

  it('drops one entirely outside the window', () => {
    expect(place(program(-5, -3))).toBeNull();
    expect(place(program(30, 32))).toBeNull();
  });

  it('keeps a zero-length programme clickable rather than invisible', () => {
    const box = place(program(4, 4));
    expect(box).toBeNull();

    // One that is merely very short still gets a usable width.
    const tiny = place({
      start_time: new Date(WINDOW_START + HOUR).toISOString(),
      end_time: new Date(WINDOW_START + HOUR + 1000).toISOString(),
    });
    expect(tiny.width).toBeGreaterThanOrEqual(2);
  });

  it('ignores a programme with unparseable times', () => {
    expect(place({ start_time: 'not a date', end_time: 'also not' })).toBeNull();
  });
});

describe('virtualizing rows', () => {
  it('renders nothing for an empty guide', () => {
    expect(visibleRows(0, 600, 0)).toEqual({
      start: 0,
      end: 0,
      offsetY: 0,
      totalHeight: 0,
    });
  });

  it('renders only what fits, plus overscan', () => {
    const { start, end, totalHeight } = visibleRows(0, 600, 49, 3);

    expect(start).toBe(0);
    // 600px of viewport at 74px a row is 9 rows, plus overscan both sides.
    expect(end).toBeLessThan(49);
    expect(end).toBeGreaterThanOrEqual(Math.ceil(600 / ROW_HEIGHT));
    expect(totalHeight).toBe(49 * ROW_HEIGHT);
  });

  it('moves the window as the user scrolls', () => {
    const { start, offsetY } = visibleRows(ROW_HEIGHT * 20, 600, 49, 3);

    expect(start).toBe(17);
    // The offset must match the first rendered row exactly, or every row is
    // drawn at the wrong height.
    expect(offsetY).toBe(17 * ROW_HEIGHT);
  });

  it('never scrolls past the last row', () => {
    const { end } = visibleRows(ROW_HEIGHT * 100, 600, 49, 3);
    expect(end).toBe(49);
  });

  it('keeps the full height regardless of what is rendered', () => {
    // The scrollbar must reflect the whole guide, not the visible slice.
    expect(visibleRows(0, 600, 49).totalHeight).toBe(49 * ROW_HEIGHT);
    expect(visibleRows(5000, 600, 49).totalHeight).toBe(49 * ROW_HEIGHT);
  });
});

describe('the hour axis', () => {
  it('marks every hour of the window', () => {
    const marks = hourMarks(WINDOW_START, WINDOW_END);
    expect(marks).toHaveLength(24);
    expect(marks[0].left).toBe(0);
    expect(marks[1].left).toBe(PX_PER_HOUR);
  });

  it('starts at the next whole hour when the window does not', () => {
    const ragged = WINDOW_START + 47 * 60_000;
    const marks = hourMarks(ragged, ragged + 3 * HOUR);

    // Labels read 7:00, not 6:47.
    expect(new Date(marks[0].time).getUTCMinutes()).toBe(0);
    expect(marks[0].left).toBeGreaterThan(0);
  });
});

describe('the now-line', () => {
  it('sits proportionally inside the window', () => {
    expect(nowOffset(WINDOW_START + 6 * HOUR, WINDOW_START, WINDOW_END)).toBe(
      6 * PX_PER_HOUR,
    );
  });

  it('is absent when now is outside the window', () => {
    expect(nowOffset(WINDOW_START - HOUR, WINDOW_START, WINDOW_END)).toBeNull();
    expect(nowOffset(WINDOW_END + HOUR, WINDOW_START, WINDOW_END)).toBeNull();
  });
});

describe('on air', () => {
  it('is true only while a programme is running', () => {
    const p = program(2, 3);

    expect(isOnAir(p, WINDOW_START + 2.5 * HOUR)).toBe(true);
    expect(isOnAir(p, WINDOW_START + HOUR)).toBe(false);
    // The end is exclusive, so two adjacent programmes are never both on air.
    expect(isOnAir(p, WINDOW_START + 3 * HOUR)).toBe(false);
    expect(isOnAir(p, WINDOW_START + 2 * HOUR)).toBe(true);
  });
});

describe('scale', () => {
  it('converts milliseconds to pixels at the declared rate', () => {
    expect(toPx(HOUR)).toBe(PX_PER_HOUR);
    expect(toPx(HOUR / 2)).toBe(PX_PER_HOUR / 2);
    expect(toPx(0)).toBe(0);
  });
});
