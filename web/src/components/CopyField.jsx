import { useState } from 'react';
import { ActionIcon, Group, TextInput, Tooltip } from '@mantine/core';
import { useClipboard } from '@mantine/hooks';
import { Check, Copy, Eye, EyeOff } from 'lucide-react';

/**
 * One URL or credential, read-only, with a copy button.
 *
 * A text input rather than a `<code>` block: the whole point of these is to
 * end up in another application's configuration field, and an input can be
 * selected, tabbed to, and read back by a test with `getByLabel` — which is
 * also how the browser journey asserts the URLs are right.
 */
export function CopyField({ label, description, value, secret = false }) {
  const clipboard = useClipboard({ timeout: 1200 });
  const [revealed, setRevealed] = useState(false);
  const hidden = secret && !revealed;

  return (
    <TextInput
      label={label}
      description={description}
      value={value}
      readOnly
      // Secrets are masked by the input's own type rather than by substituting
      // dots into the value, so "copy" still copies the password.
      type={hidden ? 'password' : 'text'}
      styles={{ input: { fontFamily: 'var(--mantine-font-family-monospace)' } }}
      // Mantine's right section ignores pointer events by default, which makes
      // a button in it look clickable and do nothing.
      rightSectionPointerEvents="all"
      rightSectionWidth={secret ? 64 : 36}
      rightSection={
        <Group gap={2} wrap="nowrap">
          {secret && (
            <Tooltip label={revealed ? 'Hide' : 'Reveal'}>
              <ActionIcon
                variant="subtle"
                color="gray"
                size="sm"
                aria-label={revealed ? `Hide ${label}` : `Reveal ${label}`}
                onClick={() => setRevealed((current) => !current)}
              >
                {revealed ? <EyeOff size={14} /> : <Eye size={14} />}
              </ActionIcon>
            </Tooltip>
          )}
          <Tooltip label={clipboard.copied ? 'Copied' : 'Copy'}>
            <ActionIcon
              variant="subtle"
              color={clipboard.copied ? 'accent' : 'gray'}
              size="sm"
              aria-label={`Copy ${label}`}
              onClick={() => clipboard.copy(value)}
            >
              {clipboard.copied ? <Check size={14} /> : <Copy size={14} />}
            </ActionIcon>
          </Tooltip>
        </Group>
      }
    />
  );
}
