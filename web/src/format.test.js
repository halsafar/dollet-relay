import { describe, expect, it } from 'vitest';

import { formatBytes } from './format.js';

describe('formatBytes', () => {
  it('reads nothing, and anything that is not a size, as zero', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(-1)).toBe('0 B');
    expect(formatBytes(Number.NaN)).toBe('0 B');
    expect(formatBytes(undefined)).toBe('0 B');
  });

  it('keeps a decimal only where it says something', () => {
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(1_572_864)).toBe('1.5 MB');
    expect(formatBytes(12 * 1024 * 1024)).toBe('12 MB');
  });
});
