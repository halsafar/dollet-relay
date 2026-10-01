import { describe, expect, it } from 'vitest';

import { compareChannelNumber, parseChannelNumber } from './channelNumber.js';

const row = (value) => ({ getValue: () => value });
const compare = (a, b) => compareChannelNumber(row(a), row(b), 'channel_number');

describe('parseChannelNumber', () => {
  it('keeps a float a float', () => {
    expect(parseChannelNumber(2.1)).toBe(2.1);
    expect(parseChannelNumber('11.1')).toBe(11.1);
  });

  it('parses the partial input a number field holds mid-typing', () => {
    // Mantine keeps the trailing separator; an f64 rejects the string "2.".
    expect(parseChannelNumber('2.')).toBe(2);
  });

  it('treats every kind of empty as unnumbered, never as zero', () => {
    expect(parseChannelNumber('')).toBeNull();
    expect(parseChannelNumber(null)).toBeNull();
    expect(parseChannelNumber(undefined)).toBeNull();
  });

  it('keeps a real zero', () => {
    expect(parseChannelNumber(0)).toBe(0);
    expect(parseChannelNumber('0')).toBe(0);
  });

  it('rejects anything that is not a finite number', () => {
    expect(parseChannelNumber('abc')).toBeNull();
    expect(parseChannelNumber(Infinity)).toBeNull();
    expect(parseChannelNumber(NaN)).toBeNull();
  });
});

describe('compareChannelNumber', () => {
  it('orders numerically, not lexicographically', () => {
    expect(compare(2.1, 11.1)).toBeLessThan(0);
    expect(compare(11.1, 2.1)).toBeGreaterThan(0);
    expect(compare(5, 5)).toBe(0);
  });

  it('puts an unnumbered channel after every numbered one', () => {
    // Ascending is the lineup order; an unnumbered channel leading it is the
    // first thing Plex shows.
    expect(compare(null, 2.1)).toBeGreaterThan(0);
    expect(compare(2.1, null)).toBeLessThan(0);
    expect(compare(undefined, 2.1)).toBeGreaterThan(0);
  });

  it('ties two unnumbered channels', () => {
    expect(compare(null, null)).toBe(0);
    expect(compare(null, undefined)).toBe(0);
  });

  it('distinguishes unnumbered from channel zero', () => {
    // Coercing null to 0 ties these, which makes the comparator inconsistent
    // and the sort dependent on input order.
    expect(compare(null, 0)).toBeGreaterThan(0);
    expect(compare(0, null)).toBeLessThan(0);
  });

  it('is a consistent total order', () => {
    const values = [null, 0, 2.1, 11.1, undefined, 5];
    const sortOnce = (input) =>
      [...input].sort((a, b) => compare(a, b)).map((v) => v ?? 'none');

    // Same answer regardless of the order it started from.
    expect(sortOnce(values)).toEqual(sortOnce([...values].reverse()));
    expect(sortOnce(values)).toEqual([0, 2.1, 5, 11.1, 'none', 'none']);
  });
});
