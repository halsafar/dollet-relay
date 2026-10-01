import { useCallback, useMemo, useState } from 'react';
import {
  Alert,
  Anchor,
  Badge,
  Button,
  Group,
  Loader,
  Menu,
  Progress,
  Text,
  Tooltip,
} from '@mantine/core';
import { Link } from 'react-router-dom';
import {
  Activity,
  ListVideo,
  RadioTower,
  SkipForward,
  Square,
  TriangleAlert,
  Users,
  X,
} from 'lucide-react';
import { notifyError, notifyQuiet } from '../notify.js';

import { Page } from '../layout/AppLayout.jsx';
import { LogoImage } from '../components/LogoImage.jsx';
import { ConfirmModal } from '../components/ConfirmModal.jsx';
import { useResource } from '../api/useResource.js';
import {
  channels as channelsApi,
  stats as statsApi,
  systemEvents as eventsApi,
} from '../api/resources.js';
import { useSession } from '../auth/session.js';
import { useLiveStats } from './useLiveStats.js';
import classes from './Stats.module.css';
import { RowAction } from '../components/RowAction.jsx';

/** Seconds as `1:05:30` or `5:30`, for a progress readout. */
function formatClock(seconds) {
  const total = Math.max(0, Math.round(seconds ?? 0));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const rest = total % 60;
  const pad = (n) => String(n).padStart(2, '0');
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(rest)}` : `${minutes}:${pad(rest)}`;
}
const PHASE_COLORS = {
  streaming: 'accent',
  connecting: 'blue',
  buffering: 'yellow',
  switching: 'yellow',
  failed: 'red',
  stopped: 'gray',
};

function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const power = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** power;
  return `${value >= 10 || power === 0 ? Math.round(value) : value.toFixed(1)} ${units[power]}`;
}

function formatSince(from, now = Date.now()) {
  return formatClock((now - new Date(from).getTime()) / 1000);
}

/**
 * Which byte stream a session carries: `{kind: 'raw' | 'profile', profile_id}`.
 *
 * A written-out wire form rather than serde's derived one, so this reads a
 * field instead of branching on the shape of a Rust enum.
 */
function isRaw(output) {
  return !output || output.kind === 'raw';
}

function describeOutput(output) {
  return isRaw(output) ? 'Direct' : `Output profile ${output.profile_id}`;
}

/**
 * The badge row, built only from fields ffmpeg actually reported.
 *
 * Every one of these is parsed from stderr and is legitimately absent until
 * the first frames arrive, so a placeholder would be a lie for the first few
 * seconds of every session.
 */
function mediaBadges(session) {
  const { media, progress } = session;
  const badges = [];

  if (media.width && media.height) badges.push(`${media.width}x${media.height}`);

  // The source rate, not the encoder's: a 2x catch-up on 25 fps reports
  // `fps=50`, which would read as a 50 fps channel.
  const fps = progress.actual_fps ?? media.source_fps;
  if (fps) badges.push(`${fps.toFixed(2)} fps`);

  // Verbatim, and deliberately not turned into a resolution: `1080p` is a
  // label a source chose for itself, and an anamorphic stream makes the
  // obvious guess wrong.
  if (media.quality) badges.push(media.quality);

  if (media.video_codec) badges.push(media.video_codec);
  if (media.audio_codec) badges.push(media.audio_codec);
  if (media.audio_channels) badges.push(media.audio_channels);
  if (media.input_format) badges.push(media.input_format);
  if (progress.speed) badges.push(`${progress.speed.toFixed(2)}x`);

  return badges;
}

export function Stats() {
  const isAdmin = useSession((state) => state.isAdmin());
  const { sessions, error, live, status, refresh } = useLiveStats(isAdmin);
  const [confirming, setConfirming] = useState(null);

  /**
   * A row can disappear between render and click — this list is live. The
   * server answers 404 for a stale id rather than a silent success, and that
   * is the expected outcome of the race, not a fault.
   */
  const act = useCallback(
    async (what, run, describe) => {
      try {
        const result = await run();
        notifyQuiet(describe(result));
      } catch (failure) {
        if (failure?.status === 404) {
          notifyQuiet('That session had already gone.');
        } else {
          // The title says which action; the message is the server's, and a
          // 409 explains its own refusal far better than this page could.
          notifyError(`Could not ${what}`, failure);
        }
      }
      await refresh();
    },
    [refresh],
  );

  const stopChannel = useCallback(
    (session, name, viewers) =>
      setConfirming({
        title: 'Stop this channel',
        message:
          viewers === 0
            ? `Stop ${name}? Nothing is watching it, but any transcode reading it ends too.`
            : `Stop ${name}? ${viewers} ${viewers === 1 ? 'viewer is' : 'viewers are'} watching and will be disconnected immediately.`,
        confirmLabel: 'Stop channel',
        onConfirm: () =>
          act(
            'stop the channel',
            () => statsApi.stopChannel(session.channel),
            () => `Stopped ${name}`,
          ),
      }),
    [act],
  );

  /**
   * A manual switch is a choice, not a failure. The engine spends no retry and
   * records no error for it, and styling this as a warning would undo that.
   *
   * It is also not instant: the ring still holds buffered content and a client
   * watches through it rather than skipping, so a viewer keeps seeing the old
   * source for a few seconds. Promising otherwise would make correct behaviour
   * look like a bug.
   */
  const nextSource = useCallback(
    (session, name) =>
      act(
        'switch source',
        () => statsApi.nextSource(session.channel),
        (result) =>
          `${name} moved to source ${(result?.source_index ?? 0) + 1}. Viewers see the change once the buffered few seconds have played out.`,
      ),
    [act],
  );

  const changeSource = useCallback(
    (session, index, sourceName) =>
      act(
        'switch source',
        () => statsApi.changeSource(session.channel, index),
        () =>
          `Switched to ${sourceName}. Viewers see the change once the buffered few seconds have played out.`,
      ),
    [act],
  );

  const disconnectClient = useCallback(
    (session, client, name) =>
      setConfirming({
        title: 'Disconnect this viewer',
        message: `Disconnect ${client.ip ?? 'this client'} from ${name}? Their stream stops immediately; the channel stays up for everyone else.`,
        confirmLabel: 'Disconnect',
        onConfirm: () =>
          act(
            'disconnect that client',
            () => statsApi.stopClient(session.channel, client.id),
            () => `Disconnected ${client.ip ?? 'the client'}`,
          ),
      }),
    [act],
  );

  const loadChannels = useCallback(() => channelsApi.list(), []);
  const { data: channels } = useResource(loadChannels, []);

  const loadEvents = useCallback(() => eventsApi.list(50), []);
  const { data: events, error: eventsError } = useResource(loadEvents, []);

  // Sessions carry a channel UUID and nothing else about the channel, so the
  // name and artwork come from the lineup.
  const byUuid = useMemo(
    () => new Map(channels.map((channel) => [channel.uuid, channel])),
    [channels],
  );

  if (!isAdmin) {
    return (
      <Page title="Stats">
        <Alert color="gray" variant="light" icon={<TriangleAlert size={16} />}>
          Stats are available to administrators only. They carry channel identifiers,
          upstream URLs and client addresses.
        </Alert>
      </Page>
    );
  }

  // Viewers, not client records: the transcode behind an output profile is a
  // client of the raw ring, and counting it makes "1 client" mean nobody is
  // watching.
  const clientCount = (sessions ?? []).reduce(
    (total, session) =>
      total + session.clients.filter((client) => !client.internal).length,
    0,
  );

  return (
    <Page
      title="Stats"
      subtitle="Active streams, as the engine sees them"
      actions={
        <Group gap={8}>
          <Text size="xs" c="dimmed">
            {sessions === null
              ? 'Loading…'
              : `${sessions.length} ${sessions.length === 1 ? 'stream' : 'streams'} · ${clientCount} ${clientCount === 1 ? 'client' : 'clients'}`}
          </Text>
          <Tooltip
            label={
              live
                ? 'Live — pushed by the server every 2 seconds'
                : `Socket ${status}; polling every 5 seconds instead`
            }
          >
            <Badge
              size="sm"
              variant="light"
              color={live ? 'accent' : 'yellow'}
              leftSection={
                live ? <RadioTower size={11} /> : <Loader size={9} color="yellow" />
              }
            >
              {live ? 'Live' : 'Polling'}
            </Badge>
          </Tooltip>
        </Group>
      }
    >
      {error && (
        <Alert color="red" variant="light" mb="sm" icon={<TriangleAlert size={16} />}>
          {error.message}
        </Alert>
      )}

      {sessions === null && !error && (
        <Group gap={8} p="lg" justify="center">
          <Loader size="sm" color="accent" />
          <Text size="sm" c="dimmed">
            Reading session statistics…
          </Text>
        </Group>
      )}

      {sessions !== null && sessions.length === 0 && (
        <div className={classes.empty}>
          <Activity size={28} strokeWidth={1.5} color="var(--mantine-color-dark-3)" />
          <Text fw={600} c="gray.3">
            Nothing is streaming
          </Text>
          <Text size="sm" c="dimmed" maw={420}>
            This is the normal state for an idle server. A session appears here as soon as
            a client starts playing a channel.
          </Text>
        </div>
      )}

      {sessions !== null && sessions.length > 0 && (
        <div className={classes.cards}>
          {sessions.map((session) => (
            <SessionCard
              key={`${session.channel}-${describeOutput(session.output)}`}
              session={session}
              channel={byUuid.get(session.channel)}
              onStop={stopChannel}
              onDisconnect={disconnectClient}
              onNext={nextSource}
              onChange={changeSource}
            />
          ))}
        </div>
      )}

      <SystemEvents events={events} error={eventsError} />

      {confirming && (
        <ConfirmModal
          title={confirming.title}
          message={confirming.message}
          confirmLabel={confirming.confirmLabel}
          onConfirm={confirming.onConfirm}
          onClose={() => setConfirming(null)}
        />
      )}
    </Page>
  );
}

function SessionCard({ session, channel, onStop, onDisconnect, onNext, onChange }) {
  const badges = mediaBadges(session);
  const name = channel?.effective_name ?? session.channel;

  // The transcode reading this channel is not a viewer. Counting it as one
  // makes "1 client" mean nobody is watching.
  const viewers = session.clients.filter((client) => !client.internal);
  const internal = session.clients.filter((client) => client.internal);

  return (
    <section className={classes.card} aria-label={`Stream ${name}`}>
      <div className={classes.cardHead}>
        <LogoImage src={channel?.logo_url ?? null} alt="" size={26} />
        <div style={{ minWidth: 0, flex: 1 }}>
          <div className={classes.channelName}>{name}</div>
          <div className={classes.output}>
            {describeOutput(session.output)} · source {session.source_index + 1}
            {session.switches > 0 &&
              ` · ${session.switches} ${session.switches === 1 ? 'failover' : 'failovers'}`}
          </div>
        </div>
        <Badge size="sm" variant="light" color={PHASE_COLORS[session.phase] ?? 'gray'}>
          {session.phase}
        </Badge>
        {!session.healthy && (
          <Tooltip label="The engine has marked this session unhealthy">
            <Badge size="sm" variant="light" color="red">
              unhealthy
            </Badge>
          </Tooltip>
        )}
        <RowAction
          label={`Stop ${name}`}
          tooltip="End this channel for everyone watching"
          color="red"
          onClick={() => onStop(session, name, viewers.length)}
        >
          <Square size={14} />
        </RowAction>
      </div>

      <NowPlaying playing={session.now_playing} />

      {session.url && (
        <div className={classes.url} title={session.url}>
          {session.url}
        </div>
      )}

      {/* A profile session reads the raw session's ring, so it follows
          whichever source the channel is on. Offering the control here would
          be offering a guaranteed 409. */}
      {isRaw(session.output) ? (
        <Group gap={6} mt={8}>
          <Button
            size="compact-xs"
            variant="default"
            leftSection={<SkipForward size={12} />}
            onClick={() => onNext(session, name)}
          >
            Next source
          </Button>
          <SourceMenu session={session} channel={channel} onChange={onChange} />
          <Text component="span" size="xs" c="dimmed">
            Moves this session only — the failover order is unchanged.
          </Text>
        </Group>
      ) : (
        <Text size="xs" c="dimmed" mt={8}>
          This transcode follows whichever source the channel is on. Switch the
          channel&apos;s own session instead.
        </Text>
      )}

      {session.last_error && (
        <Alert color="red" variant="light" p="xs" fz="xs" mt="xs">
          {session.last_error}
        </Alert>
      )}

      {badges.length > 0 && (
        <div className={classes.badges}>
          {badges.map((badge) => (
            <span key={badge} className={classes.badge}>
              {badge}
            </span>
          ))}
        </div>
      )}

      <div className={classes.metrics}>
        <Metric label="Uptime" value={formatSince(session.started_at)} />
        <Metric label="Transferred" value={formatBytes(session.total_bytes)} />
        <Metric
          label="Buffer"
          value={`${session.buffer.seconds.toFixed(1)}s · ${formatBytes(session.buffer.bytes)}`}
        />
        {session.progress.bitrate_kbps != null && (
          <Metric
            label="Bitrate"
            value={`${(session.progress.bitrate_kbps / 1000).toFixed(2)} Mbps`}
          />
        )}
        <Metric label="Viewers" value={String(viewers.length)} />
      </div>

      {/* Only streamlink reports a quality label, and it reports neither speed
          nor pace — a documented limitation of its progress line, not a
          reading that failed to arrive. */}
      {session.media.quality && session.progress.speed == null && (
        <Text size="xs" c="dimmed" mb={6}>
          streamlink does not report playback speed, so there is none to show.
        </Text>
      )}

      {internal.length > 0 && (
        <Text size="xs" c="dimmed" mb={6}>
          A transcode is reading this channel, which is why it stays up with
          {viewers.length === 0 ? ' nobody watching' : ' or without viewers'}. It cannot
          be disconnected on its own — stop the channel instead.
        </Text>
      )}

      {viewers.length === 0 ? (
        <Text size="xs" c="dimmed">
          No viewers connected. The session stays up briefly after the last one leaves, so
          a channel change does not restart the stream.
        </Text>
      ) : (
        <table className={classes.clients} aria-label={`Clients of ${name}`}>
          <thead>
            <tr>
              <th>Address</th>
              <th>Connected</th>
              <th>Duration</th>
              <th>Sent</th>
              <th>Client</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {viewers.map((client) => (
              <tr key={client.id}>
                <td>{client.ip ?? 'unknown'}</td>
                <td>{new Date(client.connected_at).toLocaleTimeString()}</td>
                <td>{formatSince(client.connected_at)}</td>
                <td>{formatBytes(client.bytes_sent)}</td>
                <td className={classes.userAgent} title={client.user_agent ?? ''}>
                  {client.user_agent ?? '—'}
                </td>
                <td>
                  <RowAction
                    label={`Disconnect ${client.ip ?? client.id}`}
                    tooltip="Disconnect this viewer"
                    color="red"
                    onClick={() => onDisconnect(session, client, name)}
                  >
                    <X size={13} />
                  </RowAction>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}

/**
 * The channel's sources, in failover order, fetched when the menu opens.
 *
 * `change_stream` takes a position in that order, so the list has to come from
 * the same place the Channels editor writes it.
 */
function SourceMenu({ session, channel, onChange }) {
  const [sources, setSources] = useState(null);
  const [failed, setFailed] = useState(false);

  const open = async () => {
    if (sources || failed || !channel) return;
    try {
      setSources(await channelsApi.streams(channel.id));
    } catch {
      setFailed(true);
    }
  };

  return (
    <Menu withinPortal position="bottom-start" onOpen={open}>
      <Menu.Target>
        <Button size="compact-xs" variant="default" leftSection={<ListVideo size={12} />}>
          Choose source
        </Button>
      </Menu.Target>
      <Menu.Dropdown mah={300} style={{ overflowY: 'auto' }}>
        <Menu.Label>Failover order</Menu.Label>
        {!channel && <Menu.Item disabled>This channel no longer exists</Menu.Item>}
        {channel && sources === null && !failed && (
          <Menu.Item disabled>Loading…</Menu.Item>
        )}
        {failed && <Menu.Item disabled>Sources could not be loaded</Menu.Item>}
        {sources?.length === 0 && <Menu.Item disabled>No sources</Menu.Item>}
        {sources?.map((source, index) => (
          <Menu.Item
            key={source.id}
            disabled={index === session.source_index}
            onClick={() => onChange(session, index, source.name)}
          >
            {index + 1}. {source.name}
            {index === session.source_index && ' (current)'}
          </Menu.Item>
        ))}
      </Menu.Dropdown>
    </Menu>
  );
}

/**
 * What is on the channel this session is carrying.
 *
 * Four states, never absent: a missing block would read as "still loading",
 * and two of these states mean "show nothing" for reasons a user would act on
 * differently. An unmapped channel is a guide assignment they can go and fix;
 * a deleted one is not.
 */
function NowPlaying({ playing }) {
  if (!playing || playing.state === 'unknown') {
    return (
      <Text size="xs" c="dimmed" mb={8}>
        This channel has been deleted, so there is no guide to show.
      </Text>
    );
  }

  if (playing.state === 'unmapped') {
    return (
      <Text size="xs" c="dimmed" mb={8}>
        No guide data for this channel.{' '}
        <Anchor component={Link} to="/guide" size="xs">
          Assign one on the TV Guide
        </Anchor>
        .
      </Text>
    );
  }

  if (playing.state === 'gap') {
    return (
      <Text size="xs" c="dimmed" mb={8}>
        Nothing scheduled on this channel right now.
      </Text>
    );
  }

  const duration = playing.duration_seconds || 0;
  const elapsed = playing.elapsed_seconds ?? 0;

  return (
    <div className={classes.nowPlaying}>
      <Group gap={6} wrap="nowrap">
        <span className={classes.nowTitle}>{playing.title}</span>
        {playing.generated && (
          <Tooltip label="Generated by the dummy guide, not from a real listing">
            <Badge size="xs" variant="light" color="gray">
              generated
            </Badge>
          </Tooltip>
        )}
      </Group>

      {playing.sub_title && (
        <div className={classes.nowSubtitle}>{playing.sub_title}</div>
      )}

      {/* Server-computed: a browser clock minutes out of true puts a visibly
          wrong marker on a half-hour programme. */}
      <Progress
        value={duration > 0 ? (elapsed / duration) * 100 : 0}
        size="sm"
        color="accent"
        mt={5}
      />
      <div className={classes.nowTimes}>
        <span>{formatClock(elapsed)} elapsed</span>
        <span>{formatClock(playing.remaining_seconds)} remaining</span>
      </div>

      {playing.description && (
        <div className={classes.nowDescription}>{playing.description}</div>
      )}
    </div>
  );
}

function Metric({ label, value }) {
  return (
    <div className={classes.metric}>
      <span className={classes.metricLabel}>{label}</span>
      <span className={classes.metricValue}>{value}</span>
    </div>
  );
}

function SystemEvents({ events, error }) {
  return (
    <div className={classes.events}>
      <div className={classes.eventsHead}>
        <Users size={14} />
        System events
      </div>

      {error && (
        <Text size="xs" c="dimmed" p="sm">
          Events could not be loaded: {error.message}
        </Text>
      )}

      {!error && events.length === 0 && (
        <Text size="xs" c="dimmed" p="sm">
          No events recorded yet.
        </Text>
      )}

      {events.map((event) => (
        <div key={event.id} className={classes.eventRow}>
          <span className={classes.eventTime}>
            {new Date(event.occurred_at).toLocaleString()}
          </span>
          <span className={classes.eventType}>{event.event_type}</span>
          <span className={classes.eventDetail}>{event.channel_name ?? ''}</span>
        </div>
      ))}
    </div>
  );
}
