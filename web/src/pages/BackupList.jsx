import { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  Button,
  FileButton,
  Group,
  Loader,
  Stack,
  Table,
  Text,
} from '@mantine/core';
import {
  DatabaseBackup,
  Download,
  History,
  Trash2,
  TriangleAlert,
  Upload,
} from 'lucide-react';

import { backups as backupsApi, fetchVersion } from '../api/resources.js';
import { useResource } from '../api/useResource.js';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { RowAction } from '../components/RowAction.jsx';
import { absoluteTime, formatBytes } from '../format.js';
import { notifyDone, notifyError, notifyQuiet } from '../notify.js';

const TRIGGERS = {
  manual: 'By hand',
  scheduled: 'Scheduled',
  uploaded: 'Uploaded',
  'pre-restore': 'Before a restore',
};

/**
 * Often enough that the page comes back soon after the server does, and slow
 * enough that a server which never comes back is not asked dozens of times a
 * minute. Otherwise arbitrary.
 */
const POLL_MS = 1000;

/**
 * The backups in `backups/`, and every action on them.
 *
 * Below the schedule's own fields in the Settings section, because the list is
 * what those fields produce.
 */
export function BackupList() {
  const load = useCallback(() => backupsApi.list(), []);
  const { data: entries, loading, error, reload } = useResource(load, []);
  const [busy, setBusy] = useState(null);
  const [confirming, setConfirming] = useState(null);
  const [restarting, setRestarting] = useState(null);

  const create = async () => {
    setBusy('create');
    try {
      const entry = await backupsApi.create();
      notifyDone(`Backed up as ${entry.name}`);
      void reload();
    } catch (failure) {
      notifyError('Could not back up', failure);
    } finally {
      setBusy(null);
    }
  };

  const upload = async (file) => {
    if (!file) return;
    setBusy('upload');
    try {
      const entry = await backupsApi.upload(file);
      notifyDone(`Uploaded as ${entry.name}`);
      void reload();
    } catch (failure) {
      notifyError(`Could not upload ${file.name}`, failure);
    } finally {
      setBusy(null);
    }
  };

  // Fetched with the session's token, which a plain link cannot carry, then
  // handed to the browser as a file.
  const download = async (name) => {
    try {
      const blob = await backupsApi.download(name);
      const url = URL.createObjectURL(blob);
      const link = document.createElement('a');
      link.href = url;
      link.download = name;
      link.click();
      URL.revokeObjectURL(url);
    } catch (failure) {
      notifyError(`Could not download ${name}`, failure);
    }
  };

  const remove = async (name) => {
    try {
      await backupsApi.remove(name);
      notifyQuiet(`Deleted ${name}`);
    } catch (failure) {
      notifyError(`Could not delete ${name}`, failure);
    }
    void reload();
  };

  const restore = async (name) => {
    try {
      const accepted = await backupsApi.restore(name);
      setRestarting({ name, preRestore: accepted.pre_restore });
    } catch (failure) {
      notifyError(`Could not restore ${name}`, failure);
    }
  };

  if (restarting) return <Restarting {...restarting} />;

  return (
    <Stack gap="xs" mt="sm">
      <Group justify="space-between">
        <Text size="sm" fw={500}>
          Stored on the server
        </Text>
        <Group gap="xs">
          <FileButton onChange={upload} accept=".zip,application/zip">
            {(props) => (
              <Button
                {...props}
                size="xs"
                variant="default"
                leftSection={<Upload size={14} />}
                loading={busy === 'upload'}
                disabled={busy !== null}
              >
                Upload a backup
              </Button>
            )}
          </FileButton>
          <Button
            size="xs"
            leftSection={<DatabaseBackup size={14} />}
            loading={busy === 'create'}
            disabled={busy !== null}
            onClick={create}
          >
            Back up now
          </Button>
        </Group>
      </Group>

      {error && (
        <Alert color="red" variant="light" icon={<TriangleAlert size={16} />}>
          {error.message}
        </Alert>
      )}

      {loading && entries.length === 0 ? (
        <Loader size="sm" color="accent" />
      ) : entries.length === 0 ? (
        !error && (
          <Text size="xs" c="dimmed">
            No backups yet.
          </Text>
        )
      ) : (
        <Table fz="xs" verticalSpacing={4} aria-label="Backups">
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Name</Table.Th>
              <Table.Th>Created</Table.Th>
              <Table.Th>Trigger</Table.Th>
              <Table.Th>Size</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {entries.map((entry) => (
              <Table.Tr key={entry.name}>
                <Table.Td>{entry.name}</Table.Td>
                <Table.Td>{absoluteTime(entry.created_at)}</Table.Td>
                <Table.Td>{TRIGGERS[entry.trigger] ?? entry.trigger}</Table.Td>
                <Table.Td>{formatBytes(entry.size_bytes)}</Table.Td>
                <Table.Td>
                  <Group gap={2} justify="flex-end" wrap="nowrap">
                    <RowAction
                      label={`Download ${entry.name}`}
                      tooltip="Download"
                      onClick={() => download(entry.name)}
                    >
                      <Download size={14} />
                    </RowAction>
                    <RowAction
                      label={`Restore ${entry.name}`}
                      tooltip="Restore"
                      onClick={() =>
                        setConfirming({
                          title: `Restore ${entry.name}`,
                          message:
                            'Everything in this instance — channels, sources, settings and ' +
                            'users — is replaced by this backup. A backup of the instance ' +
                            'as it is now is taken first and kept in this list. The server ' +
                            'then restarts, which disconnects every viewer.',
                          confirmLabel: 'Restore',
                          onConfirm: () => restore(entry.name),
                        })
                      }
                    >
                      <History size={14} />
                    </RowAction>
                    <RowAction
                      label={`Delete ${entry.name}`}
                      tooltip="Delete"
                      color="red"
                      onClick={() =>
                        setConfirming({
                          title: `Delete ${entry.name}`,
                          message:
                            'The file is deleted from the server, and there is no undo. A ' +
                            'copy you have downloaded is unaffected.',
                          onConfirm: () => remove(entry.name),
                        })
                      }
                    >
                      <Trash2 size={14} />
                    </RowAction>
                  </Group>
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      )}

      {confirming && <ConfirmModal {...confirming} onClose={() => setConfirming(null)} />}
    </Stack>
  );
}

/**
 * Shown from the moment a restore is accepted until the server is back.
 *
 * The server is still answering when the restore is accepted, so answering is
 * not the signal: the page waits to see it go away and then come back, and
 * only then reloads into the restored instance. A server with nothing to
 * restart it never comes back, which is why this says what to do then, and
 * why a reload is always one click away should the gap be too short to see.
 */
function Restarting({ name, preRestore }) {
  useEffect(() => {
    let cancelled = false;
    let wentAway = false;
    let timer;

    const poll = async () => {
      const version = await fetchVersion();
      if (cancelled) return;
      if (version === null) {
        wentAway = true;
      } else if (wentAway) {
        window.location.reload();
        return;
      }
      timer = setTimeout(poll, POLL_MS);
    };
    timer = setTimeout(poll, POLL_MS);

    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, []);

  return (
    <Alert color="yellow" variant="light" mt="sm" title="Restarting" role="status">
      <Stack gap="xs">
        <Text size="sm">
          {`Restoring ${name}. The instance as it was is saved as ${preRestore}.`}
        </Text>
        <Text size="sm">
          This page reloads when the server answers again. If it does not come back,
          nothing restarted the process: start it again by hand.
        </Text>
        <Group>
          <Button size="xs" variant="default" onClick={() => window.location.reload()}>
            Reload now
          </Button>
        </Group>
      </Stack>
    </Alert>
  );
}
