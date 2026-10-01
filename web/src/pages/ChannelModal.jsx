import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  Alert,
  Button,
  Group,
  Loader,
  Modal,
  NumberInput,
  Select,
  Stack,
  Switch,
  Text,
  TextInput,
  Tooltip,
  UnstyledButton,
} from '@mantine/core';
import { useForm } from '@mantine/form';
import { useClipboard } from '@mantine/hooks';
import { notifyDone, notifyError } from '../notify.js';
import { ArrowDown, ArrowUp, Copy, Info, TriangleAlert, Trash2 } from 'lucide-react';

import { channels as channelsApi, epgData as epgDataApi } from '../api/resources.js';
import { LogoPicker } from '../components/LogoPicker.jsx';
import { useBaseUrl } from '../components/baseUrl.js';
import { parseChannelNumber } from './channelNumber.js';
import { streamUrl } from './connectUrls.js';
import classes from './Channels.module.css';
import { RowAction } from '../components/RowAction.jsx';

/** Mantine Select carries strings; the API carries numeric ids or null. */
const toOption = (value) =>
  value === null || value === undefined ? null : String(value);
const toId = (value) => (value === null || value === '' ? null : Number(value));

/**
 * Fields the `channel_override` row can shadow, per the `effective_channel`
 * view's `COALESCE(o.field, c.field)` columns.
 *
 * `hidden_from_output` is deliberately absent — the view reads
 * `c.hidden_from_output` directly, so it has no override to shadow it and is
 * always the base value.
 */
const OVERRIDABLE = [
  'name',
  'channel_number',
  'channel_group_id',
  'logo_id',
  'stream_profile_id',
  'tvg_id',
  'tvc_guide_stationid',
  'epg_data_id',
];

/** Keeps a burst of typing to one request. */
const SEARCH_DEBOUNCE_MS = 250;

/**
 * A guide channel whose name this browser has not been told.
 *
 * Two cases, both rare: the *base* mapping of a channel whose guide is
 * overridden, since the list serializes `epg_name` for the effective mapping
 * only; and a mapping whose guide row has gone, where `epg_name` is null. No
 * endpoint resolves an `epg_data` id to its name, and an id the user can act on
 * beats an empty label.
 */
const unnamedGuide = (id) => `guide #${id}`;

/**
 * Create or edit a channel.
 *
 * Values shown are the effective ones — what the outputs actually emit. Where
 * a field is overridden the edit is written back as an override, because
 * writing the base row under a live override changes nothing the user can see.
 */
export function ChannelModal({
  channel,
  groups,
  streamProfiles,
  logos,
  onClose,
  onSaved,
}) {
  const isEdit = Boolean(channel.id);
  const baseUrl = useBaseUrl();
  const clipboard = useClipboard({ timeout: 1200 });
  const [busy, setBusy] = useState(false);
  const [failover, setFailover] = useState([]);
  // Three states, not two. "Loaded and empty" and "never loaded" look identical
  // in an array, and treating the second as the first deletes every failover
  // row on save.
  const [streamsState, setStreamsState] = useState(isEdit ? 'loading' : 'loaded');
  const [resetFields, setResetFields] = useState(() => new Set());

  const overrides = useMemo(() => channel.override ?? {}, [channel.override]);
  const isOverridden = useCallback(
    (field) =>
      overrides[field] !== null &&
      overrides[field] !== undefined &&
      !resetFields.has(field),
    [overrides, resetFields],
  );

  const effective = useCallback(
    (field) => ((overrides[field] ?? null) !== null ? overrides[field] : channel[field]),
    [overrides, channel],
  );

  const form = useForm({
    initialValues: {
      name: effective('name') ?? '',
      channel_number: effective('channel_number') ?? '',
      channel_group_id: toOption(effective('channel_group_id')),
      logo_id: effective('logo_id') ?? null,
      stream_profile_id: toOption(effective('stream_profile_id')),
      tvg_id: effective('tvg_id') ?? '',
      tvc_guide_stationid: effective('tvc_guide_stationid') ?? '',
      epg_data_id: toOption(effective('epg_data_id')),
      // Not overridable, so always the base row.
      hidden_from_output: channel.hidden_from_output ?? false,
    },
    validate: {
      name: (value) => (value.trim() ? null : 'Required'),
      channel_number: (value) => {
        const parsed = parseChannelNumber(value);
        if (parsed !== null && parsed < 0) return 'Cannot be negative';
        return null;
      },
    },
  });

  // ----------------------------------------------------------- guide picker
  //
  // Searched, never listed: `epg_data` runs to thousands of rows, and the
  // endpoint matches the name or the `tvg_id`, because a provider label is
  // often how an operator recognises the row.
  const [guideSearch, setGuideSearch] = useState('');
  const [guideRows, setGuideRows] = useState([]);
  // The term `guideRows` answers. Busy is then a comparison rather than a flag
  // — the field is busy exactly while the box says something the list does not
  // yet account for, including a search abandoned before its answer arrived.
  const [answeredTerm, setAnsweredTerm] = useState(null);
  const [guideError, setGuideError] = useState(null);

  // Every guide row this modal has been told the name of, so the field can
  // render a mapping it never searched for.
  const [knownGuides, setKnownGuides] = useState(() => {
    const known = new Map();
    // The channel list serializes `epg_name` beside the mapping, which is what
    // lets an already-mapped channel render its guide without a fetch. It is
    // null when the mapping points at a guide row that is no longer there, and
    // then the id is all anyone has.
    if (channel.epg_name) {
      known.set(String(effective('epg_data_id')), {
        label: channel.epg_name,
        tvgId: null,
      });
    }
    return known;
  });

  const guideLabel = useCallback(
    (id) => knownGuides.get(String(id))?.label ?? unnamedGuide(id),
    [knownGuides],
  );

  const selectedGuide = form.values.epg_data_id;
  const selectedGuideLabel = selectedGuide ? guideLabel(selectedGuide) : null;

  const term = guideSearch.trim();
  // Mantine puts the chosen option's own label in the search box — on mount, on
  // every selection, on a reset — so a term equal to it is not something the
  // user asked for, and querying the server for the name it is already showing
  // would be the picker talking to itself.
  const searching = term !== '' && term !== selectedGuideLabel && term !== answeredTerm;

  useEffect(() => {
    if (!searching) return undefined;

    let live = true;
    // An answer whose search has been superseded changes nothing: the box says
    // one thing and this would make the list say another.
    const settle = (apply) => {
      if (!live) return;
      apply();
      setAnsweredTerm(term);
    };

    const timer = setTimeout(() => {
      epgDataApi.list(term).then(
        (found) =>
          settle(() => {
            setGuideRows(found);
            setGuideError(null);
            setKnownGuides((current) => {
              const next = new Map(current);
              for (const row of found) {
                next.set(String(row.id), { label: row.name, tvgId: row.tvg_id ?? null });
              }
              return next;
            });
          }),
        (failure) =>
          // An empty dropdown and a failed search look identical, and the first
          // reads as "this instance has no guide data for that".
          settle(() => setGuideError(failure?.message ?? 'The guide search failed.')),
      );
    }, SEARCH_DEBOUNCE_MS);

    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [searching, term]);

  const guideOptions = useMemo(() => {
    const options = [];
    const seen = new Set();
    const push = (value, label) => {
      if (seen.has(value)) return;
      seen.add(value);
      options.push({ value, label });
    };
    // First, so a mapped channel has its own guide in the list before any
    // search — a Select whose value is absent from `data` renders blank.
    if (selectedGuide) push(selectedGuide, guideLabel(selectedGuide));
    for (const row of guideRows) push(String(row.id), row.name);
    return options;
  }, [selectedGuide, guideLabel, guideRows]);

  const renderGuideOption = useCallback(
    ({ option }) => {
      const tvgId = knownGuides.get(option.value)?.tvgId;
      return (
        <Group gap={8} justify="space-between" wrap="nowrap" w="100%">
          <span>{option.label}</span>
          {tvgId && (
            <Text size="xs" c="dimmed">
              {tvgId}
            </Text>
          )}
        </Group>
      );
    },
    [knownGuides],
  );

  const channelId = channel.id;
  useEffect(() => {
    if (!channelId) return undefined;
    let live = true;
    channelsApi
      .streams(channelId)
      .then((rows) => {
        if (!live) return;
        setFailover(rows);
        setStreamsState('loaded');
      })
      .catch(() => {
        // The state change is what stops `streams` being sent: a swallowed
        // failure here would make the save destructive.
        if (live) setStreamsState('failed');
      });
    return () => {
      live = false;
    };
  }, [channelId]);

  const move = (index, delta) => {
    setFailover((current) => {
      const next = [...current];
      const target = index + delta;
      if (target < 0 || target >= next.length) return current;
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
  };

  const drop = (index) =>
    setFailover((current) => current.filter((_, position) => position !== index));

  const resetOverride = (field) => {
    setResetFields((current) => new Set(current).add(field));
    const base = channel[field] ?? null;
    if (
      field === 'channel_group_id' ||
      field === 'stream_profile_id' ||
      field === 'epg_data_id'
    ) {
      form.setFieldValue(field, toOption(base));
    } else if (field === 'logo_id') {
      form.setFieldValue(field, base);
    } else {
      form.setFieldValue(field, base ?? '');
    }
  };

  const groupOptions = useMemo(
    () => groups.map((group) => ({ value: String(group.id), label: group.name })),
    [groups],
  );

  const profileOptions = useMemo(
    () =>
      streamProfiles.map((profile) => ({
        value: String(profile.id),
        label: profile.name,
      })),
    [streamProfiles],
  );

  const submit = async (values) => {
    setBusy(true);

    const edited = {
      name: values.name.trim(),
      channel_number: parseChannelNumber(values.channel_number),
      channel_group_id: toId(values.channel_group_id),
      logo_id: values.logo_id ?? null,
      stream_profile_id: toId(values.stream_profile_id),
      tvg_id: values.tvg_id.trim() || null,
      tvc_guide_stationid: values.tvc_guide_stationid.trim() || null,
      epg_data_id: toId(values.epg_data_id),
    };

    // An edit to an overridden field has to land on the override, or the
    // coalesce layer keeps serving the old value and the save looks ignored.
    const payload = {};
    const overridePatch = {};
    for (const field of OVERRIDABLE) {
      if (resetFields.has(field)) {
        overridePatch[field] = null;
        payload[field] = edited[field];
      } else if (isOverridden(field)) {
        overridePatch[field] = edited[field];
      } else {
        payload[field] = edited[field];
      }
    }
    if (Object.keys(overridePatch).length > 0) payload.override = overridePatch;

    // Never overridable, so it always goes on the base row.
    payload.hidden_from_output = values.hidden_from_output;

    // Only when the list genuinely loaded. `set_streams` deletes every row for
    // the channel before reinserting, so sending `[]` because a fetch failed
    // silently unassigns every stream and the channel stops playing.
    if (isEdit && streamsState === 'loaded') {
      payload.streams = failover.map((stream) => stream.id);
    }

    try {
      if (isEdit) await channelsApi.update(channel.id, payload);
      else await channelsApi.create(payload);
      notifyDone(`Saved ${edited.name}`);
      onSaved();
    } catch (failure) {
      notifyError('Could not save the channel', failure);
    } finally {
      setBusy(false);
    }
  };

  /** Label plus, when the field is overridden, what the provider said and a reset. */
  const labelFor = (field, text, format = (value) => String(value)) => {
    if (!isOverridden(field)) return text;
    const base = channel[field];
    return (
      <Group gap={6} wrap="nowrap">
        <span>{text}</span>
        <Tooltip
          label={`Overriding the provider value: ${base === null || base === undefined ? 'none' : format(base)}`}
        >
          <span className={classes.overrideBadge}>overridden</span>
        </Tooltip>
        <UnstyledButton
          className={classes.resetOverride}
          onClick={() => resetOverride(field)}
          aria-label={`Reset ${text} to the provider value`}
        >
          Reset
        </UnstyledButton>
      </Group>
    );
  };

  const groupName = (id) => groups.find((group) => group.id === id)?.name ?? 'none';
  const profileName = (id) =>
    streamProfiles.find((profile) => profile.id === id)?.name ?? 'none';

  return (
    <Modal
      opened
      onClose={onClose}
      title={isEdit ? `Edit ${channel.effective_name ?? channel.name}` : 'Add channel'}
      size="lg"
    >
      <form onSubmit={form.onSubmit(submit)}>
        <Stack gap="sm">
          <Group grow align="flex-start">
            <TextInput
              label={labelFor('name', 'Name')}
              withAsterisk
              {...form.getInputProps('name')}
            />
            <NumberInput
              label={labelFor('channel_number', 'Channel number')}
              // Blank means two different things: a new channel takes the next
              // free number in its group's range, an existing one is made
              // unnumbered — which the HDHR lineup drops.
              description={
                isEdit
                  ? 'Decimals are valid — 2.1 is a subchannel. Leave blank for none.'
                  : "Decimals are valid — 2.1 is a subchannel. Leave blank for the next free number in the group's range."
              }
              placeholder={isEdit ? 'Unnumbered' : 'Automatic'}
              min={0}
              decimalScale={2}
              step={0.1}
              {...form.getInputProps('channel_number')}
            />
          </Group>

          <Group grow align="flex-start">
            <Select
              label={labelFor('channel_group_id', 'Group', groupName)}
              data={groupOptions}
              clearable
              searchable
              placeholder="No group"
              {...form.getInputProps('channel_group_id')}
            />
            <Select
              label={labelFor('stream_profile_id', 'Stream profile', profileName)}
              data={profileOptions}
              clearable
              placeholder="Inherit default"
              {...form.getInputProps('stream_profile_id')}
            />
          </Group>

          <Select
            label={labelFor('epg_data_id', 'Guide', guideLabel)}
            description="The guide channel this one takes its listings from. It fills in at the next refresh."
            data={guideOptions}
            value={form.values.epg_data_id}
            onChange={(value) => form.setFieldValue('epg_data_id', value)}
            onSearchChange={setGuideSearch}
            searchable
            clearable
            clearButtonProps={{ 'aria-label': 'Clear the guide mapping' }}
            // The server already matched on the name *or* the tvg id; filtering
            // again here would drop every row that matched on the id.
            filter={({ options }) => options}
            renderOption={renderGuideOption}
            leftSection={
              searching ? <Loader size="xs" aria-label="Searching the guide" /> : null
            }
            placeholder="No guide"
            nothingFoundMessage={
              searching ? 'Searching…' : 'Type a guide channel name or id'
            }
            error={guideError}
          />

          <TextInput
            label={labelFor('tvg_id', 'Guide id (tvg-id)')}
            description="Matches this channel to guide data by id."
            {...form.getInputProps('tvg_id')}
          />

          <TextInput
            label={labelFor('tvc_guide_stationid', 'Gracenote station id')}
            description="Used instead of the guide id when a source is set to gracenote."
            {...form.getInputProps('tvc_guide_stationid')}
          />

          <Switch
            label="Include in HDHR, M3U and EPG output"
            description="Turn this off to keep the channel out of Plex without deleting it."
            checked={!form.values.hidden_from_output}
            onChange={(event) =>
              form.setFieldValue('hidden_from_output', !event.currentTarget.checked)
            }
          />

          <LogoPicker
            label={labelFor('logo_id', 'Logo')}
            logos={logos}
            value={form.values.logo_id}
            onChange={(id) => form.setFieldValue('logo_id', id)}
          />

          {isEdit && (
            <Stack gap={6}>
              <Group gap={6}>
                <Text size="sm" fw={500}>
                  Failover order
                </Text>
                <Tooltip
                  label="The engine tries these in order and moves to the next when one fails"
                  multiline
                  w={240}
                >
                  <Info size={13} color="var(--mantine-color-dark-2)" />
                </Tooltip>
              </Group>

              {streamsState === 'loading' && (
                <Group gap={8}>
                  <Loader size="xs" color="gray" />
                  <Text size="xs" c="dimmed">
                    Loading the failover list…
                  </Text>
                </Group>
              )}

              {streamsState === 'failed' && (
                <Alert
                  color="red"
                  variant="light"
                  p="xs"
                  fz="xs"
                  icon={<TriangleAlert size={15} />}
                  title="Failover list could not be loaded"
                >
                  Saving will leave this channel&apos;s streams untouched. Close and
                  reopen to edit them.
                </Alert>
              )}

              {streamsState === 'loaded' && (
                <>
                  <Alert color="gray" variant="light" p="xs" fz="xs">
                    Position 1 is the stream played first. Every stream below it is a
                    fallback, tried in this order when the one above stops.
                  </Alert>
                  {failover.length === 0 && (
                    <Text size="xs" c="dimmed">
                      No streams assigned. Add them from the Streams pane.
                    </Text>
                  )}
                </>
              )}

              {streamsState === 'loaded' &&
                failover.map((stream, index) => (
                  <div
                    key={stream.id}
                    className={classes.failoverRow}
                    data-primary={index === 0 || undefined}
                  >
                    <span className={classes.failoverPosition}>{index + 1}</span>
                    <span className={classes.failoverName}>{stream.name}</span>
                    <RowAction
                      label={`Move ${stream.name} up`}
                      tooltip="Move up"
                      color="gray"
                      disabled={index === 0}
                      onClick={() => move(index, -1)}
                    >
                      <ArrowUp size={14} />
                    </RowAction>
                    <RowAction
                      label={`Move ${stream.name} down`}
                      tooltip="Move down"
                      color="gray"
                      disabled={index === failover.length - 1}
                      onClick={() => move(index, 1)}
                    >
                      <ArrowDown size={14} />
                    </RowAction>
                    <RowAction
                      label={`Remove ${stream.name}`}
                      tooltip="Remove from channel"
                      color="red"
                      onClick={() => drop(index)}
                    >
                      <Trash2 size={14} />
                    </RowAction>
                  </div>
                ))}
            </Stack>
          )}

          <Group justify={channel.uuid ? 'space-between' : 'flex-end'} gap="xs" mt="xs">
            {/* What you paste into VLC when a channel looks wrong: the same
                bytes the proxy serves Plex, with nothing else in the way. The
                output profile is not here because a channel does not have one
                — it is a property of the output URL, chosen on Connect. */}
            {channel.uuid && (
              <Button
                variant="subtle"
                color="gray"
                size="xs"
                leftSection={<Copy size={13} />}
                onClick={() => clipboard.copy(streamUrl(baseUrl, channel.uuid))}
              >
                {clipboard.copied ? 'Copied' : 'Copy stream URL'}
              </Button>
            )}
            <Group gap="xs">
              <Button variant="default" onClick={onClose}>
                Cancel
              </Button>
              <Button type="submit" loading={busy} disabled={streamsState === 'loading'}>
                Save
              </Button>
            </Group>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}
