import { useId } from 'react';
import { SegmentedControl, Stack, Text } from '@mantine/core';

import { useAppearance } from '../appearance.js';

const TEXT_SIZE_OPTIONS = [
  { value: 'small', label: 'Small' },
  { value: 'default', label: 'Default' },
  { value: 'large', label: 'Large' },
];

const CONTRAST_OPTIONS = [
  { value: 'default', label: 'Default' },
  { value: 'high', label: 'High' },
];

/** The one Settings section the server never sees. */
export function AppearanceSettings() {
  const { textSize, contrast, setTextSize, setContrast } = useAppearance();

  return (
    <Stack gap="sm">
      <Text size="xs" c="dimmed">
        Stored in this browser only, and applied as soon as it is chosen.
      </Text>
      <Choice
        label="Text size"
        data={TEXT_SIZE_OPTIONS}
        value={textSize}
        onChange={setTextSize}
      />
      <Choice
        label="Contrast"
        data={CONTRAST_OPTIONS}
        value={contrast}
        onChange={setContrast}
      />
    </Stack>
  );
}

/** A SegmentedControl is a bare radiogroup; the label is what names it. */
function Choice({ label, data, value, onChange }) {
  const id = useId();

  return (
    <Stack gap={4} align="flex-start">
      <Text id={id} size="xs" fw={500}>
        {label}
      </Text>
      <SegmentedControl
        aria-labelledby={id}
        size="xs"
        data={data}
        value={value}
        onChange={onChange}
      />
    </Stack>
  );
}
