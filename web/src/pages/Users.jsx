import { useCallback, useMemo, useState } from 'react';
import {
  ActionIcon,
  Badge,
  Button,
  Group,
  Modal,
  NumberInput,
  PasswordInput,
  Select,
  Stack,
  Switch,
  Text,
  TextInput,
  Tooltip,
} from '@mantine/core';
import { useForm } from '@mantine/form';
import { notifyDone, notifyError, notifyQuiet } from '../notify.js';
import { Pencil, Plus, Trash2 } from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { DataTable } from '../components/DataTable.jsx';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { USER_LEVELS, USER_LEVEL_LABELS, users as usersApi } from '../api/resources.js';
import { useResource } from '../api/useResource.js';
import { useSession } from '../auth/session.js';
import { RowAction } from '../components/RowAction.jsx';

// Values are the integers the API stores, as strings because Mantine's Select
// only carries string values.
const LEVELS = [USER_LEVELS.ADMIN, USER_LEVELS.STANDARD, USER_LEVELS.STREAMER].map(
  (level) => ({ value: String(level), label: USER_LEVEL_LABELS[level] }),
);

const LEVEL_COLORS = {
  [USER_LEVELS.ADMIN]: 'accent',
  [USER_LEVELS.STANDARD]: 'blue',
  [USER_LEVELS.STREAMER]: 'gray',
};

export function Users() {
  const currentUser = useSession((state) => state.user);
  const [editing, setEditing] = useState(null);
  const [confirming, setConfirming] = useState(null);
  const [selected, setSelected] = useState([]);

  const load = useCallback(() => usersApi.list(), []);
  const { data: rows, loading, error, reload } = useResource(load, []);

  const remove = useCallback(
    async (user) => {
      try {
        await usersApi.remove(user.id);
        notifyQuiet(`Deleted ${user.username}`);
        await reload();
      } catch (failure) {
        notifyError('Could not delete the user', failure);
      }
    },
    [reload],
  );

  const columns = useMemo(
    () => [
      { accessorKey: 'username', header: 'Username', size: 200 },
      {
        accessorKey: 'email',
        header: 'Email',
        size: 260,
        cell: ({ getValue }) => getValue() || <Text c="dimmed">—</Text>,
      },
      {
        accessorKey: 'user_level',
        header: 'Level',
        size: 130,
        cell: ({ getValue }) => {
          const level = getValue();
          return (
            <Badge size="xs" variant="light" color={LEVEL_COLORS[level] ?? 'gray'}>
              {LEVELS.find((entry) => entry.value === String(level))?.label ?? level}
            </Badge>
          );
        },
      },
      {
        accessorKey: 'stream_limit',
        header: 'Stream limit',
        size: 110,
        cell: ({ getValue }) => (getValue() ? getValue() : 'Unlimited'),
      },
      {
        accessorKey: 'is_active',
        header: 'Active',
        size: 90,
        cell: ({ getValue }) => (
          <Badge size="xs" variant="light" color={getValue() ? 'accent' : 'red'}>
            {getValue() ? 'Yes' : 'No'}
          </Badge>
        ),
      },
    ],
    [],
  );

  const rowActions = useCallback(
    (user) => (
      <Group gap={2} justify="flex-end" wrap="nowrap">
        <RowAction
          label={`Edit ${user.username}`}
          tooltip="Edit"
          onClick={() => setEditing(user)}
        >
          <Pencil size={14} />
        </RowAction>
        <Tooltip
          label={user.id === currentUser?.id ? 'You cannot delete yourself' : 'Delete'}
        >
          <ActionIcon
            variant="subtle"
            color="red"
            size="sm"
            aria-label={`Delete ${user.username}`}
            disabled={user.id === currentUser?.id}
            onClick={() =>
              setConfirming({
                title: `Delete ${user.username}`,
                message: `${user.username} and their API key are deleted. Any client signed in as them stops working, and there is no undo.`,
                onConfirm: () => remove(user),
              })
            }
          >
            <Trash2 size={14} />
          </ActionIcon>
        </Tooltip>
      </Group>
    ),
    [currentUser?.id, remove],
  );

  const getRowId = useCallback((user) => String(user.id), []);

  return (
    <Page
      title="Users"
      subtitle="Accounts that can sign in or stream"
      actions={
        <Button leftSection={<Plus size={14} />} onClick={() => setEditing({})}>
          Add user
        </Button>
      }
    >
      <DataTable
        label="Users"
        data={rows}
        columns={columns}
        getRowId={getRowId}
        loading={loading}
        error={error}
        enableSelection
        onSelectionChange={setSelected}
        rowActions={rowActions}
        emptyMessage={error ? 'Users could not be loaded.' : 'No users.'}
        toolbar={
          selected.length > 0 && (
            <Text size="xs" c="dimmed">
              {selected.length} selected
            </Text>
          )
        }
      />

      {confirming && (
        <ConfirmModal
          title={confirming.title}
          message={confirming.message}
          onConfirm={confirming.onConfirm}
          onClose={() => setConfirming(null)}
        />
      )}
      {/* Mounted only while open, so the form starts from this user's values
          rather than needing an effect to copy them in. */}
      {editing && (
        <UserModal
          user={editing}
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

function UserModal({ user, onClose, onSaved }) {
  const isEdit = Boolean(user.id);
  const [busy, setBusy] = useState(false);
  const hasXcPassword = Boolean(user.custom_properties?.xc_password);

  const form = useForm({
    initialValues: {
      username: user.username ?? '',
      email: user.email ?? '',
      password: '',
      xc_password: '',
      user_level: String(user.user_level ?? USER_LEVELS.STANDARD),
      stream_limit: user.stream_limit ?? 0,
      is_active: user.is_active ?? true,
    },
    // `isEdit` is fixed for this component's lifetime — the modal is mounted
    // fresh per user — so capturing it in the closure is safe.
    validate: {
      username: (value) => (value.trim() ? null : 'Required'),
      password: (value) => (isEdit || value ? null : 'Required for a new user'),
    },
  });

  const submit = async (values) => {
    setBusy(true);
    const payload = {
      username: values.username.trim(),
      email: values.email.trim() || null,
      user_level: Number(values.user_level),
      stream_limit: Number(values.stream_limit) || 0,
      is_active: values.is_active,
    };
    // An empty password field on edit means "leave it alone", not "blank it".
    if (values.password) payload.password = values.password;
    // Same rule, and the same reason the field renders empty for an existing
    // user: the server merges `custom_properties`, so sending nothing leaves
    // whatever is there.
    if (values.xc_password) {
      payload.custom_properties = { xc_password: values.xc_password };
    }

    try {
      if (isEdit) await usersApi.update(user.id, payload);
      else await usersApi.create(payload);
      notifyDone(`Saved ${payload.username}`);
      onSaved();
    } catch (failure) {
      notifyError('Could not save the user', failure);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      opened
      onClose={onClose}
      title={isEdit ? `Edit ${user.username}` : 'Add user'}
      size="sm"
    >
      <form onSubmit={form.onSubmit(submit)}>
        <Stack gap="sm">
          <TextInput label="Username" withAsterisk {...form.getInputProps('username')} />
          <TextInput label="Email" type="email" {...form.getInputProps('email')} />
          <PasswordInput
            label="Password"
            withAsterisk={!isEdit}
            description={isEdit ? 'Leave blank to keep the current password' : undefined}
            autoComplete="new-password"
            {...form.getInputProps('password')}
          />
          <PasswordInput
            label="Xtream password"
            description={
              hasXcPassword
                ? 'Set. Leave blank to keep it, or type to replace it.'
                : 'Needed before an Xtream player can sign in as this user. Not the password above.'
            }
            autoComplete="new-password"
            {...form.getInputProps('xc_password')}
          />
          <Select
            label="Level"
            data={LEVELS}
            allowDeselect={false}
            {...form.getInputProps('user_level')}
          />
          <NumberInput
            label="Stream limit"
            description="Reported to Xtream players as their connection allowance. Not enforced by the proxy. 0 means unlimited."
            min={0}
            {...form.getInputProps('stream_limit')}
          />
          <Switch
            label="Active"
            {...form.getInputProps('is_active', { type: 'checkbox' })}
          />
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
