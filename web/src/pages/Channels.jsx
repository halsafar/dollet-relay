import { useCallback, useMemo, useState } from 'react';
import {
  ActionIcon,
  Button,
  Group,
  Menu,
  Select,
  Switch,
  Text,
  Tooltip,
  UnstyledButton,
} from '@mantine/core';
import { useClipboard } from '@mantine/hooks';
import { notifyDone, notifyError, notifyQuiet } from '../notify.js';
import {
  CalendarDays,
  EllipsisVertical,
  ListVideo,
  MonitorPlay,
  Pencil,
  Plus,
  Radio,
  Trash2,
} from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { DataTable } from '../components/DataTable.jsx';
import { useResource } from '../api/useResource.js';
import {
  channelGroups as groupsApi,
  channelProfiles as profilesApi,
  channels as channelsApi,
  logos as logosApi,
  streams as streamsApi,
  streamProfiles as streamProfilesApi,
} from '../api/resources.js';
import { ChannelModal } from './ChannelModal.jsx';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { LogoImage } from '../components/LogoImage.jsx';
import { compareChannelNumber } from './channelNumber.js';
import { guideUrl, hdhrUrl, playlistUrl } from './connectUrls.js';
import { useBaseUrl } from '../components/baseUrl.js';
import classes from './Channels.module.css';
import { RowAction } from '../components/RowAction.jsx';

/**
 * Documented gaps on this screen, so none of them is a silent omission:
 *
 * - **Channel-profile membership.** Per profile rather than per channel, so it
 *   lives in the bulk menu where the profile can be named.
 * - **Preview.** No player is embedded. The channel editor copies the stream
 *   URL instead, which is what a VLC window is for.
 *
 * Dragging a channel writes its number and nothing else's. Plex sorts by
 * number, so a reorder *is* a renumber — but the step grid leaves gaps between
 * numbers, and a drop between two channels takes a number from the gap. That
 * is one re-scanned channel instead of a renumbered group, which is why this
 * refuses when a pair has no room left rather than pushing everything below
 * the drop down one. Laying a whole group back out on the grid is the Groups
 * page's Renumber.
 */

const channelRowLabel = (channel) => channel.effective_name;

/**
 * The three output URLs, copied from the header.
 *
 * Built from the Connect page's base picker rather than from `window.location`,
 * so the address these hand out is the one that screen says clients use — and
 * changing it in one place changes it in both.
 */
const OUTPUT_LINKS = [
  { label: 'HDHR', icon: MonitorPlay, url: (base) => hdhrUrl(base) },
  { label: 'M3U', icon: ListVideo, url: (base) => playlistUrl(base) },
  { label: 'EPG', icon: CalendarDays, url: (base) => guideUrl(base) },
];

/**
 * The EPG filter's value for "mapped to nothing".
 *
 * A Select carries strings, so the option needs a value no guide channel can
 * have. XMLTV is XML and a NUL is not representable in it, so a name can never
 * collide with this one.
 */
const NO_GUIDE = '\u0000';

export function Channels() {
  const base = useBaseUrl();
  const [editing, setEditing] = useState(null);
  const [confirming, setConfirming] = useState(null);
  const [selectedChannels, setSelectedChannels] = useState([]);
  const [selectedStreams, setSelectedStreams] = useState([]);
  const [groupFilter, setGroupFilter] = useState(null);
  const [epgFilter, setEpgFilter] = useState(null);

  const loadChannels = useCallback(() => channelsApi.list(), []);
  const {
    data: channels,
    loading: channelsLoading,
    error: channelsError,
    reload: reloadChannels,
    setData: setChannels,
  } = useResource(loadChannels, []);

  const loadGroups = useCallback(() => groupsApi.list(), []);
  const { data: groups } = useResource(loadGroups, []);

  const loadStreamProfiles = useCallback(() => streamProfilesApi.list(), []);
  const { data: streamProfiles } = useResource(loadStreamProfiles, []);

  const loadLogos = useCallback(() => logosApi.all(), []);
  const { data: logos } = useResource(loadLogos, []);

  const loadProfiles = useCallback(() => profilesApi.list(), []);
  const { data: profiles } = useResource(loadProfiles, []);

  // Both selects narrow the already-loaded lineup, because the channel list is
  // fetched whole and unpaginated.
  const visibleChannels = useMemo(
    () =>
      channels.filter((channel) => {
        // Compared against the effective group name: that is what the Group
        // column shows and what the server groups by. Matching on the base
        // `channel_group_id` would return nothing for any overridden channel.
        if (groupFilter && (channel.group_name ?? '') !== groupFilter) return false;
        if (epgFilter === NO_GUIDE) return channel.effective_epg_data_id == null;
        if (epgFilter && (channel.epg_name ?? '') !== epgFilter) return false;
        return true;
      }),
    [channels, groupFilter, epgFilter],
  );

  // The guides actually in use, from the loaded rows: an endpoint would list
  // every guide channel the provider publishes, and thousands of options for a
  // lineup of fifty is not a filter.
  //
  // "No guide" is the question this filter exists for — which channels Plex
  // will show an empty strip for — and it cannot be asked of a list of names.
  const epgOptions = useMemo(() => {
    const named = [...new Set(channels.map((c) => c.epg_name).filter(Boolean))]
      .sort()
      .map((value) => ({ value, label: value }));
    return [{ value: NO_GUIDE, label: 'No guide' }, ...named];
  }, [channels]);

  const groupOptions = useMemo(
    () =>
      groups.map((group) => ({
        value: group.name,
        label: `${group.name} (${group.channel_count})`,
      })),
    [groups],
  );

  /**
   * The drag itself, which only rearranges what is on screen. Numbers are the
   * real order, so nothing is decided until the mouse comes up.
   */
  const reorderChannels = useCallback(
    (ids) => {
      const byId = new Map(channels.map((channel) => [String(channel.id), channel]));
      const moved = new Set(ids);
      let next = 0;
      // Only the places the visible rows hold are re-sequenced: a group filter
      // must not drag the channels it hides along with them.
      setChannels((current) =>
        current.map((channel) =>
          moved.has(String(channel.id)) ? byId.get(ids[next++]) : channel,
        ),
      );
    },
    [channels, setChannels],
  );

  /**
   * The drop. The server is given the rows either side of it and answers with
   * the number it found between them, or refuses because there is none left.
   *
   * Either way the lineup is re-read rather than patched: what the table was
   * showing mid-drag is a guess at an order that numbers decide, and the
   * server has just decided it.
   */
  const commitMove = useCallback(
    async (id, ids) => {
      const at = ids.indexOf(id);
      try {
        await channelsApi.move(Number(id), {
          after: at > 0 ? Number(ids[at - 1]) : null,
          before: at < ids.length - 1 ? Number(ids[at + 1]) : null,
        });
      } catch (failure) {
        notifyError('Could not move the channel', failure);
      }
      await reloadChannels();
    },
    [reloadChannels],
  );

  const removeChannel = useCallback(
    async (channel) => {
      try {
        await channelsApi.remove(channel.id);
        notifyQuiet(`Deleted ${channel.effective_name}`);
        await reloadChannels();
      } catch (failure) {
        notifyError('Could not delete the channel', failure);
      }
    },
    [reloadChannels],
  );

  const bulkDeleteChannels = useCallback(async () => {
    try {
      const { deleted } = await channelsApi.bulkDelete(selectedChannels.map(Number));
      notifyQuiet(`Deleted ${deleted} channels`);
      await reloadChannels();
    } catch (failure) {
      notifyError('Could not delete the channels', failure);
    }
  }, [selectedChannels, reloadChannels]);

  const setProfileMembership = useCallback(
    async (profile, enabled) => {
      try {
        const { updated } = await profilesApi.setMembership(
          profile.id,
          selectedChannels.map(Number),
          enabled,
        );
        notifyDone(
          `${enabled ? 'Enabled' : 'Disabled'} ${updated} channels in ${profile.name}`,
        );
      } catch (failure) {
        notifyError('Could not update the profile', failure);
      }
    },
    [selectedChannels],
  );

  /**
   * Whether a channel reaches HDHR, `/output/m3u` and `/output/epg` — which is
   * to say, whether Plex sees it at all. Patched per row rather than through
   * the editor because curating a lineup means toggling many of them, and the
   * only alternative the user had was deleting the channel.
   */
  const setHidden = useCallback(
    async (channel, hidden) => {
      try {
        const updated = await channelsApi.update(channel.id, {
          hidden_from_output: hidden,
        });
        // Patch the one row rather than refetching the lineup: a full reload
        // per toggle makes the table flash on every click.
        setChannels((current) =>
          current.map((row) => (row.id === channel.id ? { ...row, ...updated } : row)),
        );
      } catch (failure) {
        notifyError('Could not change visibility', failure);
      }
    },
    [setChannels],
  );

  const columns = useMemo(
    () => [
      {
        id: 'visible',
        accessorFn: (row) => !row.hidden_from_output,
        header: '',
        size: 46,
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ row }) => (
          <Tooltip
            label={
              row.original.hidden_from_output
                ? 'Hidden from HDHR, M3U and EPG output'
                : 'Included in HDHR, M3U and EPG output'
            }
          >
            <Switch
              size="xs"
              checked={!row.original.hidden_from_output}
              aria-label={`Include ${row.original.effective_name} in outputs`}
              onChange={(event) => setHidden(row.original, !event.currentTarget.checked)}
            />
          </Tooltip>
        ),
      },
      {
        id: 'channel_number',
        accessorFn: (row) => row.effective_channel_number,
        header: '#',
        size: 70,
        // `basic` coerces null to 0, which ties with a real channel 0 and makes
        // the comparator inconsistent. This matches the server exactly: numeric,
        // unnumbered last on ascending — an unnumbered channel leading the HDHR
        // lineup is the first thing Plex would show.
        sortingFn: compareChannelNumber,
        cell: ({ getValue }) => {
          const number = getValue();
          return number === null || number === undefined ? (
            <span className={classes.unnumbered}>—</span>
          ) : (
            <span className={classes.numberCell}>{number}</span>
          );
        },
      },
      {
        id: 'name',
        accessorFn: (row) => row.effective_name,
        header: 'Name',
        size: 260,
      },
      {
        id: 'epg',
        // The guide channel this one is mapped to, which is what decides
        // whether it has listings. `tvg_id` is a provider label that most
        // mapped channels do not carry — it stays in the editor, where it
        // still feeds the outputs.
        accessorFn: (row) => row.epg_name ?? '',
        header: 'EPG',
        size: 150,
        cell: ({ getValue }) =>
          getValue() || <span className={classes.unnumbered}>—</span>,
      },
      {
        id: 'group_name',
        accessorFn: (row) => row.group_name ?? '',
        header: 'Group',
        size: 170,
        cell: ({ getValue }) =>
          getValue() || <span className={classes.unnumbered}>—</span>,
      },
      {
        id: 'logo',
        accessorFn: (row) => row.logo_url ?? '',
        header: '',
        size: 70,
        enableSorting: false,
        enableColumnFilter: false,
        cell: ({ row }) => <LogoImage src={row.original.logo_url} alt="" size={20} />,
      },
    ],
    [setHidden],
  );

  const channelActions = useCallback(
    (channel) => (
      <Group gap={2} justify="flex-end" wrap="nowrap">
        <RowAction
          label={`Edit ${channel.effective_name}`}
          tooltip="Edit"
          onClick={() => setEditing(channel)}
        >
          <Pencil size={14} />
        </RowAction>
        <Tooltip label="Delete">
          <ActionIcon
            variant="subtle"
            color="red"
            size="sm"
            aria-label={`Delete ${channel.effective_name}`}
            onClick={() =>
              setConfirming({
                title: 'Delete channel',
                message: `Delete ${channel.effective_name}? Its failover list goes with it, and there is no undo.`,
                onConfirm: () => removeChannel(channel),
              })
            }
          >
            <Trash2 size={14} />
          </ActionIcon>
        </Tooltip>
      </Group>
    ),
    [removeChannel],
  );

  const getChannelId = useCallback((channel) => String(channel.id), []);

  return (
    <Page
      title="Channels"
      actions={
        <div className={classes.links} role="group" aria-label="Output links">
          <span className={classes.linksLabel}>Copy:</span>
          {OUTPUT_LINKS.map(({ label, icon: Icon, url }) => (
            <CopyUrlButton key={label} label={label} icon={Icon} url={url(base)} />
          ))}
        </div>
      }
    >
      <div className={classes.split}>
        <section className={`${classes.pane} ${classes.channelsPane}`}>
          <h2 className={classes.paneTitle}>
            Channels
            <Text component="span" size="xs" c="dimmed" fw={400}>
              {visibleChannels.length} of {channels.length}
            </Text>
          </h2>

          <DataTable
            label="Channels"
            data={visibleChannels}
            columns={columns}
            getRowId={getChannelId}
            rowLabel={channelRowLabel}
            onReorder={reorderChannels}
            onReorderEnd={commitMove}
            loading={channelsLoading}
            error={channelsError}
            enableSelection
            onSelectionChange={setSelectedChannels}
            rowActions={channelActions}
            emptyMessage={
              channelsError ? 'Channels could not be loaded.' : 'No channels yet.'
            }
            toolbar={
              <Group gap={6} wrap="nowrap">
                <Select
                  size="xs"
                  data={groupOptions}
                  value={groupFilter}
                  onChange={setGroupFilter}
                  placeholder="All groups"
                  aria-label="Filter by group"
                  clearable
                  searchable
                  w={170}
                  comboboxProps={{ withinPortal: true }}
                />
                <Select
                  size="xs"
                  data={epgOptions}
                  value={epgFilter}
                  onChange={setEpgFilter}
                  placeholder="All guides"
                  aria-label="Filter by guide"
                  clearable
                  searchable
                  w={150}
                  comboboxProps={{ withinPortal: true }}
                />
                {selectedChannels.length > 0 && (
                  <>
                    <Text size="xs" c="dimmed">
                      {selectedChannels.length} selected
                    </Text>
                    <Menu withinPortal position="bottom-end">
                      <Menu.Target>
                        <UnstyledButton
                          aria-label="Bulk actions"
                          style={{ display: 'flex', padding: 4 }}
                        >
                          <EllipsisVertical size={15} />
                        </UnstyledButton>
                      </Menu.Target>
                      <Menu.Dropdown>
                        <Menu.Label>Channel profiles</Menu.Label>
                        {profiles.length === 0 && (
                          <Menu.Item disabled>No profiles</Menu.Item>
                        )}
                        {profiles.flatMap((profile) => [
                          <Menu.Item
                            key={`on-${profile.id}`}
                            onClick={() => setProfileMembership(profile, true)}
                          >
                            Enable in {profile.name}
                          </Menu.Item>,
                          <Menu.Item
                            key={`off-${profile.id}`}
                            onClick={() => setProfileMembership(profile, false)}
                          >
                            Disable in {profile.name}
                          </Menu.Item>,
                        ])}
                        <Menu.Divider />
                        <Menu.Item
                          color="red"
                          leftSection={<Trash2 size={13} />}
                          onClick={() =>
                            setConfirming({
                              title: 'Delete channels',
                              message: `Delete ${selectedChannels.length} channels and their failover lists? There is no undo.`,
                              onConfirm: bulkDeleteChannels,
                            })
                          }
                        >
                          Delete {selectedChannels.length} channels
                        </Menu.Item>
                      </Menu.Dropdown>
                    </Menu>
                  </>
                )}
                <Button
                  size="xs"
                  leftSection={<Plus size={13} />}
                  onClick={() => setEditing({})}
                >
                  Add
                </Button>
              </Group>
            }
          />
        </section>

        <StreamsPane
          groups={groups}
          channels={channels}
          selected={selectedStreams}
          onSelectionChange={setSelectedStreams}
          onChanged={reloadChannels}
        />
      </div>

      {confirming && (
        <ConfirmModal
          title={confirming.title}
          message={confirming.message}
          onConfirm={confirming.onConfirm}
          onClose={() => setConfirming(null)}
        />
      )}

      {editing && (
        <ChannelModal
          channel={editing}
          groups={groups}
          streamProfiles={streamProfiles}
          logos={logos}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void reloadChannels();
          }}
        />
      )}
    </Page>
  );
}

/** One compact copy button, with the URL it copies as its tooltip. */
function CopyUrlButton({ label, icon: Icon, url }) {
  const clipboard = useClipboard({ timeout: 1200 });
  return (
    <Tooltip label={clipboard.copied ? 'Copied' : url}>
      <Button
        size="xs"
        variant="default"
        leftSection={<Icon size={13} />}
        aria-label={`Copy ${label} URL`}
        onClick={() => clipboard.copy(url)}
      >
        {label}
      </Button>
    </Tooltip>
  );
}

/**
 * Provider streams, server-paginated.
 *
 * Separate state from the channel list on purpose: this endpoint always
 * paginates and can hold tens of thousands of rows, so it cannot share the
 * channel table's load-everything-and-filter-locally approach.
 */
function StreamsPane({ groups, channels, selected, onSelectionChange, onChanged }) {
  // Null until the table reports; see the note in Logos.jsx. A second object
  // holding the same values still fetches the same page twice.
  const [query, setQuery] = useState(null);
  const [groupFilter, setGroupFilter] = useState(null);
  const [confirming, setConfirming] = useState(null);
  // One add at a time: appending is a read-modify-write of the failover list,
  // and two in flight would each append to the same stale copy.
  const [adding, setAdding] = useState(false);

  const load = useCallback(
    () =>
      query
        ? streamsApi.list({
            page: query.pageIndex + 1,
            pageSize: query.pageSize,
            search: query.search || undefined,
            channelGroup: groupFilter ? Number(groupFilter) : undefined,
            ordering: query.ordering,
          })
        : Promise.resolve({ results: [], count: 0 }),
    [query, groupFilter],
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

  const groupOptions = useMemo(
    () =>
      groups.map((group) => ({
        value: String(group.id),
        label: `${group.name} (${group.stream_count})`,
      })),
    [groups],
  );

  const addToChannel = useCallback(
    async (stream, channel) => {
      if (adding) return;
      setAdding(true);
      try {
        const current = await channelsApi.streams(channel.id);
        if (current.some((existing) => existing.id === stream.id)) {
          notifyQuiet(`${stream.name} is already on ${channel.effective_name}`);
          return;
        }
        // Appended, not prepended: a new stream is a fallback behind whatever
        // is already working, never a silent replacement for it.
        await channelsApi.setStreams(channel.id, [
          ...current.map((existing) => existing.id),
          stream.id,
        ]);
        notifyDone(`Added ${stream.name} to ${channel.effective_name}`);
        await onChanged();
      } catch (failure) {
        notifyError('Could not add stream', failure);
      } finally {
        setAdding(false);
      }
    },
    [adding, onChanged],
  );

  const createChannelFrom = useCallback(
    async (stream) => {
      try {
        await channelsApi.create({
          name: stream.name,
          channel_group_id: stream.channel_group_id ?? null,
          tvg_id: stream.tvg_id ?? null,
          streams: [stream.id],
        });
        notifyDone(`Created channel ${stream.name}`);
        await onChanged();
      } catch (failure) {
        notifyError('Could not create channel', failure);
      }
    },
    [onChanged],
  );

  const bulkDelete = useCallback(async () => {
    try {
      const { deleted } = await streamsApi.bulkDelete(selected.map(Number));
      notifyQuiet(`Deleted ${deleted} streams`);
      await reload();
    } catch (failure) {
      notifyError('Could not delete the streams', failure);
    }
  }, [selected, reload]);

  const columns = useMemo(
    () => [
      { accessorKey: 'name', header: 'Name', size: 220 },
      {
        id: 'group_name',
        accessorFn: (row) =>
          groups.find((group) => group.id === row.channel_group_id)?.name ?? '',
        header: 'Group',
        size: 150,
        cell: ({ getValue }) =>
          getValue() || <span className={classes.unnumbered}>—</span>,
      },
    ],
    [groups],
  );

  const rowActions = useCallback(
    (stream) => (
      <Group gap={2} justify="flex-end" wrap="nowrap">
        <Menu withinPortal position="bottom-end">
          <Menu.Target>
            <Tooltip label="Add to channel">
              <ActionIcon
                variant="subtle"
                color="accent"
                size="sm"
                aria-label={`Add ${stream.name} to a channel`}
              >
                <Plus size={14} />
              </ActionIcon>
            </Tooltip>
          </Menu.Target>
          <Menu.Dropdown mah={320} style={{ overflowY: 'auto' }}>
            <Menu.Label>Append to failover list</Menu.Label>
            {channels.length === 0 && <Menu.Item disabled>No channels</Menu.Item>}
            {channels.map((channel) => (
              <Menu.Item key={channel.id} onClick={() => addToChannel(stream, channel)}>
                {channel.effective_name}
              </Menu.Item>
            ))}
          </Menu.Dropdown>
        </Menu>
        <RowAction
          label={`Create channel from ${stream.name}`}
          tooltip="Create a channel from this stream"
          color="gray"
          onClick={() => createChannelFrom(stream)}
        >
          <Radio size={14} />
        </RowAction>
      </Group>
    ),
    [channels, addToChannel, createChannelFrom],
  );

  const getStreamId = useCallback((stream) => String(stream.id), []);

  return (
    <section className={`${classes.pane} ${classes.streamsPane}`}>
      <h2 className={classes.paneTitle}>
        Streams
        <Text component="span" size="xs" c="dimmed" fw={400}>
          {data.count}
        </Text>
      </h2>

      <DataTable
        label="Streams"
        data={data.results}
        columns={columns}
        getRowId={getStreamId}
        loading={loading}
        error={error}
        enableSelection
        onSelectionChange={onSelectionChange}
        rowActions={rowActions}
        rowCount={data.count}
        onQueryChange={setQuery}
        filterKey={groupFilter}
        searchPlaceholder="Search streams"
        emptyMessage={error ? 'Streams could not be loaded.' : 'No streams yet.'}
        toolbar={
          <Group gap={6} wrap="nowrap">
            <Select
              size="xs"
              data={groupOptions}
              value={groupFilter}
              onChange={setGroupFilter}
              placeholder="All groups"
              aria-label="Filter streams by group"
              clearable
              searchable
              w={160}
              comboboxProps={{ withinPortal: true }}
            />
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
                      title: 'Delete streams',
                      message: `Delete ${selected.length} streams? Any channel using one loses that failover entry.`,
                      onConfirm: bulkDelete,
                    })
                  }
                >
                  Delete
                </Button>
              </>
            )}
          </Group>
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
    </section>
  );
}
