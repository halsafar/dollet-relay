import { useCallback, useMemo, useState } from 'react';
import {
  Accordion,
  Alert,
  Button,
  Center,
  Code,
  Group,
  Loader,
  MultiSelect,
  NumberInput,
  Select,
  Stack,
  Switch,
  TagsInput,
  Text,
  TextInput,
} from '@mantine/core';
import { notifyDone, notifyError } from '../notify.js';
import { TriangleAlert } from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { AppearanceSettings } from './AppearanceSettings.jsx';
import {
  outputProfiles,
  settings as settingsApi,
  streamProfiles,
  userAgents,
} from '../api/resources.js';
import { useResource } from '../api/useResource.js';
import { BackupList } from './BackupList.jsx';
import {
  FIELD_META,
  GROUP_HELP,
  GROUP_ORDER,
  HASH_KEY_TOKENS,
  NETWORK_ENDPOINTS,
  fieldErrors,
  humanize,
  joinHashKeys,
  parseHashKeys,
} from './settingsFields.js';

/** The one group with its own renderer and its own save rule. */
const NETWORK_ACCESS = 'network_access';

/** Ordinary fields, with the backups their schedule writes listed under them. */
const BACKUPS = 'backup_settings';

/** Not a server group: this browser's own, applied as it is chosen rather than saved. */
const APPEARANCE = 'appearance';

/** The lists a `reference` field draws its options from. */
const REFERENCE_LISTS = {
  userAgents: { label: 'User agents', api: userAgents },
  streamProfiles: { label: 'Stream profiles', api: streamProfiles },
  outputProfiles: { label: 'Output profiles', api: outputProfiles },
};

/**
 * Loaded once for the page and handed down, rather than fetched by each field:
 * the fields that need these lists sit inside a form that re-renders on every
 * keystroke, and their options do not change while it does.
 *
 * A list that will not load resolves to its failure rather than rejecting, so
 * one dead endpoint leaves the other selects working and its own field falls
 * back to the raw row id — still editable, which is the point.
 */
async function loadReferences() {
  const wanted = new Set(
    Object.values(FIELD_META)
      .map((meta) => meta.resource)
      .filter(Boolean),
  );

  const entries = await Promise.all(
    [...wanted].map(async (name) => {
      try {
        return [name, { options: toOptions(await REFERENCE_LISTS[name].api.list()) }];
      } catch (failure) {
        return [name, { error: failure }];
      }
    }),
  );
  return Object.fromEntries(entries);
}

/**
 * Rows to `Select` options. The schema makes these names unique NOCASE, so the
 * id is noise and stays out of the label; it is appended only if a list comes
 * back with two rows sharing a name, because two identical options are not a
 * choice anyone can make.
 */
function toOptions(rows) {
  const seen = new Map();
  for (const row of rows) seen.set(row.name, (seen.get(row.name) ?? 0) + 1);

  return rows.map((row) => ({
    value: String(row.id),
    label: seen.get(row.name) > 1 ? `${row.name} (#${row.id})` : row.name,
  }));
}

export function Settings() {
  const load = useCallback(() => settingsApi.list(), []);
  const { data: groups, loading, error, setData: setGroups } = useResource(load, []);
  const { data: lists, loading: listsLoading } = useResource(loadReferences, {});

  const references = useMemo(
    () => ({ loading: listsLoading, lists }),
    [listsLoading, lists],
  );

  const listFailures = Object.entries(lists)
    .filter(([, entry]) => entry.error)
    .map(([name, entry]) => `${REFERENCE_LISTS[name].label}: ${entry.error.message}`);

  const ordered = useMemo(() => {
    const rank = (key) => {
      const index = GROUP_ORDER.indexOf(key);
      return index === -1 ? GROUP_ORDER.length : index;
    };
    return [...groups].sort((a, b) => rank(a.key) - rank(b.key));
  }, [groups]);

  const replaceGroup = useCallback(
    (updated) => {
      setGroups((current) =>
        current.map((group) =>
          group.key === updated.key ? { ...group, ...updated } : group,
        ),
      );
    },
    [setGroups],
  );

  if (loading) {
    return (
      <Page title="Settings">
        <Center h={240}>
          <Loader color="accent" />
        </Center>
      </Page>
    );
  }

  return (
    <Page title="Settings" subtitle="Server configuration, stored in the database">
      {error && (
        <Alert color="red" variant="light" icon={<TriangleAlert size={16} />} mb="md">
          {error.message}
        </Alert>
      )}

      {!error && ordered.length === 0 && (
        <Text size="sm" c="dimmed">
          The server returned no settings.
        </Text>
      )}

      {listFailures.length > 0 && (
        <Alert color="red" variant="light" icon={<TriangleAlert size={16} />} mb="md">
          {`Could not list ${listFailures.join('; ')}. Those settings show the stored row id instead of a name.`}
        </Alert>
      )}

      <Accordion
        variant="separated"
        multiple
        defaultValue={[APPEARANCE, ...ordered.map((group) => group.key)]}
        styles={{
          item: {
            background: 'var(--mantine-color-dark-8)',
            border: '1px solid var(--mantine-color-dark-6)',
          },
        }}
      >
        <Accordion.Item value={APPEARANCE}>
          <Accordion.Control>
            <Text fw={500} size="sm">
              Appearance
            </Text>
          </Accordion.Control>
          <Accordion.Panel>
            <AppearanceSettings />
          </Accordion.Panel>
        </Accordion.Item>
        {ordered.map((group) => (
          <Accordion.Item key={group.key} value={group.key}>
            <Accordion.Control>
              <Text fw={500} size="sm">
                {group.name}
              </Text>
            </Accordion.Control>
            <Accordion.Panel>
              <SettingsSection
                group={group}
                references={references}
                onSaved={replaceGroup}
              />
            </Accordion.Panel>
          </Accordion.Item>
        ))}
      </Accordion>
    </Page>
  );
}

/**
 * The `network_access` map as the server must receive it.
 *
 * Every endpoint the form holds, in one request. `merge_network_access` folds
 * what it is given into the stored map, so an endpoint left out of the request
 * simply keeps whatever is stored — which means "the user emptied this one"
 * can only be said with an explicit `null`, the value that removes a key.
 * Sending `""` instead stores a key that reads like a configured restriction
 * and gates nothing.
 *
 * An endpoint that was never stored and is still empty is left out entirely:
 * asking to remove a key that does not exist is noise in a request whose whole
 * subject is who may reach this server.
 */
function networkPayload(stored, draft) {
  const payload = {};
  for (const [key, list] of Object.entries(draft)) {
    const entries = String(list).trim();
    if (entries.split(',').every((entry) => !entry.trim())) {
      if (key in stored) payload[key] = null;
    } else {
      payload[key] = entries;
    }
  }
  return payload;
}

/**
 * One settings group. Sends only the changed fields, because the server merges
 * a partial object into the stored blob — except for `network_access`, which
 * sends every endpoint it rendered so that an emptied one can be removed by
 * name rather than left behind.
 */
function SettingsSection({ group, references, onSaved }) {
  const [draft, setDraft] = useState(group.value);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState(null);

  const changed = useMemo(
    () =>
      Object.fromEntries(
        Object.entries(draft).filter(
          ([field, value]) =>
            JSON.stringify(value) !== JSON.stringify(group.value[field]),
        ),
      ),
    [draft, group.value],
  );

  const dirty = Object.keys(changed).length > 0;
  const errors = useMemo(() => fieldErrors(group.key, draft), [group.key, draft]);
  const blocked = Object.keys(errors).length > 0;

  const set = (field, value) => setDraft((current) => ({ ...current, [field]: value }));

  const save = async () => {
    setBusy(true);
    try {
      const payload =
        group.key === NETWORK_ACCESS ? networkPayload(group.value, draft) : changed;
      const updated = await settingsApi.update(group.key, payload);
      // The server is authoritative: it may coerce or reject individual fields.
      const value = updated?.value ?? updated ?? draft;
      setDraft(value);
      setFailure(null);
      onSaved({ key: group.key, name: updated?.name ?? group.name, value });
      notifyDone(`Saved ${group.name}`);
    } catch (rejection) {
      // Inline as well as in a notification: a 400 here names the entries that
      // were refused, and that has to stay on screen next to the fields being
      // fixed rather than fading out of the corner.
      setFailure(rejection);
      notifyError('Could not save the settings', rejection);
    } finally {
      setBusy(false);
    }
  };

  const fields = Object.keys(group.value).filter(
    (field) => !FIELD_META[`${group.key}.${field}`]?.hidden,
  );
  const network = group.key === NETWORK_ACCESS;

  return (
    <Stack gap="sm">
      {GROUP_HELP[group.key] && (
        <Text size="xs" c="dimmed">
          {GROUP_HELP[group.key]}
        </Text>
      )}

      {failure && (
        <Alert color="red" variant="light" icon={<TriangleAlert size={16} />}>
          {failure.message}
        </Alert>
      )}

      {!network && fields.length === 0 && (
        <Text size="xs" c="dimmed">
          Nothing configured in this section.
        </Text>
      )}

      {network ? (
        <NetworkAccessFields draft={draft} onChange={set} />
      ) : (
        fields.map((field) => (
          <SettingField
            key={field}
            groupKey={group.key}
            field={field}
            value={draft[field]}
            error={errors[field]}
            references={references}
            onChange={(value) => set(field, value)}
          />
        ))
      )}

      <Group justify="flex-end" gap="xs">
        <Button
          variant="default"
          disabled={!dirty || busy}
          onClick={() => setDraft(group.value)}
        >
          Revert
        </Button>
        <Button disabled={!dirty || blocked} loading={busy} onClick={save}>
          Save
        </Button>
      </Group>

      {group.key === BACKUPS && <BackupList />}
    </Stack>
  );
}

/**
 * The four endpoint classes, always, in the order they gate the surface: the
 * admin UI first, then the two anonymous outputs, then the Xtream API.
 *
 * Iterating the stored map instead would render nothing on a fresh instance —
 * the map is empty until someone restricts something, which is exactly when
 * nobody can find the control.
 */
function NetworkAccessFields({ draft, onChange }) {
  const known = new Set(NETWORK_ENDPOINTS.map((endpoint) => endpoint.key));
  const extra = Object.keys(draft).filter((key) => !known.has(key));

  const row = ({ key, label, help, warning }) => (
    <Stack gap={4} key={key}>
      <TextInput
        label={label ?? key}
        description={help}
        placeholder="Open to everyone"
        value={draft[key] ?? ''}
        onChange={(event) => onChange(key, event.currentTarget.value)}
      />
      {warning && (
        <Text size="xs" c="yellow.6">
          {warning}
        </Text>
      )}
    </Stack>
  );

  return (
    <>
      {NETWORK_ENDPOINTS.map(row)}
      {/* A class this build has not heard of still has to be editable, and
          must survive the save rather than being dropped from the map. */}
      {extra.map((key) => row({ key }))}
    </>
  );
}

/** The declared type, or a guess from the value when none was declared. */
function controlType(meta, value) {
  if (meta.type) return meta.type;
  if (typeof value === 'boolean') return 'boolean';
  if (typeof value === 'number') return 'number';
  if (Array.isArray(value)) return 'tags';
  return 'string';
}

/**
 * Renders one setting from its declared type, falling back to the runtime type
 * of its value.
 *
 * The alternative — a hand-written form per group — breaks silently the moment
 * the server adds a field, and says nothing about the field it is missing.
 */
function SettingField({ groupKey, field, value, error, references, onChange }) {
  const meta = FIELD_META[`${groupKey}.${field}`] ?? {};
  const label = meta.label ?? humanize(field);
  const description = meta.unit
    ? [meta.help, `In ${meta.unit}.`].filter(Boolean).join(' ')
    : meta.help;
  const shared = { label, description, error };
  const type = controlType(meta, value);

  if (type === 'hashKeys') {
    return (
      <MultiSelect
        {...shared}
        data={HASH_KEY_TOKENS}
        value={parseHashKeys(value)}
        onChange={(next) => onChange(joinHashKeys(next))}
        placeholder={parseHashKeys(value).length === 0 ? 'Nothing selected' : undefined}
      />
    );
  }

  if (type === 'reference') {
    const list = references.lists[meta.resource];

    if (references.loading) {
      return (
        <Select {...shared} data={[]} value={null} placeholder="Loading…" disabled />
      );
    }

    // No options means the list never arrived: keep the row id editable rather
    // than leaving a setting nobody can change until the endpoint comes back.
    if (list?.options) {
      return (
        <Select
          {...shared}
          // Every setting that points at a row is an `Option<Id>` server-side,
          // so "none" is always a valid answer and gets an option of its own
          // rather than only the clear button, which is easy to miss.
          data={[{ value: '', label: 'Not set' }, ...list.options]}
          value={value === null || value === undefined ? null : String(value)}
          searchable
          clearable
          placeholder="Not set"
          onChange={(next) =>
            onChange(next === null || next === '' ? null : Number(next))
          }
        />
      );
    }
  }

  if (type === 'boolean') {
    return (
      <Switch
        {...shared}
        checked={Boolean(value)}
        onChange={(event) => onChange(event.currentTarget.checked)}
      />
    );
  }

  // A reference whose list failed to load lands here: the id is what the
  // server stores anyway, so the setting stays editable without the names.
  if (type === 'number' || type === 'reference') {
    return (
      <NumberInput
        {...shared}
        value={value ?? ''}
        placeholder={meta.nullable ? 'Not set' : undefined}
        onChange={(next) => {
          // Mantine hands back '' for an emptied field. For a nullable setting
          // that means "unset"; coercing it to 0 would silently pick profile 0.
          if (next === '' || next === null) return onChange(meta.nullable ? null : 0);
          return onChange(Number(next));
        }}
      />
    );
  }

  if (type === 'tags') {
    return (
      <TagsInput
        {...shared}
        value={Array.isArray(value) ? value.map(String) : []}
        onChange={onChange}
        placeholder="Add and press Enter"
      />
    );
  }

  // No group stores a nested object today — `network_access` is a flat map, so
  // its endpoints arrive as ordinary string fields. Showing one read-only beats
  // letting the text fallback below render it as "[object Object]" and then
  // save that string back.
  if (value !== null && typeof value === 'object') {
    return (
      <Stack gap={2}>
        <Text size="xs" fw={500}>
          {label}
        </Text>
        <Code block fz={11}>
          {JSON.stringify(value, null, 2)}
        </Code>
        <Text size="xs" c="dimmed">
          Not editable here.
        </Text>
      </Stack>
    );
  }

  return (
    <TextInput
      {...shared}
      value={value ?? ''}
      placeholder={meta.nullable ? 'Not set' : undefined}
      onChange={(event) => {
        // A non-nullable string field must keep sending '' when emptied. Only a
        // field declared nullable may send null, which is what the server's
        // `Option<String>` wants and what '' would fail deserialization on.
        const next = event.currentTarget.value;
        onChange(meta.nullable && next === '' ? null : next);
      }}
    />
  );
}
