import { useCallback, useMemo, useState } from 'react';
import {
  Anchor,
  Badge,
  Button,
  Group,
  Modal,
  Stack,
  Text,
  TextInput,
} from '@mantine/core';
import { useForm } from '@mantine/form';
import { notifyDone, notifyError, notifyQuiet } from '../notify.js';
import { Pencil, Plus, Trash2 } from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { DataTable } from '../components/DataTable.jsx';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { LogoImage } from '../components/LogoImage.jsx';
import { useResource } from '../api/useResource.js';
import { logos as logosApi } from '../api/resources.js';
import classes from './Logos.module.css';
import { RowAction } from '../components/RowAction.jsx';

/**
 * The URL column is a unique index, so a duplicate comes back as a raw SQLite
 * message. Nobody should have to read "UNIQUE constraint failed: logo.url".
 */
function describeFailure(failure, fallback) {
  if (failure?.status === 409) return 'A logo with that URL already exists.';
  return failure?.message ?? fallback;
}

export function Logos() {
  // Null until the table says what it wants: the table reports its query as
  // soon as it mounts, and seeding a second object holding the same values
  // here fetches the same rows twice.
  const [query, setQuery] = useState(null);
  const [editing, setEditing] = useState(null);
  const [confirming, setConfirming] = useState(null);
  const [selected, setSelected] = useState([]);

  const load = useCallback(
    () =>
      query
        ? logosApi.list({
            page: query.pageIndex + 1,
            pageSize: query.pageSize,
            search: query.search || undefined,
            ordering: query.ordering,
          })
        : Promise.resolve({ results: [], count: 0 }),
    [query],
  );

  const {
    data,
    loading: fetching,
    error,
    reload,
  } = useResource(load, {
    results: [],
    count: 0,
  });
  const loading = fetching || query === null;

  const remove = useCallback(
    async (logo) => {
      try {
        await logosApi.remove(logo.id);
        notifyQuiet(`Deleted ${logo.name}`);
        await reload();
      } catch (failure) {
        notifyError('Could not delete the logo', {
          message: describeFailure(failure, 'The server refused the request.'),
        });
      }
    },
    [reload],
  );

  const bulkDelete = useCallback(async () => {
    try {
      const { deleted } = await logosApi.bulkDelete(selected.map(Number));
      notifyQuiet(`Deleted ${deleted} logos`);
      await reload();
    } catch (failure) {
      notifyError('Could not delete the logos', failure);
    }
  }, [selected, reload]);

  const cleanup = useCallback(async () => {
    try {
      const { deleted } = await logosApi.cleanup();
      notifyQuiet(deleted ? `Removed ${deleted} unused logos` : 'No unused logos');
      await reload();
    } catch (failure) {
      notifyError('Could not remove unused logos', failure);
    }
  }, [reload]);

  /**
   * Deleting an in-use logo is not refused: both `channel.logo_id` and
   * `channel_override.logo_id` are `ON DELETE SET NULL`, so the row goes and
   * every channel using it silently loses its artwork. The server will not warn
   * about that, so this is the only place it can be said.
   */
  const confirmDelete = useCallback((logo) => {
    const inUse = logo.channel_count > 0;
    setConfirming({
      title: 'Delete logo',
      message: inUse
        ? `${logo.name} is used by ${logo.channel_count} ${
            logo.channel_count === 1 ? 'channel' : 'channels'
          }. Deleting it leaves ${logo.channel_count === 1 ? 'that channel' : 'those channels'} with no artwork.`
        : `Delete ${logo.name}? It is not used by any channel.`,
      logo,
    });
  }, []);

  const columns = useMemo(
    () => [
      {
        id: 'artwork',
        header: '',
        size: 74,
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ row }) => <LogoImage src={row.original.url} alt={row.original.name} />,
      },
      { accessorKey: 'name', header: 'Name', size: 220 },
      {
        accessorKey: 'url',
        header: 'URL',
        size: 320,
        cell: ({ getValue }) => (
          <Anchor
            href={getValue()}
            target="_blank"
            rel="noreferrer noopener"
            className={classes.url}
            title={getValue()}
          >
            {getValue()}
          </Anchor>
        ),
      },
      {
        id: 'channel_count',
        accessorFn: (row) => row.channel_count ?? 0,
        header: 'Used by',
        size: 110,
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ getValue }) => {
          const count = getValue();
          return count > 0 ? (
            <Badge size="xs" variant="light" color="accent">
              {count} {count === 1 ? 'channel' : 'channels'}
            </Badge>
          ) : (
            <Text component="span" size="xs" c="dimmed">
              Unused
            </Text>
          );
        },
      },
    ],
    [],
  );

  const rowActions = useCallback(
    (logo) => (
      <Group gap={2} justify="flex-end" wrap="nowrap">
        <RowAction
          label={`Edit ${logo.name}`}
          tooltip="Edit"
          onClick={() => setEditing(logo)}
        >
          <Pencil size={14} />
        </RowAction>
        <RowAction
          label={`Delete ${logo.name}`}
          tooltip="Delete"
          color="red"
          onClick={() => confirmDelete(logo)}
        >
          <Trash2 size={14} />
        </RowAction>
      </Group>
    ),
    [confirmDelete],
  );

  const getRowId = useCallback((logo) => String(logo.id), []);

  return (
    <Page
      title="Logos"
      subtitle="Channel artwork, and which channels use it"
      actions={
        <Button leftSection={<Plus size={14} />} onClick={() => setEditing({})}>
          Add logo
        </Button>
      }
    >
      <DataTable
        label="Logos"
        data={data.results}
        columns={columns}
        getRowId={getRowId}
        loading={loading}
        error={error}
        enableSelection
        onSelectionChange={setSelected}
        rowActions={rowActions}
        rowCount={data.count}
        onQueryChange={setQuery}
        searchPlaceholder="Search name or URL"
        emptyMessage={error ? 'Logos could not be loaded.' : 'No logos yet.'}
        toolbar={
          <Group gap={6} wrap="nowrap">
            {selected.length > 0 && (
              <>
                <Text size="xs" c="dimmed">
                  {selected.length} selected
                </Text>
                <Button
                  size="xs"
                  variant="light"
                  color="red"
                  leftSection={<Trash2 size={13} />}
                  onClick={() =>
                    setConfirming({
                      title: 'Delete logos',
                      message: `Delete ${selected.length} logos? Any channel using one is left with no artwork.`,
                      onConfirm: bulkDelete,
                    })
                  }
                >
                  Delete
                </Button>
              </>
            )}
            <Button
              size="xs"
              variant="default"
              onClick={() =>
                setConfirming({
                  title: 'Remove unused logos',
                  message:
                    'Delete every logo no channel references? Channels keep the artwork they use.',
                  confirmLabel: 'Remove unused',
                  onConfirm: cleanup,
                })
              }
            >
              Remove unused
            </Button>
          </Group>
        }
      />

      {confirming && (
        <ConfirmModal
          title={confirming.title}
          message={confirming.message}
          confirmLabel={confirming.confirmLabel}
          onConfirm={confirming.onConfirm ?? (() => remove(confirming.logo))}
          onClose={() => setConfirming(null)}
        />
      )}

      {editing && (
        <LogoModal
          logo={editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void reload();
          }}
        />
      )}
    </Page>
  );
}

function LogoModal({ logo, onClose, onSaved }) {
  const isEdit = Boolean(logo.id);
  const [busy, setBusy] = useState(false);

  const form = useForm({
    initialValues: { name: logo.name ?? '', url: logo.url ?? '' },
    validate: {
      url: (value) => (value.trim() ? null : 'Required'),
    },
  });

  const submit = async (values) => {
    setBusy(true);
    const url = values.url.trim();
    // The server defaults an absent name to the URL, which reads badly in a
    // list; sending the URL explicitly keeps that behaviour visible here.
    const name = values.name.trim() || url;

    try {
      if (isEdit) await logosApi.update(logo.id, { name, url });
      else await logosApi.create(name, url);
      notifyDone(`Saved ${name}`);
      onSaved();
    } catch (failure) {
      const message = describeFailure(failure, 'Could not save the logo.');
      // A duplicate URL is the one failure this form can point at a field,
      // and it is derived from the status rather than from a body shape the
      // server does not send.
      if (failure?.status === 409) form.setFieldError('url', message);
      notifyError('Could not save the logo', { message });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      opened
      onClose={onClose}
      title={isEdit ? `Edit ${logo.name}` : 'Add logo'}
      size="md"
    >
      <form onSubmit={form.onSubmit(submit)}>
        <Stack gap="sm">
          <TextInput
            label="URL"
            withAsterisk
            description="Must be unique — the same artwork is reused rather than duplicated."
            {...form.getInputProps('url')}
          />
          <TextInput
            label="Name"
            description="Defaults to the URL."
            {...form.getInputProps('name')}
          />

          <Group gap="sm" align="center">
            <Text size="xs" c="dimmed">
              Preview
            </Text>
            <LogoImage
              src={form.values.url.trim() || null}
              alt="Logo preview"
              size={40}
            />
          </Group>

          <Group justify="flex-end" gap="xs" mt="xs">
            <Button variant="default" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" loading={busy}>
              Save
            </Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}
