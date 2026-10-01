import { useCallback, useMemo, useState } from 'react';
import { Badge, Button, Group, Stack, Text } from '@mantine/core';
import { Check, Trash2 } from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { DataTable } from '../components/DataTable.jsx';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { useResource } from '../api/useResource.js';
import { notifications as notificationsApi } from '../api/resources.js';
import { notifyError, notifyQuiet } from '../notify.js';
import classes from './Notifications.module.css';
import { absoluteTime } from '../format.js';
import { RowAction } from '../components/RowAction.jsx';

const SEVERITY_COLOURS = { error: 'red', warning: 'yellow', info: 'blue' };

const UNITS = [
  ['year', 31_536_000_000],
  ['month', 2_592_000_000],
  ['day', 86_400_000],
  ['hour', 3_600_000],
  ['minute', 60_000],
];

/**
 * "2 hours ago", for the one thing on this page where the gap matters more
 * than the instant: how long a condition has been going unanswered.
 *
 * The absolute timestamp is still there, as the cell's tooltip — a relative
 * label on a screen nobody reloads drifts, which is why every other page here
 * shows the instant instead.
 */
function relative(value) {
  const at = new Date(value);
  if (Number.isNaN(at.getTime())) return String(value);

  const elapsed = at.getTime() - Date.now();
  const format = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });
  for (const [unit, ms] of UNITS) {
    if (Math.abs(elapsed) >= ms) return format.format(Math.round(elapsed / ms), unit);
  }
  return format.format(Math.round(elapsed / 1000), 'second');
}

/**
 * What the background jobs found and could not decide.
 *
 * Every row here is a condition that would otherwise be a log line — a filter
 * that will not compile, a group whose number range is full — met weeks later
 * as channels missing from Plex with no explanation. Unacknowledged rows come
 * first from the server; acknowledging
 * one keeps it, because the condition may still be true.
 */
export function Notifications() {
  const [confirming, setConfirming] = useState(null);

  const load = useCallback(() => notificationsApi.list(), []);
  const { data, loading, error, reload } = useResource(load, []);

  const unacknowledged = data.filter((row) => !row.acknowledged_at).length;

  const acknowledge = useCallback(
    async (notification) => {
      try {
        await notificationsApi.acknowledge(notification.id);
        await reload();
      } catch (failure) {
        notifyError('Could not acknowledge', failure);
      }
    },
    [reload],
  );

  const acknowledgeAll = useCallback(async () => {
    try {
      const { acknowledged } = await notificationsApi.acknowledgeAll();
      notifyQuiet(`Acknowledged ${acknowledged}`);
      await reload();
    } catch (failure) {
      notifyError('Could not acknowledge', failure);
    }
  }, [reload]);

  const remove = useCallback(
    async (notification) => {
      try {
        await notificationsApi.remove(notification.id);
        notifyQuiet('Deleted the notification');
        await reload();
      } catch (failure) {
        notifyError('Could not delete the notification', failure);
      }
    },
    [reload],
  );

  const columns = useMemo(
    () => [
      {
        accessorKey: 'severity',
        header: 'Severity',
        size: 110,
        cell: ({ getValue }) => (
          <Badge size="xs" variant="light" color={SEVERITY_COLOURS[getValue()] ?? 'gray'}>
            {getValue()}
          </Badge>
        ),
      },
      {
        accessorKey: 'title',
        header: 'What happened',
        size: 460,
        cell: ({ row }) => (
          <Stack gap={2}>
            <Text size="sm" fw={row.original.acknowledged_at ? 400 : 600}>
              {row.original.title}
            </Text>
            <Text size="xs" c="dimmed" className={classes.message}>
              {row.original.message}
            </Text>
          </Stack>
        ),
      },
      {
        id: 'occurrences',
        header: 'Seen',
        size: 200,
        // A rendered summary of three fields rather than one value, so there
        // is nothing here to sort or filter by.
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ row }) => {
          const {
            occurrences,
            updated_at: updated,
            acknowledged_at: seen,
          } = row.original;
          return (
            <Stack gap={2}>
              <Text size="xs" title={absoluteTime(updated)}>
                {occurrences === 1
                  ? `once, ${relative(updated)}`
                  : `${occurrences} times, last ${relative(updated)}`}
              </Text>
              {seen ? (
                <Text size="xs" c="dimmed" title={absoluteTime(seen)}>
                  acknowledged {relative(seen)}
                </Text>
              ) : (
                <Text size="xs" c="yellow">
                  needs attention
                </Text>
              )}
            </Stack>
          );
        },
      },
    ],
    [],
  );

  const rowActions = useCallback(
    (notification) => (
      <Group gap={2} justify="flex-end" wrap="nowrap">
        {!notification.acknowledged_at && (
          <RowAction
            label={`Acknowledge ${notification.title}`}
            tooltip="Acknowledge"
            onClick={() => acknowledge(notification)}
          >
            <Check size={14} />
          </RowAction>
        )}
        <RowAction
          label={`Delete ${notification.title}`}
          tooltip="Delete"
          color="red"
          onClick={() => setConfirming(notification)}
        >
          <Trash2 size={14} />
        </RowAction>
      </Group>
    ),
    [acknowledge],
  );

  const getRowId = useCallback((notification) => String(notification.id), []);

  return (
    <Page
      title="Notifications"
      subtitle="What the background jobs found and could not decide"
      actions={
        <Button
          variant="default"
          leftSection={<Check size={14} />}
          disabled={unacknowledged === 0}
          onClick={acknowledgeAll}
        >
          Acknowledge all
        </Button>
      }
    >
      <DataTable
        label="Notifications"
        data={data}
        columns={columns}
        getRowId={getRowId}
        loading={loading}
        error={error}
        rowActions={rowActions}
        emptyMessage={
          error ? 'Notifications could not be loaded.' : 'Nothing needs your attention.'
        }
      />

      {confirming && (
        <ConfirmModal
          title="Delete notification"
          // Deleting is not the same as acknowledging, and the difference is
          // the whole reason both buttons exist.
          message={`Delete "${confirming.title}"? If the condition is still there, the next refresh raises it again.`}
          onConfirm={() => remove(confirming)}
          onClose={() => setConfirming(null)}
        />
      )}
    </Page>
  );
}
