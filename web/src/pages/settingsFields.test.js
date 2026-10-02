import { describe, expect, it } from 'vitest';

import { groupLabel, sectionSlug, splitHelp } from './settingsFields.js';

describe('splitHelp', () => {
  it('puts the first sentence under the field and the rest behind it', () => {
    expect(splitHelp('What it is. When to change it. What changing it does.')).toEqual({
      summary: 'What it is.',
      detail: 'When to change it. What changing it does.',
    });
  });

  it('does not break a sentence at a decimal point', () => {
    expect(
      splitHelp(
        'Below which it counts as buffering: 1.0 is real time. Read from ffmpeg.',
      ),
    ).toEqual({
      summary: 'Below which it counts as buffering: 1.0 is real time.',
      detail: 'Read from ffmpeg.',
    });
  });

  it('does not break a sentence at a dot inside a name', () => {
    expect(
      splitHelp(
        'us favours vrix.us over vrix.uk, and counts against others. Leave unset.',
      ),
    ).toEqual({
      summary: 'us favours vrix.us over vrix.uk, and counts against others.',
      detail: 'Leave unset.',
    });
  });

  it('keeps a closing quote with the sentence it ends', () => {
    expect(splitHelp('Compared as "VRIX". HD is ignored already.')).toEqual({
      summary: 'Compared as "VRIX".',
      detail: 'HD is ignored already.',
    });
  });

  it('shows one sentence whole, with nothing behind it', () => {
    expect(splitHelp('Only this.')).toEqual({ summary: 'Only this.' });
  });

  it('has nothing to say for a field with no help', () => {
    expect(splitHelp(undefined)).toEqual({});
  });
});

describe('sectionSlug', () => {
  it('drops the settings suffix and hyphenates the rest', () => {
    expect(sectionSlug('stream_settings')).toBe('stream');
    expect(sectionSlug('network_access')).toBe('network-access');
  });
});

describe('groupLabel', () => {
  it('uses the listed heading for a known group', () => {
    expect(groupLabel({ key: 'epg_settings', name: 'EPG Settings' })).toBe(
      'Guide matching',
    );
  });

  it('falls back to the server name without a trailing Settings', () => {
    expect(groupLabel({ key: 'brand_new_settings', name: 'Brand New Settings' })).toBe(
      'Brand New',
    );
  });
});
