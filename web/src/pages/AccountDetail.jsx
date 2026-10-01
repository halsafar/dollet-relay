import { useCallback, useState } from 'react';
import { Link } from 'react-router-dom';
import {
  ActionIcon,
  Accordion,
  Alert,
  Anchor,
  Badge,
  Button,
  Code,
  Group,
  Loader,
  Modal,
  Stack,
  Switch,
  Text,
  TextInput,
} from '@mantine/core';
import { notifyError } from '../notify.js';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { Info, Plus, Trash2 } from 'lucide-react';

import { useResource } from '../api/useResource.js';
import { m3uAccounts } from '../api/resources.js';
import classes from './Sources.module.css';

/**
 * Provider groups, rewrite profiles and import filters for one account.
 *
 * Separate from the account form because these are the settings that decide
 * *what gets imported* rather than *where from*, and because each is a
 * collection with its own round trips.
 */
export function AccountDetail({ account, onClose }) {
  return (
    <Modal opened onClose={onClose} title={`${account.name} — import rules`} size="lg">
      <Accordion variant="separated" multiple defaultValue={['groups']}>
        <Accordion.Item value="groups">
          <Accordion.Control>
            <Text fw={500} size="sm">
              Groups
            </Text>
          </Accordion.Control>
          <Accordion.Panel>
            <Groups account={account} />
          </Accordion.Panel>
        </Accordion.Item>

        <Accordion.Item value="profiles">
          <Accordion.Control>
            <Text fw={500} size="sm">
              Rewrite profiles
            </Text>
          </Accordion.Control>
          <Accordion.Panel>
            <Profiles account={account} />
          </Accordion.Panel>
        </Accordion.Item>

        <Accordion.Item value="filters">
          <Accordion.Control>
            <Text fw={500} size="sm">
              Import filters
            </Text>
          </Accordion.Control>
          <Accordion.Panel>
            <Filters account={account} />
          </Accordion.Panel>
        </Accordion.Item>
      </Accordion>
    </Modal>
  );
}

/**
 * The provider's groups live on the Groups page, where each has its range and
 * its standing with every provider; this only says how many of them this
 * account imports and points there. Two places to edit the same link would
 * disagree the moment one of them was open while the other saved.
 */
function Groups({ account }) {
  const load = useCallback(() => m3uAccounts.groups(account.id), [account.id]);
  const { data: groups, loading } = useResource(load, []);

  const imported = groups.filter((group) => group.enabled).length;
  const synced = groups.filter((group) => group.auto_channel_sync).length;

  return (
    <Stack gap={8}>
      {loading && <Loader size="xs" color="gray" />}
      {!loading && groups.length === 0 && (
        <Text size="xs" c="dimmed">
          No groups yet. They appear after the first refresh.
        </Text>
      )}
      {!loading && groups.length > 0 && (
        <Text size="sm">
          Importing {imported} of {groups.length}{' '}
          {groups.length === 1 ? 'group' : 'groups'}
          {synced > 0 && `, ${synced} auto-synced`}.
        </Text>
      )}
      <Anchor component={Link} to={`/groups?account=${account.id}`} size="sm">
        Manage groups →
      </Anchor>
    </Stack>
  );
}

function Profiles({ account }) {
  const load = useCallback(() => m3uAccounts.profiles(account.id), [account.id]);
  const { data: profiles, loading, reload } = useResource(load, []);
  const [draft, setDraft] = useState(null);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(null);

  const save = async () => {
    setBusy(true);
    try {
      await m3uAccounts.createProfile(account.id, {
        name: draft.name.trim(),
        search_pattern: draft.search_pattern,
        replace_pattern: draft.replace_pattern,
      });
      setDraft(null);
      await reload();
    } catch (failure) {
      // The server compiles the pattern before storing it, so an unusable
      // regex is refused here rather than discovered at the next refresh.
      notifyError('Could not save the profile', failure);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (profile) => {
    try {
      await m3uAccounts.removeProfile(account.id, profile.id);
      await reload();
    } catch (failure) {
      notifyError('Could not delete the profile', failure);
    }
  };

  return (
    <Stack gap={8}>
      <Alert color="gray" variant="light" p="xs" fz="xs" icon={<Info size={15} />}>
        A profile rewrites the stream URL before it is played — usually to point at a
        different server or port for the same content. Patterns are PCRE-flavoured; the
        server refuses one that will not compile.
      </Alert>

      {loading && <Loader size="xs" color="gray" />}
      {!loading && profiles.length === 0 && !draft && (
        <Text size="xs" c="dimmed">
          No rewrite profiles. Streams are played at the URL the provider gave.
        </Text>
      )}

      {profiles.map((profile) => (
        <div key={profile.id} className={classes.ruleRow}>
          <div style={{ minWidth: 0, flex: 1 }}>
            <Text size="xs" fw={500}>
              {profile.name}
              {profile.is_default && (
                <Badge size="xs" variant="light" color="gray" ml={6}>
                  default
                </Badge>
              )}
            </Text>
            <Code fz={10.5} className={classes.pattern}>
              {profile.search_pattern || '(no pattern)'} → {profile.replace_pattern || ''}
            </Code>
          </div>
          {!profile.is_default && (
            <ActionIcon
              variant="subtle"
              color="red"
              size="sm"
              aria-label={`Delete profile ${profile.name}`}
              onClick={() =>
                setConfirming({
                  title: `Delete ${profile.name}`,
                  message:
                    'Streams this profile rewrites go back to the URL the provider gave, at the next refresh. There is no undo.',
                  onConfirm: () => remove(profile),
                })
              }
            >
              <Trash2 size={14} />
            </ActionIcon>
          )}
        </div>
      ))}

      {draft ? (
        <Stack gap={6}>
          <TextInput
            size="xs"
            label="Name"
            value={draft.name}
            onChange={(event) => setDraft({ ...draft, name: event.currentTarget.value })}
          />
          <TextInput
            size="xs"
            label="Search pattern"
            description="$1 in a search pattern is read as a backreference."
            value={draft.search_pattern}
            onChange={(event) =>
              setDraft({ ...draft, search_pattern: event.currentTarget.value })
            }
          />
          <TextInput
            size="xs"
            label="Replace pattern"
            description="$1 inserts the first captured group."
            value={draft.replace_pattern}
            onChange={(event) =>
              setDraft({ ...draft, replace_pattern: event.currentTarget.value })
            }
          />
          <Group gap="xs" justify="flex-end">
            <Button size="xs" variant="default" onClick={() => setDraft(null)}>
              Cancel
            </Button>
            <Button size="xs" loading={busy} onClick={save}>
              Add profile
            </Button>
          </Group>
        </Stack>
      ) : (
        <Button
          size="xs"
          variant="default"
          leftSection={<Plus size={13} />}
          onClick={() => setDraft({ name: '', search_pattern: '', replace_pattern: '' })}
        >
          Add profile
        </Button>
      )}
      {confirming && (
        <ConfirmModal
          title={confirming.title}
          message={confirming.message}
          onConfirm={confirming.onConfirm}
          onClose={() => setConfirming(null)}
        />
      )}
    </Stack>
  );
}

function Filters({ account }) {
  const load = useCallback(() => m3uAccounts.filters(account.id), [account.id]);
  const { data: filters, loading, reload } = useResource(load, []);
  const [draft, setDraft] = useState(null);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(null);

  const save = async () => {
    setBusy(true);
    try {
      await m3uAccounts.createFilter(account.id, {
        filter_type: draft.filter_type,
        regex_pattern: draft.regex_pattern,
        exclude: draft.exclude,
      });
      setDraft(null);
      await reload();
    } catch (failure) {
      notifyError('Could not save the filter', failure);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (filter) => {
    try {
      await m3uAccounts.removeFilter(account.id, filter.id);
      await reload();
    } catch (failure) {
      notifyError('Could not delete the filter', failure);
    }
  };

  return (
    <Stack gap={8}>
      <Alert color="gray" variant="light" p="xs" fz="xs" icon={<Info size={15} />}>
        Filters decide which streams are imported at all. An exclude filter drops
        everything it matches; an include filter keeps only what matches.
      </Alert>

      {loading && <Loader size="xs" color="gray" />}
      {!loading && filters.length === 0 && !draft && (
        <Text size="xs" c="dimmed">
          No filters. Every stream the provider lists is imported.
        </Text>
      )}

      {filters.map((filter) => (
        <div key={filter.id} className={classes.ruleRow}>
          <Badge size="xs" variant="light" color={filter.exclude ? 'red' : 'accent'}>
            {filter.exclude ? 'exclude' : 'include'}
          </Badge>
          <Text size="xs" c="dimmed">
            {filter.filter_type}
          </Text>
          <Code fz={10.5} className={classes.pattern} style={{ flex: 1 }}>
            {filter.regex_pattern}
          </Code>
          <ActionIcon
            variant="subtle"
            color="red"
            size="sm"
            aria-label={`Delete filter ${filter.regex_pattern}`}
            onClick={() =>
              setConfirming({
                title: 'Delete filter',
                message:
                  'Streams this rule excluded are imported at the next refresh. There is no undo.',
                onConfirm: () => remove(filter),
              })
            }
          >
            <Trash2 size={14} />
          </ActionIcon>
        </div>
      ))}

      {draft ? (
        <Stack gap={6}>
          <TextInput
            size="xs"
            label="Pattern"
            value={draft.regex_pattern}
            onChange={(event) =>
              setDraft({ ...draft, regex_pattern: event.currentTarget.value })
            }
          />
          <Switch
            size="xs"
            label="Exclude what this matches"
            checked={draft.exclude}
            onChange={(event) =>
              setDraft({ ...draft, exclude: event.currentTarget.checked })
            }
          />
          <Group gap="xs" justify="flex-end">
            <Button size="xs" variant="default" onClick={() => setDraft(null)}>
              Cancel
            </Button>
            <Button size="xs" loading={busy} onClick={save}>
              Add filter
            </Button>
          </Group>
        </Stack>
      ) : (
        <Button
          size="xs"
          variant="default"
          leftSection={<Plus size={13} />}
          onClick={() =>
            setDraft({ filter_type: 'name', regex_pattern: '', exclude: true })
          }
        >
          Add filter
        </Button>
      )}
      {confirming && (
        <ConfirmModal
          title={confirming.title}
          message={confirming.message}
          onConfirm={confirming.onConfirm}
          onClose={() => setConfirming(null)}
        />
      )}
    </Stack>
  );
}
