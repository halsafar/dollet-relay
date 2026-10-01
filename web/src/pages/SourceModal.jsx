import { useState } from 'react';
import {
  Alert,
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
} from '@mantine/core';
import { useForm } from '@mantine/form';
import { notifyDone, notifyError } from '../notify.js';
import { Info, ShieldCheck } from 'lucide-react';

import { epgSources, m3uAccounts } from '../api/resources.js';
import { describeStaleDays } from './staleStreams.js';
import classes from './Sources.module.css';

const ACCOUNT_TYPES = [
  { value: 'standard', label: 'M3U playlist' },
  { value: 'xtream_codes', label: 'Xtream Codes' },
];

const EPG_TYPES = [
  { value: 'xmltv', label: 'XMLTV' },
  { value: 'dummy', label: 'Dummy (generated)' },
];

/**
 * Create or edit an M3U account or an EPG source.
 *
 * One component for both because they differ in three fields and share every
 * concern that matters here: a write-only password, a refresh interval, and a
 * priority that decides which source wins a conflict.
 */
export function SourceModal({ kind, source, onClose, onSaved }) {
  const isM3u = kind === 'm3u';
  const isEdit = Boolean(source.id);
  const [busy, setBusy] = useState(false);

  const form = useForm({
    initialValues: {
      name: source.name ?? '',
      account_type: source.account_type ?? 'standard',
      source_type: source.source_type ?? 'xmltv',
      server_url: source.server_url ?? '',
      url: source.url ?? '',
      file_path: source.file_path ?? '',
      username: source.username ?? '',
      // Never prefilled: the server does not return it, so an empty box here
      // means "leave it alone", not "blank it".
      password: '',
      max_streams: source.max_streams ?? 0,
      priority: source.priority ?? 0,
      refresh_interval_hours: source.refresh_interval_hours ?? 24,
      stale_stream_days: source.stale_stream_days ?? 0,
      is_active: source.is_active ?? true,
    },
    validate: {
      name: (value) => (value.trim() ? null : 'Required'),
    },
  });

  const dummy = !isM3u && form.values.source_type === 'dummy';

  const submit = async (values) => {
    setBusy(true);
    const payload = {
      name: values.name.trim(),
      is_active: values.is_active,
      priority: Number(values.priority) || 0,
      refresh_interval_hours: Number(values.refresh_interval_hours) || 0,
      username: values.username.trim() || null,
    };

    // Only when the user typed one. Sending '' would clear a working password.
    if (values.password) payload.password = values.password;

    if (isM3u) {
      payload.account_type = values.account_type;
      payload.server_url = values.server_url.trim() || null;
      payload.file_path = values.file_path.trim() || null;
      payload.max_streams = Number(values.max_streams) || 0;
      payload.stale_stream_days = Number(values.stale_stream_days) || 0;
    } else {
      payload.source_type = values.source_type;
      payload.url = values.url.trim() || null;
      payload.file_path = values.file_path.trim() || null;
    }

    const api = isM3u ? m3uAccounts : epgSources;
    try {
      if (isEdit) await api.update(source.id, payload);
      else await api.create(payload);
      notifyDone(`Saved ${payload.name}`);
      onSaved();
    } catch (failure) {
      notifyError('Could not save the source', failure);
    } finally {
      setBusy(false);
    }
  };

  const title = isEdit
    ? `Edit ${source.name}`
    : isM3u
      ? 'Add M3U account'
      : 'Add EPG source';

  return (
    <Modal opened onClose={onClose} title={title} size="lg">
      <form onSubmit={form.onSubmit(submit)}>
        <Stack gap="sm">
          <Group grow align="flex-start">
            <TextInput label="Name" withAsterisk {...form.getInputProps('name')} />
            <Select
              label="Type"
              data={isM3u ? ACCOUNT_TYPES : EPG_TYPES}
              allowDeselect={false}
              {...form.getInputProps(isM3u ? 'account_type' : 'source_type')}
            />
          </Group>

          {dummy ? (
            <Alert color="gray" variant="light" p="xs" fz="xs" icon={<Info size={15} />}>
              A dummy source generates placeholder programmes for channels with no real
              guide data, so they still appear in Plex rather than vanishing. It fetches
              nothing and needs no URL.
            </Alert>
          ) : (
            <>
              <TextInput
                label={
                  isM3u && form.values.account_type === 'xtream_codes'
                    ? 'Server URL'
                    : 'URL'
                }
                description="Leave blank to read from a file instead."
                {...form.getInputProps(isM3u ? 'server_url' : 'url')}
              />
              <TextInput
                label="File path"
                description="A path on the server, used when no URL is set."
                {...form.getInputProps('file_path')}
              />

              <Group grow align="flex-start">
                <TextInput label="Username" {...form.getInputProps('username')} />
                <PasswordInput
                  label="Password"
                  placeholder={source.has_password ? 'Unchanged' : ''}
                  description={
                    source.has_password
                      ? 'Stored. Type a new one to replace it; leave blank to keep it.'
                      : 'Sent to the provider only. It is never read back.'
                  }
                  autoComplete="new-password"
                  {...form.getInputProps('password')}
                />
              </Group>

              {source.has_password && (
                <Group gap={6}>
                  <ShieldCheck size={13} color="var(--mantine-color-accent-5)" />
                  <Text size="xs" c="dimmed">
                    A password is stored for this source. It is write-only — this page
                    cannot display it.
                  </Text>
                </Group>
              )}
            </>
          )}

          <Group grow align="flex-start">
            <NumberInput
              label="Refresh every"
              description="Hours. 0 disables the schedule; manual refresh still works."
              min={0}
              {...form.getInputProps('refresh_interval_hours')}
            />
            <NumberInput
              label="Priority"
              description="Higher wins when two sources disagree."
              min={0}
              {...form.getInputProps('priority')}
            />
          </Group>

          {isM3u && (
            <>
              <NumberInput
                label="Maximum streams"
                description="Concurrent connections this provider allows. 0 means unlimited."
                min={0}
                {...form.getInputProps('max_streams')}
              />

              <div>
                <NumberInput
                  label="Delete stale streams after"
                  description="Days."
                  min={0}
                  {...form.getInputProps('stale_stream_days')}
                />
                <Alert
                  color={form.values.stale_stream_days > 0 ? 'yellow' : 'gray'}
                  variant="light"
                  p="xs"
                  fz="xs"
                  mt={6}
                  className={classes.staleNote}
                >
                  {describeStaleDays(Number(form.values.stale_stream_days))}
                </Alert>
              </div>
            </>
          )}

          <Switch
            label="Active"
            description="An inactive source is neither refreshed on its schedule nor used."
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
