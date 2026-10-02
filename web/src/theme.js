import { createTheme, rem } from '@mantine/core';

/**
 * Dark-only, dense, green-accented.
 *
 * Surfaces, borders and secondary text all come from this one scale, so
 * readability is fixed here rather than page by page. Mantine's dark scheme
 * reads text from `dark-0`, dimmed text from `dark-2` and placeholders from
 * `dark-3`; this app lays the page on `dark-9`, panels on `dark-8`, table
 * headers and hover on `dark-7`, and borders and input fields on `dark-6`.
 *
 * The surfaces are darker than Mantine's stock ones: this UI is mostly large
 * tables, and stock `dark.7` surfaces put enough light on screen that row text
 * stops being the brightest thing in view. The text tones are held to measured
 * contrast floors in `theme.test.js`.
 */
const surface = [
  '#e4e8ee',
  '#b4bcc7',
  '#9aa3b0',
  '#7d8592',
  '#3a3f47',
  '#2a2e35',
  '#1e2228',
  '#13161b',
  '#0e1115',
  '#07090c',
];

const accent = [
  '#e6f9ee',
  '#c6f0d8',
  '#9ae5bb',
  '#6ed99e',
  '#4bcf86',
  '#37d67a',
  '#26b866',
  '#1b9352',
  '#136f3e',
  '#0a4a29',
];

/**
 * High contrast lifts the text tones only: main text to near white, dimmed text
 * up a step to where `dark-1` sits by default, and `dark-1` between the two so
 * the scale still runs light to dark. The surfaces stay, so the layout reads
 * the same and only the words get brighter.
 */
const highContrastText = ['#f4f6f9', '#ccd2da', '#b4bcc7'];

/**
 * Mantine's `scale` multiplies every size it emits through `rem()`, and the CSS
 * modules multiply their font sizes by the same `--mantine-scale`, so one number
 * resizes the text. The steps are arbitrary: they set the tables' 12.5 px text
 * at 11.25 px and 14 px.
 */
const textScale = { small: 0.9, default: 1, large: 1.12 };

const base = {
  primaryColor: 'accent',
  primaryShade: 6,
  fontFamily:
    'Inter, ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
  fontFamilyMonospace: 'ui-monospace, SFMono-Regular, Menlo, monospace',
  defaultRadius: 'sm',
  headings: {
    fontWeight: '600',
    sizes: {
      h1: { fontSize: rem(22), lineHeight: '1.3' },
      h2: { fontSize: rem(18), lineHeight: '1.3' },
      h3: { fontSize: rem(15), lineHeight: '1.35' },
    },
  },
  components: {
    Button: { defaultProps: { size: 'xs' } },
    TextInput: { defaultProps: { size: 'xs' } },
    PasswordInput: { defaultProps: { size: 'xs' } },
    NumberInput: { defaultProps: { size: 'xs' } },
    Select: { defaultProps: { size: 'xs' } },
    MultiSelect: { defaultProps: { size: 'xs' } },
    Switch: { defaultProps: { size: 'sm' } },
    Checkbox: { defaultProps: { size: 'xs' } },
    Table: { defaultProps: { verticalSpacing: 4, horizontalSpacing: 'sm', fz: 'xs' } },
    Modal: { defaultProps: { centered: true, overlayProps: { backgroundOpacity: 0.7 } } },
    Tooltip: { defaultProps: { fz: 'xs', withArrow: true, openDelay: 400 } },
  },
};

/** The theme for one browser's appearance choices, from `appearance.js`. */
export function buildTheme({ textSize, contrast }) {
  const dark = contrast === 'high' ? [...highContrastText, ...surface.slice(3)] : surface;

  return createTheme({ ...base, scale: textScale[textSize], colors: { dark, accent } });
}
