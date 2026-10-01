import { SegmentedControl, Stack, Text, TextInput } from '@mantine/core';

import {
  BASE_URL_SOURCES,
  browserOrigin,
  normalizeBaseUrl,
  useBaseUrlStore,
} from './baseUrl.js';

/** The environment variable an operator has to set to get the third option. */
export const ADVERTISED_ENV = 'DOLLET_ADVERTISED_BASE_URL';

/**
 * Which address the URLs on this screen are built from.
 *
 * Three sources, because there are three honest answers: the address this
 * browser used, the one the server has been told to advertise to everyone, and
 * whatever the operator knows their clients use — a container name, a LAN
 * address, a hostname that only resolves on the far side of a VPN.
 */
export function BaseUrlPicker() {
  const source = useBaseUrlStore((state) => state.source);
  const custom = useBaseUrlStore((state) => state.custom);
  const advertised = useBaseUrlStore((state) => state.advertised);
  const setSource = useBaseUrlStore((state) => state.setSource);
  const setCustom = useBaseUrlStore((state) => state.setCustom);

  const options = [
    { value: BASE_URL_SOURCES.browser, label: 'This browser' },
    // Offered only when it is real: an option that resolves to nothing is a
    // choice that silently does not change the URLs beneath it.
    ...(advertised ? [{ value: BASE_URL_SOURCES.advertised, label: 'Advertised' }] : []),
    { value: BASE_URL_SOURCES.custom, label: 'Custom' },
  ];

  const invalid = custom.trim() !== '' && normalizeBaseUrl(custom) === null;

  return (
    <Stack gap={6}>
      <SegmentedControl
        size="xs"
        data={options}
        value={source}
        onChange={setSource}
        aria-label="Base address"
      />

      {source === BASE_URL_SOURCES.browser && (
        <Text size="xs" c="dimmed">
          {browserOrigin()} — the address you are reading this page on.
        </Text>
      )}

      {source === BASE_URL_SOURCES.advertised && (
        <Text size="xs" c="dimmed">
          {advertised} — from {ADVERTISED_ENV}, which every client is given regardless of
          how it reached this server.
        </Text>
      )}

      {source === BASE_URL_SOURCES.custom && (
        <TextInput
          size="xs"
          label="Custom base URL"
          placeholder="http://dollet-relay:9191"
          description="Scheme, host and port. Whatever your clients can reach."
          value={custom}
          error={invalid ? 'Enter an absolute http:// or https:// URL.' : null}
          onChange={(event) => setCustom(event.currentTarget.value)}
        />
      )}
    </Stack>
  );
}
