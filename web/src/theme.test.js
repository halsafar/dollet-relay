import { describe, expect, it } from 'vitest';

import { CONTRASTS, TEXT_SIZES } from './appearance.js';
import { buildTheme } from './theme.js';

/** WCAG 2.x relative luminance of a `#rrggbb` colour. */
function luminance(hex) {
  const channels = [1, 3, 5].map((at) => parseInt(hex.slice(at, at + 2), 16) / 255);
  const [r, g, b] = channels.map((c) =>
    c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4,
  );
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a, b) {
  const [light, dark] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (light + 0.05) / (dark + 0.05);
}

const palette = (choice) =>
  buildTheme({ textSize: 'default', contrast: choice }).colors.dark;

/**
 * Mantine's dark scheme draws text in `dark-0` and dimmed text in `dark-2`, and
 * this UI puts both on panels (`dark-8`) and on the page (`dark-9`). 4.5:1 is
 * WCAG AA for body text, and dimmed text here is body-sized. 10:1 for the main
 * text is an arbitrary floor of ours, well past AAA's 7:1, so that it stays
 * visibly brighter than dimmed.
 */
const FLOORS = [
  { text: 0, name: 'text', floor: 10 },
  { text: 2, name: 'dimmed', floor: 4.5 },
];

const BACKGROUNDS = [
  { shade: 8, name: 'a panel' },
  { shade: 9, name: 'the page' },
];

describe.each(CONTRASTS)('the dark palette at %s contrast', (choice) => {
  const dark = palette(choice);

  for (const { text, name, floor } of FLOORS) {
    for (const { shade, name: where } of BACKGROUNDS) {
      it(`keeps ${name} on ${where} at ${floor}:1 or better`, () => {
        expect(contrast(dark[text], dark[shade])).toBeGreaterThanOrEqual(floor);
      });
    }
  }

  it('runs from lightest to darkest', () => {
    // Every Mantine default maps a role to a shade by its position; a scale
    // out of order makes, say, the hover tone darker than the surface under it.
    const levels = dark.map(luminance);
    for (let shade = 1; shade < levels.length; shade += 1) {
      expect(levels[shade]).toBeLessThan(levels[shade - 1]);
    }
  });
});

describe('buildTheme', () => {
  it.each([
    ['small', 0.9],
    ['default', 1],
    ['large', 1.12],
  ])('sets the %s text size as a scale of %s', (textSize, scale) => {
    expect(buildTheme({ textSize, contrast: 'default' }).scale).toBe(scale);
  });

  it('has a scale for every text size the store offers', () => {
    for (const textSize of TEXT_SIZES) {
      expect(buildTheme({ textSize, contrast: 'default' }).scale).toBeGreaterThan(0);
    }
  });

  it('brightens text and dimmed text at high contrast', () => {
    const normal = palette('default');
    const high = palette('high');

    for (const text of [0, 2]) {
      expect(contrast(high[text], high[8])).toBeGreaterThan(
        contrast(normal[text], normal[8]),
      );
    }
  });

  it('changes only the text tones at high contrast', () => {
    // A contrast choice that moved the panels would move every border and
    // hover tone drawn against them, and the layout would read differently.
    expect(palette('high').slice(3)).toEqual(palette('default').slice(3));
  });
});
