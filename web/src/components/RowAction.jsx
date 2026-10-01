import { ActionIcon, Tooltip } from '@mantine/core';

/**
 * One icon button in a table row.
 *
 * One place, because an icon-only button with no tooltip is its own whole
 * explanation on a page where every neighbour explains itself on hover.
 *
 * The tooltip is not optional here for that reason, and `label` feeds both it
 * and the accessible name — an `aria-label` that disagrees with the visible
 * tooltip is the version of this bug a screen reader gets.
 *
 * @param {object} props
 * @param {string} props.label What this does, e.g. `Edit ESPN`. Shown on hover
 *   and read out as the button's name.
 * @param {string} [props.tooltip] Overrides the hover text where it has to
 *   differ — a disabled button explaining *why* it is disabled is the case.
 * @param {'accent' | 'red'} [props.color]
 * @param {boolean} [props.disabled]
 * @param {() => void} props.onClick
 * @param {import('react').ReactNode} props.children The icon.
 */
export function RowAction({
  label,
  tooltip,
  color = 'accent',
  disabled = false,
  onClick,
  children,
}) {
  return (
    <Tooltip label={tooltip ?? label}>
      <ActionIcon
        variant="subtle"
        color={color}
        size="sm"
        aria-label={label}
        disabled={disabled}
        onClick={onClick}
      >
        {children}
      </ActionIcon>
    </Tooltip>
  );
}
