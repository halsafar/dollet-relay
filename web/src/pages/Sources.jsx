import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  ActionIcon,
  Alert,
  Anchor,
  Badge,
  Button,
  Group,
  Progress,
  Switch,
  Text,
  Tooltip,
} from '@mantine/core';
import { Link } from 'react-router-dom';
import { notifyDone, notifyError, notifyQuiet } from '../notify.js';
import {
  HelpCircle,
  Link2,
  Pencil,
  Plus,
  RefreshCw,
  SlidersHorizontal,
  Trash2,
  X,
} from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { DataTable } from '../components/DataTable.jsx';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { useResource } from '../api/useResource.js';
import {
  epgSources as epgApi,
  jobs as jobsApi,
  m3uAccounts as m3uApi,
} from '../api/resources.js';
import { SourceModal } from './SourceModal.jsx';
import { AccountDetail } from './AccountDetail.jsx';
import classes from './Sources.module.css';
import { absoluteTime } from '../format.js';
import { RowAction } from '../components/RowAction.jsx';

/** How often to re-read source rows while any refresh is running. */
const POLL_MS = 2000;

const STATUS_COLORS = { success: 'accent', error: 'red', fetching: 'blue', idle: 'gray' };

/** How many ambiguous matches to name before falling back to a count. */
const NAMED_DECISIONS = 5;

export function Sources() {
  const [editing, setEditing] = useState(null);
  const [detail, setDetail] = useState(null);
  const [confirming, setConfirming] = useState(null);
  const [matching, setMatching] = useState(false);

  const loadAccounts = useCallback(() => m3uApi.list(), []);
  const {
    data: accounts,
    loading: accountsLoading,
    error: accountsError,
    reload: reloadAccounts,
  } = useResource(loadAccounts, []);

  const loadSources = useCallback(() => epgApi.list(), []);
  const {
    data: sources,
    loading: sourcesLoading,
    error: sourcesError,
    reload: reloadSources,
  } = useResource(loadSources, []);

  const loadJobs = useCallback(() => jobsApi.list(), []);
  const { data: jobs, reload: reloadJobs } = useResource(loadJobs, []);

  const loadAmbiguous = useCallback(() => epgApi.ambiguous(), []);
  const { data: ambiguous, reload: reloadAmbiguous } = useResource(loadAmbiguous, []);

  const busy =
    accounts.some((account) => account.status === 'running') ||
    sources.some((source) => source.status === 'running');

  // Only while something is running. Polling an idle server every two seconds
  // to watch nothing happen is a load source with no reader.
  useEffect(() => {
    if (!busy) return undefined;
    const timer = setInterval(() => {
      void reloadAccounts();
      void reloadSources();
      void reloadJobs();
      void reloadAmbiguous();
    }, POLL_MS);
    return () => clearInterval(timer);
  }, [busy, reloadAccounts, reloadSources, reloadJobs, reloadAmbiguous]);

  // Jobs carry the owning id in their payload, so this needs no key parsing.
  const jobFor = useCallback(
    (field, id) => jobs.find((job) => job.running && job.payload?.[field] === id) ?? null,
    [jobs],
  );

  const refresh = useCallback(
    async (api, id, name) => {
      try {
        await api.refresh(id);
        notifyDone(`Refreshing ${name}…`);
        await Promise.all([reloadAccounts(), reloadSources(), reloadJobs()]);
      } catch (failure) {
        notifyError('Could not start the refresh', failure);
      }
    },
    [reloadAccounts, reloadSources, reloadJobs],
  );

  const cancel = useCallback(
    async (job) => {
      try {
        const { cancelling } = await jobsApi.cancel(job.key);
        // Cooperative: the handler notices at its next checkpoint.
        notifyQuiet(
          cancelling
            ? 'Cancelling at the next checkpoint…'
            : 'That job is no longer running.',
        );
        await reloadJobs();
      } catch (failure) {
        notifyError('Could not cancel', failure);
      }
    },
    [reloadJobs],
  );

  const remove = useCallback(async (api, id, name, reload) => {
    try {
      await api.remove(id);
      notifyQuiet(`Deleted ${name}`);
      await reload();
    } catch (failure) {
      notifyError('Could not delete the source', failure);
    }
  }, []);

  /**
   * Fill in the guide for channels that have none.
   *
   * The counts come back rather than a "done": `matched` is what was assigned,
   * and `need_a_decision` is what landed in the band the scorer refuses to
   * call — which is exactly the list the alert above this button reads, so it
   * is reloaded here and the new questions appear at once.
   */
  const matchUnmapped = useCallback(async () => {
    setMatching(true);
    try {
      const { matched, need_a_decision: decisions } = await epgApi.match();
      notifyDone(
        `Matched ${matched} ${matched === 1 ? 'channel' : 'channels'}` +
          (decisions > 0
            ? `, ${decisions} ${decisions === 1 ? 'needs' : 'need'} a decision`
            : ''),
      );
      await reloadAmbiguous();
    } catch (failure) {
      notifyError('Could not match channels', failure);
    } finally {
      setMatching(false);
    }
  }, [reloadAmbiguous]);

  const setActive = useCallback(async (api, row, isActive, reload) => {
    try {
      await api.update(row.id, { is_active: isActive });
      await reload();
    } catch (failure) {
      notifyError('Could not change the source', failure);
    }
  }, []);

  const statusColumn = useMemo(
    () => ({
      id: 'status',
      accessorFn: (row) => row.status,
      header: 'Status',
      size: 110,
      cell: ({ row }) => (
        <Badge
          size="xs"
          variant="light"
          color={STATUS_COLORS[row.original.status] ?? 'gray'}
        >
          {row.original.status}
        </Badge>
      ),
    }),
    [],
  );

  const messageColumn = useMemo(
    () => ({
      id: 'last_message',
      accessorFn: (row) => row.last_message ?? '',
      header: 'Last result',
      size: 380,
      cell: ({ row }) => {
        const { status, progress, last_message: message } = row.original;
        if (status === 'running') {
          return (
            <Group gap={8} wrap="nowrap">
              <Progress value={progress * 100} size="sm" w={90} color="accent" />
              <Text size="xs" c="dimmed" truncate>
                {message ?? 'Working…'}
              </Text>
            </Group>
          );
        }
        return (
          // Verbatim from the server: "61 streams: 0 new, 2 updated, 59
          // unchanged, 0 stale, 0 removed" says more than any summary here.
          <Text
            size="xs"
            c={status === 'failed' ? 'red.4' : 'dimmed'}
            title={message ?? ''}
            truncate
          >
            {message ?? '—'}
          </Text>
        );
      },
    }),
    [],
  );

  const accountColumns = useMemo(
    () => [
      { accessorKey: 'name', header: 'Name', size: 160 },
      {
        id: 'account_type',
        accessorFn: (row) => (row.account_type === 'xtream_codes' ? 'Xtream' : 'M3U'),
        header: 'Type',
        size: 90,
      },
      {
        id: 'location',
        // The username and password are deliberately absent: this table is the
        // most screenshot-able surface in the application.
        accessorFn: (row) => row.server_url ?? row.file_path ?? '',
        header: 'URL / File',
        size: 260,
        cell: ({ getValue }) => (
          <Text size="xs" c="dimmed" title={getValue()} truncate>
            {getValue() || '—'}
          </Text>
        ),
      },
      statusColumn,
      messageColumn,
      {
        id: 'updated_at',
        accessorFn: (row) => row.updated_at ?? '',
        header: 'Refreshed',
        size: 150,
        cell: ({ row }) => (
          <Text size="xs" c="dimmed">
            {absoluteTime(row.original.updated_at, 'Never')}
          </Text>
        ),
      },
      {
        id: 'is_active',
        header: 'Active',
        size: 70,
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ row }) => (
          <Switch
            size="xs"
            checked={row.original.is_active}
            aria-label={`Activate ${row.original.name}`}
            onChange={(event) =>
              setActive(m3uApi, row.original, event.currentTarget.checked, reloadAccounts)
            }
          />
        ),
      },
    ],
    [statusColumn, messageColumn, setActive, reloadAccounts],
  );

  const sourceColumns = useMemo(
    () => [
      { accessorKey: 'name', header: 'Name', size: 160 },
      { accessorKey: 'source_type', header: 'Type', size: 90 },
      {
        id: 'location',
        accessorFn: (row) => row.url ?? row.file_path ?? '',
        header: 'URL / File',
        size: 260,
        cell: ({ getValue }) => (
          <Text size="xs" c="dimmed" title={getValue()} truncate>
            {getValue() || '—'}
          </Text>
        ),
      },
      statusColumn,
      messageColumn,
      {
        id: 'updated_at',
        accessorFn: (row) => row.updated_at ?? '',
        header: 'Refreshed',
        size: 150,
        cell: ({ row }) => (
          <Text size="xs" c="dimmed">
            {absoluteTime(row.original.updated_at, 'Never')}
          </Text>
        ),
      },
      {
        id: 'is_active',
        header: 'Active',
        size: 70,
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ row }) => (
          <Switch
            size="xs"
            checked={row.original.is_active}
            aria-label={`Activate ${row.original.name}`}
            onChange={(event) =>
              setActive(epgApi, row.original, event.currentTarget.checked, reloadSources)
            }
          />
        ),
      },
    ],
    [statusColumn, messageColumn, setActive, reloadSources],
  );

  const rowActions = useCallback(
    (row, kind) => {
      const api = kind === 'm3u' ? m3uApi : epgApi;
      const reload = kind === 'm3u' ? reloadAccounts : reloadSources;
      const job = jobFor(kind === 'm3u' ? 'm3u_account_id' : 'epg_source_id', row.id);

      return (
        <Group gap={2} justify="flex-end" wrap="nowrap">
          {job ? (
            <RowAction
              label={`Cancel refresh of ${row.name}`}
              tooltip="Cancel this refresh"
              color="red"
              onClick={() => cancel(job)}
            >
              <X size={14} />
            </RowAction>
          ) : (
            <RowAction
              label={`Refresh ${row.name}`}
              tooltip="Refresh now"
              onClick={() => refresh(api, row.id, row.name)}
            >
              <RefreshCw size={14} />
            </RowAction>
          )}
          {kind === 'm3u' && (
            <RowAction
              label={`Import rules for ${row.name}`}
              tooltip="Groups, profiles and filters"
              color="gray"
              onClick={() => setDetail(row)}
            >
              <SlidersHorizontal size={14} />
            </RowAction>
          )}
          <RowAction
            label={`Edit ${row.name}`}
            tooltip="Edit"
            onClick={() => setEditing({ kind, source: row })}
          >
            <Pencil size={14} />
          </RowAction>
          <Tooltip label={row.locked ? 'This source is built in' : 'Delete'}>
            <ActionIcon
              variant="subtle"
              color="red"
              size="sm"
              disabled={Boolean(row.locked)}
              aria-label={`Delete ${row.name}`}
              onClick={() =>
                setConfirming({
                  title: kind === 'm3u' ? 'Delete M3U account' : 'Delete EPG source',
                  message:
                    kind === 'm3u'
                      ? `Delete ${row.name}? Its streams go with it, and any channel using one loses that failover entry.`
                      : `Delete ${row.name}? Channels matched to it lose their guide data.`,
                  onConfirm: () => remove(api, row.id, row.name, reload),
                })
              }
            >
              <Trash2 size={14} />
            </ActionIcon>
          </Tooltip>
        </Group>
      );
    },
    [jobFor, cancel, refresh, remove, reloadAccounts, reloadSources],
  );

  const accountActions = useCallback((row) => rowActions(row, 'm3u'), [rowActions]);
  const sourceActions = useCallback((row) => rowActions(row, 'epg'), [rowActions]);
  const getRowId = useCallback((row) => String(row.id), []);

  const needsDecision = ambiguous.length;

  return (
    <Page title="Sources" subtitle="Where channels and guide data come from">
      {needsDecision > 0 && (
        <Alert
          color="yellow"
          variant="light"
          mb="sm"
          icon={<HelpCircle size={16} />}
          title={`${needsDecision} ${needsDecision === 1 ? 'channel needs' : 'channels need'} a guide decision`}
        >
          The matcher found a likely guide for{' '}
          {needsDecision === 1 ? 'this channel' : 'these channels'} but not a confident
          one, so {needsDecision === 1 ? 'it is' : 'they are'} left unmatched for you to
          decide rather than guessed.
          <ul className={classes.decisions}>
            {ambiguous.slice(0, NAMED_DECISIONS).map((match) => (
              <li key={match.channel_id}>
                <Text span size="xs" fw={500}>
                  {match.channel_name}
                </Text>
                <Text span size="xs" c="dimmed">
                  {' → '}
                  {match.candidate_name} ({Math.round(match.score)}% match)
                </Text>
              </li>
            ))}
          </ul>
          {needsDecision > NAMED_DECISIONS && (
            <Text size="xs" c="dimmed" mb={6}>
              and {needsDecision - NAMED_DECISIONS} more.
            </Text>
          )}
          <Anchor component={Link} to="/guide" size="xs">
            Accept or dismiss each one on the TV Guide
          </Anchor>
        </Alert>
      )}

      {/* Beside the alert above, because that is where an operator already
          reads what the matcher does and what it refuses to do. */}
      <Alert
        color="gray"
        variant="light"
        mb="sm"
        icon={<Link2 size={16} />}
        title="Match channels to guide data"
      >
        <Text size="xs" mb={8}>
          Scores every channel that has <strong>no guide</strong> against the guide
          channels your sources published, and maps the confident ones. A channel that
          already has a guide is never touched, however it got one. A candidate the scorer
          is unsure of becomes a decision to make on the TV Guide, not an assignment.
        </Text>
        <Button size="xs" variant="default" loading={matching} onClick={matchUnmapped}>
          Match unmapped channels
        </Button>
      </Alert>

      <section className={classes.section}>
        <div className={classes.sectionHeader}>
          <h2 className={classes.sectionTitle}>M3U accounts</h2>
          <Button
            size="xs"
            variant="default"
            leftSection={<RefreshCw size={13} />}
            onClick={async () => {
              await m3uApi.refreshAll();
              await Promise.all([reloadAccounts(), reloadJobs()]);
            }}
          >
            Refresh all
          </Button>
          <Button
            size="xs"
            leftSection={<Plus size={13} />}
            onClick={() => setEditing({ kind: 'm3u', source: {} })}
          >
            Add M3U
          </Button>
        </div>

        <DataTable
          label="M3U accounts"
          data={accounts}
          columns={accountColumns}
          getRowId={getRowId}
          loading={accountsLoading}
          error={accountsError}
          rowActions={accountActions}
          pageSize={25}
          emptyMessage={
            accountsError
              ? 'Accounts could not be loaded.'
              : 'No providers yet. Add an M3U playlist or an Xtream Codes account to import channels.'
          }
        />
      </section>

      <section className={classes.section}>
        <div className={classes.sectionHeader}>
          <h2 className={classes.sectionTitle}>Guide sources</h2>
          <Button
            size="xs"
            variant="default"
            leftSection={<RefreshCw size={13} />}
            onClick={async () => {
              await epgApi.refreshAll();
              await Promise.all([reloadSources(), reloadJobs()]);
            }}
          >
            Refresh all
          </Button>
          <Button
            size="xs"
            leftSection={<Plus size={13} />}
            onClick={() => setEditing({ kind: 'epg', source: {} })}
          >
            Add EPG
          </Button>
        </div>

        <DataTable
          label="Guide sources"
          data={sources}
          columns={sourceColumns}
          getRowId={getRowId}
          loading={sourcesLoading}
          error={sourcesError}
          rowActions={sourceActions}
          pageSize={25}
          emptyMessage={
            sourcesError
              ? 'Sources could not be loaded.'
              : 'No guide sources yet. Without one, channels appear in Plex with no programme information.'
          }
        />
      </section>

      {confirming && (
        <ConfirmModal
          title={confirming.title}
          message={confirming.message}
          onConfirm={confirming.onConfirm}
          onClose={() => setConfirming(null)}
        />
      )}

      {editing && (
        <SourceModal
          kind={editing.kind}
          source={editing.source}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void (editing.kind === 'm3u' ? reloadAccounts() : reloadSources());
          }}
        />
      )}

      {detail && <AccountDetail account={detail} onClose={() => setDetail(null)} />}
    </Page>
  );
}
