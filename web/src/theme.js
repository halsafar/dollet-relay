import { createTheme, rem } from '@mantine/core';

/**
 * Dark-only, dense, green-accented.
 *
 * The palette is deliberately darker and lower-contrast than Mantine's stock
 * dark scheme: this UI is mostly large tables, and stock `dark.7` surfaces put
 * enough light on screen that row text stops being the brightest thing in view.
 */
const surface = [
  '#c9ced6',
  '#9aa2ae',
  '#6f7683',
  '#4e545e',
  '#3a3f47',
  '#2a2e35',
  '#1e2228',
  '#171a1f',
  '#12151a',
  '#0a0c10',
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

export const theme = createTheme({
  primaryColor: 'accent',
  primaryShade: 6,
  colors: { dark: surface, accent },
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
});
