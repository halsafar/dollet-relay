import { useMemo, useState } from 'react';
import { Group, Stack, Text, TextInput, UnstyledButton } from '@mantine/core';
import { Search } from 'lucide-react';

import { LogoImage } from './LogoImage.jsx';
import classes from '../pages/Channels.module.css';

/** How many logos to show before the search box is the only way to the rest. */
const VISIBLE = 60;

/**
 * Picks a logo by sight rather than by id.
 *
 * Logo names come from providers and are frequently just a filename, so the
 * image is the only useful label — but rendering a thousand of them at once is
 * a thousand network requests, hence the cap plus search.
 */
export function LogoPicker({ logos, value, onChange, label = 'Logo' }) {
  const [query, setQuery] = useState('');

  const matches = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const filtered = needle
      ? logos.filter((logo) => logo.name.toLowerCase().includes(needle))
      : logos;
    return filtered.slice(0, VISIBLE);
  }, [logos, query]);

  const selected = logos.find((logo) => logo.id === value) ?? null;

  return (
    <Stack gap={6}>
      <Group justify="space-between" align="flex-end">
        <Text size="sm" fw={500} component="div">
          {label}
        </Text>
        <Group gap={8}>
          {selected && (
            <UnstyledButton
              onClick={() => onChange(null)}
              aria-label="Clear logo"
              style={{ fontSize: 11, color: 'var(--mantine-color-dark-2)' }}
            >
              Clear
            </UnstyledButton>
          )}
          <TextInput
            size="xs"
            value={query}
            onChange={(event) => setQuery(event.currentTarget.value)}
            placeholder="Search logos"
            aria-label="Search logos"
            leftSection={<Search size={12} />}
            w={170}
          />
        </Group>
      </Group>

      {logos.length === 0 && (
        <Text size="xs" c="dimmed">
          No logos yet. They arrive with M3U ingest.
        </Text>
      )}

      <div className={classes.logoGrid}>
        {matches.map((logo) => (
          <UnstyledButton
            key={logo.id}
            className={classes.logoOption}
            data-selected={logo.id === value || undefined}
            aria-label={logo.name}
            aria-pressed={logo.id === value}
            onClick={() => onChange(logo.id)}
          >
            <LogoImage src={logo.url} alt={logo.name} size={40} />
          </UnstyledButton>
        ))}
      </div>

      {matches.length === 0 && logos.length > 0 && (
        <Text size="xs" c="dimmed">
          No logo matches that search.
        </Text>
      )}
    </Stack>
  );
}
