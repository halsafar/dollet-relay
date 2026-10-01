import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { notifyError, notifyQuiet } from '../notify.js';

import { Stats } from './Stats.jsx';
import {
  channels as channelsApi,
  stats as statsApi,
  systemEvents as eventsApi,
} from '../api/resources.js';
import { api } from '../api/client.js';
import { ws } from '../ws/client.js';
import { useSession } from '../auth/session.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  channels: { list: vi.fn(), streams: vi.fn() },
  stats: {
    get: vi.fn(),
    stopChannel: vi.fn(),
    stopClient: vi.fn(),
    nextSource: vi.fn(),
    changeSource: vi.fn(),
  },
  systemEvents: { list: vi.fn() },
  USER_LEVELS: { STREAMER: 0, STANDARD: 1, ADMIN: 10 },
}));

vi.mock('../api/client.js', () => ({
  api: { refreshAccessToken: vi.fn() },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

vi.mock('../ws/client.js', () => {
  const subscribers = new Set();
  const statusListeners = new Set();
  let status = 'idle';
  return {
    ws: {
      connect: vi.fn(),
      close: vi.fn(),
      getStatus: () => status,
      onStatusChange: vi.fn((handler) => {
        statusListeners.add(handler);
        return () => statusListeners.delete(handler);
      }),
      subscribe: vi.fn((handler) => {
        subscribers.add(handler);
        return () => subscribers.delete(handler);
      }),
      // Test seams standing in for the server.
      _push: (type, data) => subscribers.forEach((fn) => fn(data, { type, data })),
      _setStatus: (next) => {
        status = next;
        statusListeners.forEach((fn) => fn(next));
      },
      _reset: () => {
        subscribers.clear();
        statusListeners.clear();
        status = 'idle';
      },
    },
  };
});

/** Shaped exactly as `dollet_stream::SessionStats` serializes. */
const SESSION = {
  channel: '11111111-1111-1111-1111-111111111111',
  output: { kind: 'raw', profile_id: null },
  phase: 'streaming',
  healthy: true,
  source_index: 0,
  source_id: 5,
  url: 'http://provider.test/live/stream.ts',
  switches: 0,
  last_error: null,
  started_at: new Date(Date.now() - 125_000).toISOString(),
  total_bytes: 52_428_800,
  buffer: { chunks: 40, bytes: 10_485_760, head: 900, oldest: 860, seconds: 14.8 },
  media: {
    input_format: 'mpegts',
    video_codec: 'h264',
    width: 1280,
    height: 720,
    source_fps: 59.94,
    pixel_format: 'yuv420p',
    video_bitrate_kbps: 3570,
    audio_codec: 'ac3',
    sample_rate: 48000,
    audio_channels: 'stereo',
    audio_bitrate_kbps: 192,
  },
  progress: { speed: 1.01, fps: 59.94, actual_fps: 59.94, bitrate_kbps: 3570 },
  now_playing: {
    state: 'programme',
    title: 'PBS News Hour',
    sub_title: null,
    description: 'Co-anchors offer in-depth analysis of current events.',
    start: '2026-02-10T18:00:00Z',
    stop: '2026-02-10T19:00:00Z',
    // Server-computed on purpose; the page must not recompute these.
    elapsed_seconds: 2505,
    remaining_seconds: 1095,
    duration_seconds: 3600,
  },
  clients: [
    {
      id: 'c1',
      ip: '10.0.2.163',
      user_agent: 'VLC/3.0.21 LibVLC/3.0.21',
      connected_at: new Date(Date.now() - 65_000).toISOString(),
      bytes_sent: 3_145_728,
      internal: false,
    },
  ],
};

/** The transcode behind an output profile, which is not a viewer. */
const INTERNAL_CLIENT = {
  id: 'transcode-1',
  ip: null,
  user_agent: null,
  connected_at: new Date(Date.now() - 300_000).toISOString(),
  bytes_sent: 99_000_000,
  internal: true,
};

/** A second session on the same channel, through a transcode. */
const PROFILE_SESSION = {
  ...SESSION,
  output: { kind: 'profile', profile_id: 3 },
  phase: 'buffering',
  healthy: false,
  source_index: 2,
  switches: 2,
  last_error: 'upstream closed the connection',
  clients: [],
};

const CHANNELS = [
  {
    id: 1,
    uuid: '11111111-1111-1111-1111-111111111111',
    effective_name: 'KETC-HD',
    logo_url: 'http://logos.test/pbs.png',
  },
];

const EVENTS = [
  {
    id: 1,
    event_type: 'stream_started',
    occurred_at: new Date().toISOString(),
    channel_uuid: '22222222-2222-2222-2222-222222222222',
    channel_name: 'KMOV-HD',
    details: {},
  },
];

function asAdmin() {
  useSession.setState({ status: 'authenticated', user: { id: 1, user_level: 10 } });
}

beforeEach(() => {
  vi.clearAllMocks();
  ws._reset();
  asAdmin();
  channelsApi.list.mockResolvedValue(CHANNELS);
  eventsApi.list.mockResolvedValue(EVENTS);
  statsApi.get.mockResolvedValue([]);
  statsApi.stopChannel.mockResolvedValue({ stopped: 1 });
  statsApi.stopClient.mockResolvedValue({ disconnected: true });
  statsApi.nextSource.mockResolvedValue({ switched: true, source_index: 1 });
  statsApi.changeSource.mockResolvedValue({ switched: true, source_index: 2 });
  channelsApi.streams.mockResolvedValue([
    { id: 100, name: 'Primary feed' },
    { id: 101, name: 'Backup feed' },
  ]);
  api.refreshAccessToken.mockResolvedValue('access-2');
});

afterEach(() => {
  vi.useRealTimers();
});

describe('an idle server', () => {
  it('says nothing is streaming, and does not call that an error', async () => {
    renderWithProviders(<Stats />);

    expect(await screen.findByText('Nothing is streaming')).toBeInTheDocument();
    // The normal state for a server nobody is watching.
    expect(screen.getByText(/normal state for an idle server/i)).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('counts zero streams and zero clients', async () => {
    renderWithProviders(<Stats />);
    expect(await screen.findByText('0 streams · 0 clients')).toBeInTheDocument();
  });

  it('distinguishes not-yet-loaded from genuinely empty', async () => {
    let release;
    statsApi.get.mockReturnValue(new Promise((resolve) => (release = resolve)));
    renderWithProviders(<Stats />);

    expect(screen.getByText(/Reading session statistics/)).toBeInTheDocument();
    expect(screen.queryByText('Nothing is streaming')).not.toBeInTheDocument();

    release([]);
    expect(await screen.findByText('Nothing is streaming')).toBeInTheDocument();
  });
});

describe('an active session', () => {
  beforeEach(() => {
    statsApi.get.mockResolvedValue([SESSION]);
  });

  it('names the channel from the lineup, not the bare UUID', async () => {
    renderWithProviders(<Stats />);

    // Sessions carry only a channel UUID.
    expect(await screen.findByText('KETC-HD')).toBeInTheDocument();
    expect(screen.queryByText(SESSION.channel)).not.toBeInTheDocument();
  });

  it('falls back to the UUID when the channel is not in the lineup', async () => {
    channelsApi.list.mockResolvedValue([]);
    renderWithProviders(<Stats />);

    expect(await screen.findByText(SESSION.channel)).toBeInTheDocument();
  });

  it('shows the media badges ffmpeg reported', async () => {
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    for (const badge of ['1280x720', '59.94 fps', 'h264', 'ac3', 'stereo', 'mpegts']) {
      expect(screen.getByText(badge)).toBeInTheDocument();
    }
    expect(screen.getByText('1.01x')).toBeInTheDocument();
  });

  it('omits badges for fields ffmpeg has not reported yet', async () => {
    statsApi.get.mockResolvedValue([
      {
        ...SESSION,
        media: { ...SESSION.media, width: null, height: null, audio_codec: null },
        progress: { speed: null, fps: null, actual_fps: null, bitrate_kbps: null },
      },
    ]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // Absent for the first seconds of every session; a placeholder would lie.
    expect(screen.queryByText('1280x720')).not.toBeInTheDocument();
    expect(screen.queryByText('ac3')).not.toBeInTheDocument();
    expect(screen.getByText('h264')).toBeInTheDocument();
  });

  it('prefers the source frame rate over the encoder rate', async () => {
    // A 2x catch-up on a 25 fps source reports fps=50, which would read as a
    // 50 fps channel.
    statsApi.get.mockResolvedValue([
      {
        ...SESSION,
        media: { ...SESSION.media, source_fps: 25 },
        progress: { ...SESSION.progress, fps: 50, actual_fps: 25, speed: 2 },
      },
    ]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText('25.00 fps')).toBeInTheDocument();
    expect(screen.queryByText('50.00 fps')).not.toBeInTheDocument();
  });

  it('reports transfer, buffer and uptime', async () => {
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText('50 MB')).toBeInTheDocument();
    expect(screen.getByText('14.8s · 10 MB')).toBeInTheDocument();
    expect(screen.getByText('3.57 Mbps')).toBeInTheDocument();
  });

  it('lists each client with its address, agent and bytes', async () => {
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    const table = screen.getByRole('table', { name: 'Clients of KETC-HD' });
    expect(within(table).getByText('10.0.2.163')).toBeInTheDocument();
    expect(within(table).getByText('VLC/3.0.21 LibVLC/3.0.21')).toBeInTheDocument();
    expect(within(table).getByText('3.0 MB')).toBeInTheDocument();
  });

  it('explains a session with no viewers rather than looking broken', async () => {
    statsApi.get.mockResolvedValue([{ ...SESSION, clients: [] }]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText(/No viewers connected/)).toBeInTheDocument();
    expect(screen.getByText('1 stream · 0 clients')).toBeInTheDocument();
  });

  it('distinguishes a transcode session from the direct one', async () => {
    statsApi.get.mockResolvedValue([SESSION, PROFILE_SESSION]);
    renderWithProviders(<Stats />);

    // Two sessions on one channel: the raw ring and a ring downstream of it,
    // so the channel name legitimately appears twice.
    expect(await screen.findAllByText('KETC-HD')).toHaveLength(2);
    expect(screen.getByText(/Direct · source 1/)).toBeInTheDocument();
    expect(
      screen.getByText(/Output profile 3 · source 3 · 2 failovers/),
    ).toBeInTheDocument();
  });

  it('surfaces an unhealthy session and its last error', async () => {
    statsApi.get.mockResolvedValue([PROFILE_SESSION]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText('unhealthy')).toBeInTheDocument();
    expect(screen.getByText('buffering')).toBeInTheDocument();
    expect(screen.getByText('upstream closed the connection')).toBeInTheDocument();
  });

  it('shows the upstream currently being read', async () => {
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText('http://provider.test/live/stream.ts')).toBeInTheDocument();
  });
});

describe('live updates', () => {
  it('polls while the socket is not open', async () => {
    renderWithProviders(<Stats />);

    await waitFor(() => expect(statsApi.get).toHaveBeenCalled());
    expect(await screen.findByText('Polling')).toBeInTheDocument();
  });

  it('takes pushed frames once the socket is open', async () => {
    renderWithProviders(<Stats />);
    await screen.findByText('Nothing is streaming');

    ws._setStatus('open');
    ws._push('channel_stats', [SESSION]);

    expect(await screen.findByText('KETC-HD')).toBeInTheDocument();
    expect(await screen.findByText('Live')).toBeInTheDocument();
  });

  it('stops polling once the socket is open', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    renderWithProviders(<Stats />);
    await waitFor(() => expect(statsApi.get).toHaveBeenCalledTimes(1));

    ws._setStatus('open');
    await waitFor(() => expect(ws.onStatusChange).toHaveBeenCalled());
    statsApi.get.mockClear();

    await vi.advanceTimersByTimeAsync(20_000);

    // The socket is authoritative; polling alongside it doubles the load for
    // no extra freshness.
    expect(statsApi.get).not.toHaveBeenCalled();
  });

  it('resumes polling when the socket drops', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    renderWithProviders(<Stats />);
    ws._setStatus('open');
    await screen.findByText('Live');
    statsApi.get.mockClear();

    ws._setStatus('reconnecting');

    // A stats page that silently stops updating is worse than a slow one.
    await waitFor(() => expect(statsApi.get).toHaveBeenCalled());
    expect(await screen.findByText('Polling')).toBeInTheDocument();
  });

  it('does not poll faster than once every five seconds', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    renderWithProviders(<Stats />);
    await waitFor(() => expect(statsApi.get).toHaveBeenCalledTimes(1));

    await vi.advanceTimersByTimeAsync(12_000);

    // One immediate call plus two intervals. A reconnect storm on the socket
    // must not become a request storm here.
    expect(statsApi.get.mock.calls.length).toBeLessThanOrEqual(3);
  });

  it('ignores frames that are not channel_stats', async () => {
    renderWithProviders(<Stats />);
    await screen.findByText('Nothing is streaming');
    ws._setStatus('open');

    // The server greets every client with `{type: "hello", data: {admin}}`,
    // and this page only cares about `channel_stats`.
    ws._push('hello', { admin: true });

    expect(screen.getByText('Nothing is streaming')).toBeInTheDocument();
  });

  it('refreshes a stale access token before opening the socket', async () => {
    const { tokenStore } = await import('../auth/tokenStore.js');
    vi.spyOn(tokenStore, 'isAccessExpired').mockReturnValue(true);

    renderWithProviders(<Stats />);

    // A handshake cannot retry a 401 the way a fetch can.
    await waitFor(() => expect(api.refreshAccessToken).toHaveBeenCalled());
    await waitFor(() => expect(ws.connect).toHaveBeenCalled());
  });

  it('closes the socket when the page unmounts', async () => {
    const { unmount } = renderWithProviders(<Stats />);
    await waitFor(() => expect(ws.connect).toHaveBeenCalled());

    unmount();
    expect(ws.close).toHaveBeenCalled();
  });

  it('surfaces a polling failure', async () => {
    statsApi.get.mockRejectedValue(new ApiError('Server unavailable.', { status: 503 }));
    renderWithProviders(<Stats />);

    expect(await screen.findByText('Server unavailable.')).toBeInTheDocument();
  });
});

describe('the admin gate', () => {
  it('refuses a non-admin, matching the server', async () => {
    useSession.setState({ status: 'authenticated', user: { id: 2, user_level: 1 } });
    renderWithProviders(<Stats />);

    expect(
      await screen.findByText(/available to administrators only/i),
    ).toBeInTheDocument();
  });

  it('opens no socket and issues no request for a non-admin', async () => {
    useSession.setState({ status: 'authenticated', user: { id: 2, user_level: 0 } });
    renderWithProviders(<Stats />);
    await screen.findByText(/administrators only/i);

    // Both endpoints are admin-only, so this would be a socket yielding
    // nothing plus a 403 every five seconds.
    expect(ws.connect).not.toHaveBeenCalled();
    expect(statsApi.get).not.toHaveBeenCalled();
  });
});

describe('system events', () => {
  it('lists recent events', async () => {
    renderWithProviders(<Stats />);

    expect(await screen.findByText('stream_started')).toBeInTheDocument();
    expect(eventsApi.list).toHaveBeenCalledWith(50);
  });

  it('says so when there are none', async () => {
    eventsApi.list.mockResolvedValue([]);
    renderWithProviders(<Stats />);

    expect(await screen.findByText('No events recorded yet.')).toBeInTheDocument();
  });

  it('reports a failure without taking the page down', async () => {
    eventsApi.list.mockRejectedValue(new ApiError('Nope', { status: 500 }));
    renderWithProviders(<Stats />);

    expect(await screen.findByText(/Events could not be loaded/)).toBeInTheDocument();
    expect(screen.getByText('Nothing is streaming')).toBeInTheDocument();
  });
});

describe('unexpected payload shapes', () => {
  it('treats an unfamiliar output kind as a transcode, not as a crash', async () => {
    statsApi.get.mockResolvedValue([
      { ...SESSION, output: { kind: 'something-new', profile_id: 9 } },
    ]);
    renderWithProviders(<Stats />);

    // Anything that is not `raw` is a transcode of the channel, so an unknown
    // kind still reads as one rather than blanking the card.
    expect(await screen.findByText(/Output profile 9 · source 1/)).toBeInTheDocument();
  });

  it('treats a non-list stats frame as no sessions', async () => {
    renderWithProviders(<Stats />);
    await screen.findByText('Nothing is streaming');
    ws._setStatus('open');

    ws._push('channel_stats', null);

    expect(screen.getByText('Nothing is streaming')).toBeInTheDocument();
  });

  it('does not open the socket when the token refresh fails', async () => {
    const { tokenStore } = await import('../auth/tokenStore.js');
    vi.spyOn(tokenStore, 'isAccessExpired').mockReturnValue(true);
    api.refreshAccessToken.mockRejectedValue(new ApiError('Expired', { status: 401 }));

    renderWithProviders(<Stats />);

    await waitFor(() => expect(api.refreshAccessToken).toHaveBeenCalled());
    // The session is gone; the route guard handles it. Connecting anyway would
    // just spin the reconnect.
    expect(ws.connect).not.toHaveBeenCalled();
  });
});

describe('partial session data', () => {
  it('counts a single failover in the singular', async () => {
    statsApi.get.mockResolvedValue([{ ...SESSION, switches: 1 }]);
    renderWithProviders(<Stats />);

    expect(await screen.findByText(/1 failover$/)).toBeInTheDocument();
  });

  it('renders a phase it has no colour for', async () => {
    statsApi.get.mockResolvedValue([{ ...SESSION, phase: 'draining' }]);
    renderWithProviders(<Stats />);

    // The engine may gain a phase before this page knows about it.
    expect(await screen.findByText('draining')).toBeInTheDocument();
  });

  it('handles a client with no address or user agent', async () => {
    // Real when the proxy in front forwards neither.
    statsApi.get.mockResolvedValue([
      {
        ...SESSION,
        clients: [{ ...SESSION.clients[0], ip: null, user_agent: null }],
      },
    ]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    const table = screen.getByRole('table', { name: 'Clients of KETC-HD' });
    expect(within(table).getByText('unknown')).toBeInTheDocument();
    expect(within(table).getByText('—')).toBeInTheDocument();
  });

  it('renders an event that is not tied to a channel', async () => {
    eventsApi.list.mockResolvedValue([
      {
        id: 2,
        event_type: 'logout',
        occurred_at: new Date().toISOString(),
        channel_uuid: null,
        channel_name: null,
        details: {},
      },
    ]);
    renderWithProviders(<Stats />);

    expect(await screen.findByText('logout')).toBeInTheDocument();
  });

  it('omits the bitrate metric when ffmpeg has not reported one', async () => {
    statsApi.get.mockResolvedValue([
      { ...SESSION, progress: { ...SESSION.progress, bitrate_kbps: null } },
    ]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.queryByText('Bitrate')).not.toBeInTheDocument();
    expect(screen.getByText('Transferred')).toBeInTheDocument();
  });
});

describe('stopping a channel', () => {
  beforeEach(() => {
    statsApi.get.mockResolvedValue([SESSION]);
  });

  it('names the channel and its viewer count before stopping', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Stop KETC-HD' }));

    // Someone is watching; the confirmation has to say so.
    expect(await screen.findByText(/1 viewer is watching/)).toBeInTheDocument();
    expect(statsApi.stopChannel).not.toHaveBeenCalled();
  });

  it('stops only after the confirmation is accepted', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Stop KETC-HD' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Stop channel' }),
    );

    await waitFor(() =>
      expect(statsApi.stopChannel).toHaveBeenCalledWith(SESSION.channel),
    );
  });

  it('abandons the stop on cancel', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Stop KETC-HD' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Cancel' }),
    );

    expect(statsApi.stopChannel).not.toHaveBeenCalled();
  });

  it('says so when nothing is watching', async () => {
    const user = userEvent.setup();
    statsApi.get.mockResolvedValue([{ ...SESSION, clients: [INTERNAL_CLIENT] }]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Stop KETC-HD' }));

    expect(await screen.findByText(/Nothing is watching it/)).toBeInTheDocument();
  });

  it('re-reads the list afterwards', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');
    statsApi.get.mockClear();

    await user.click(screen.getByRole('button', { name: 'Stop KETC-HD' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Stop channel' }),
    );

    await waitFor(() => expect(statsApi.get).toHaveBeenCalled());
  });
});

describe('disconnecting one viewer', () => {
  beforeEach(() => {
    statsApi.get.mockResolvedValue([SESSION]);
  });

  it('names the address being disconnected', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Disconnect 10.0.2.163' }));

    expect(
      await screen.findByText(/Disconnect 10\.0\.2\.163 from KETC-HD/),
    ).toBeInTheDocument();
    expect(screen.getByText(/channel stays up for everyone else/)).toBeInTheDocument();
  });

  it('sends the channel uuid and the client id', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Disconnect 10.0.2.163' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Disconnect' }),
    );

    await waitFor(() =>
      expect(statsApi.stopClient).toHaveBeenCalledWith(SESSION.channel, 'c1'),
    );
  });

  it('treats a stale row as an expected race, not a fault', async () => {
    const user = userEvent.setup();
    // The list is live: the client left between render and click. The server
    // answers 404 rather than a silent success precisely so this is visible.
    statsApi.stopClient.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Disconnect 10.0.2.163' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Disconnect' }),
    );

    await waitFor(() => expect(notifyQuiet).toHaveBeenCalled());
    // Quiet, not an error: it is the expected outcome of the race.
    expect(notifyQuiet.mock.calls.at(-1)[0]).toMatch(/already gone/);
    expect(notifyError).not.toHaveBeenCalled();
  });

  it('passes the engine refusal through verbatim', async () => {
    const user = userEvent.setup();
    statsApi.stopClient.mockRejectedValue(
      new ApiError(
        'that is the transcode reading this channel, not a viewer; stop the channel instead',
        { status: 409 },
      ),
    );
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Disconnect 10.0.2.163' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Disconnect' }),
    );

    await waitFor(() => expect(notifyError).toHaveBeenCalled());
    const [title, failure] = notifyError.mock.calls.at(-1);
    // The title names the action, so a refused switch cannot read as a
    // failed stop, and the message is the engine's own sentence.
    expect(title).toBe('Could not disconnect that client');
    expect(failure.message).toMatch(/stop the channel instead/);
  });
});

describe('the transcode consumer', () => {
  it('explains why a channel with no viewers is still up', async () => {
    statsApi.get.mockResolvedValue([{ ...SESSION, clients: [INTERNAL_CLIENT] }]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText(/A transcode is reading this channel/)).toBeInTheDocument();
    expect(screen.getByText(/nobody watching/)).toBeInTheDocument();
    expect(screen.getByText(/stop the channel instead/)).toBeInTheDocument();
  });

  it('is not a row with a disconnect button and a blank address', async () => {
    statsApi.get.mockResolvedValue([{ ...SESSION, clients: [INTERNAL_CLIENT] }]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // Evicting it would strand the encoder, so the engine refuses — offering
    // the button would be offering a 409.
    expect(
      screen.queryByRole('button', { name: /Disconnect transcode-1/ }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole('table', { name: 'Clients of KETC-HD' }),
    ).not.toBeInTheDocument();
    expect(screen.getByText(/No viewers connected/)).toBeInTheDocument();
  });

  it('is not counted as a viewer', async () => {
    statsApi.get.mockResolvedValue([
      { ...SESSION, clients: [INTERNAL_CLIENT, SESSION.clients[0]] },
    ]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // Two client records, one viewer. Counting the transcode makes "1 client"
    // mean nobody is watching.
    expect(screen.getByText('1 stream · 1 client')).toBeInTheDocument();
    const table = screen.getByRole('table', { name: 'Clients of KETC-HD' });
    expect(within(table).queryAllByRole('row')).toHaveLength(2);
  });
});

describe('quality and speed', () => {
  it('shows a source-advertised quality label verbatim', async () => {
    statsApi.get.mockResolvedValue([
      { ...SESSION, media: { ...SESSION.media, quality: '1080p' } },
    ]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // A label, not a resolution: an anamorphic source makes the guess wrong.
    expect(screen.getByText('1080p')).toBeInTheDocument();
    expect(screen.getByText('1280x720')).toBeInTheDocument();
  });

  it('explains a missing speed on a streamlink source', async () => {
    statsApi.get.mockResolvedValue([
      {
        ...SESSION,
        media: { ...SESSION.media, quality: '1080p' },
        progress: { ...SESSION.progress, speed: null },
      },
    ]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // A documented limitation of streamlink's progress line, not a gap.
    expect(
      screen.getByText(/streamlink does not report playback speed/),
    ).toBeInTheDocument();
  });

  it('says nothing about speed when ffmpeg simply has not reported yet', async () => {
    statsApi.get.mockResolvedValue([
      { ...SESSION, progress: { ...SESSION.progress, speed: null } },
    ]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // No quality label means this is not streamlink; the reading is just
    // pending, and claiming otherwise would be wrong.
    expect(screen.queryByText(/streamlink does not report/)).not.toBeInTheDocument();
  });
});

describe('now playing', () => {
  const withPlaying = (now_playing) => {
    statsApi.get.mockResolvedValue([{ ...SESSION, now_playing }]);
  };

  it('shows the programme and its progress', async () => {
    statsApi.get.mockResolvedValue([SESSION]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText('PBS News Hour')).toBeInTheDocument();
    expect(screen.getByText(/in-depth analysis/)).toBeInTheDocument();
  });

  it('uses the server clock rather than the browser clock', async () => {
    statsApi.get.mockResolvedValue([SESSION]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // 2505s and 1095s, exactly as sent. A browser minutes out of true would
    // put a visibly wrong marker on a half-hour programme.
    expect(screen.getByText('41:45 elapsed')).toBeInTheDocument();
    expect(screen.getByText('18:15 remaining')).toBeInTheDocument();
  });

  it('marks a generated block as generated but still shows it', async () => {
    withPlaying({
      state: 'programme',
      generated: true,
      title: 'KETC-HD Programming',
      sub_title: null,
      description: null,
      elapsed_seconds: 600,
      remaining_seconds: 3000,
      duration_seconds: 3600,
    });
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // Real as far as any client is concerned, so it renders as a programme.
    expect(screen.getByText('KETC-HD Programming')).toBeInTheDocument();
    expect(screen.getByText('generated')).toBeInTheDocument();
  });

  it('says nothing is scheduled for a mapped channel with a gap', async () => {
    withPlaying({ state: 'gap' });
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(
      screen.getByText(/Nothing scheduled on this channel right now/),
    ).toBeInTheDocument();
  });

  it('offers a fix for an unmapped channel, which is actionable', async () => {
    withPlaying({ state: 'unmapped' });
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // Distinct from `unknown`: this one the user can go and fix.
    expect(screen.getByText(/No guide data for this channel/)).toBeInTheDocument();
    expect(
      screen.getByRole('link', { name: /Assign one on the TV Guide/ }),
    ).toHaveAttribute('href', '/guide');
  });

  it('says a deleted channel is deleted, and offers nothing to fix', async () => {
    withPlaying({ state: 'unknown' });
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText(/This channel has been deleted/)).toBeInTheDocument();
    expect(screen.queryByRole('link', { name: /TV Guide/ })).not.toBeInTheDocument();
  });

  it('treats a session with no now_playing at all as unknown', async () => {
    statsApi.get.mockResolvedValue([{ ...SESSION, now_playing: undefined }]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // The server always sends it; if it ever does not, do not claim a gap.
    expect(screen.getByText(/This channel has been deleted/)).toBeInTheDocument();
  });
});

describe('switching source', () => {
  beforeEach(() => {
    statsApi.get.mockResolvedValue([SESSION]);
  });

  it('moves to the next source without asking first', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Next source' }));

    await waitFor(() =>
      expect(statsApi.nextSource).toHaveBeenCalledWith(SESSION.channel),
    );
  });

  it('reports the switch as a choice, not a failure', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Next source' }));

    await waitFor(() => expect(notifyQuiet).toHaveBeenCalled());
    // The engine spends no retry and records no error, so this is not routed
    // through the error channel at all.
    expect(notifyError).not.toHaveBeenCalled();
    expect(notifyQuiet.mock.calls.at(-1)[0]).toMatch(/source 2/);
  });

  it('does not promise the viewer sees it immediately', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Next source' }));

    await waitFor(() => expect(notifyQuiet).toHaveBeenCalled());
    // The ring still holds buffered content and a client watches through it
    // rather than skipping — correct behaviour that looks like a bug.
    expect(notifyQuiet.mock.calls.at(-1)[0]).toMatch(/buffered few seconds/);
  });

  it('says the failover order is unchanged', async () => {
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // A manual switch is a temporary override, not a re-ordering.
    expect(screen.getByText(/Moves this session only/)).toBeInTheDocument();
  });

  it('lists the channel sources in failover order', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Choose source' }));

    // jsdom leaves the floating dropdown `display: none` once it is positioned,
    // which can land before the sources do, so both lookups include hidden
    // elements. Each label is split across text nodes, so match on the items
    // themselves.
    await screen.findByRole('menuitem', { name: /2\. Backup feed/, hidden: true });
    const items = screen.getAllByRole('menuitem', { hidden: true });
    expect(items.map((item) => item.textContent)).toEqual([
      '1. Primary feed (current)',
      '2. Backup feed',
    ]);
    expect(items[0]).toHaveAttribute('data-disabled');
  });

  it('switches by position in that order', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Choose source' }));
    await user.click(
      await screen.findByRole('menuitem', { name: /Backup feed/, hidden: true }),
    );

    await waitFor(() =>
      expect(statsApi.changeSource).toHaveBeenCalledWith(SESSION.channel, 1),
    );
  });

  it('treats a torn-down session as the expected race', async () => {
    const user = userEvent.setup();
    statsApi.nextSource.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Next source' }));

    await waitFor(() => expect(notifyQuiet).toHaveBeenCalled());
    expect(notifyError).not.toHaveBeenCalled();
  });

  it('passes an out-of-range refusal through with its range', async () => {
    const user = userEvent.setup();
    statsApi.nextSource.mockRejectedValue(
      new ApiError('this channel has 2 source(s), numbered 0 to 1', { status: 400 }),
    );
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Next source' }));

    await waitFor(() => expect(notifyError).toHaveBeenCalled());
    const [title, failure] = notifyError.mock.calls.at(-1);
    // Named for the action attempted, not for whichever branch got there.
    expect(title).toBe('Could not switch source');
    expect(failure.message).toMatch(/numbered 0 to 1/);
  });

  it('offers nothing on a transcode session, and says where the control lives', async () => {
    statsApi.get.mockResolvedValue([PROFILE_SESSION]);
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    // A profile follows whichever source the channel is on, so the button
    // would be a guaranteed 409.
    expect(screen.queryByRole('button', { name: 'Next source' })).not.toBeInTheDocument();
    expect(
      screen.getByText(/follows whichever source the channel is on/),
    ).toBeInTheDocument();
  });

  it('says so when the channel behind the session is gone', async () => {
    const user = userEvent.setup();
    channelsApi.list.mockResolvedValue([]);
    renderWithProviders(<Stats />);
    await screen.findByText(SESSION.channel);

    await user.click(screen.getByRole('button', { name: 'Choose source' }));

    expect(
      await screen.findByRole('menuitem', {
        name: 'This channel no longer exists',
        hidden: true,
      }),
    ).toBeInTheDocument();
  });

  it('reports sources that could not be loaded', async () => {
    const user = userEvent.setup();
    channelsApi.streams.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    renderWithProviders(<Stats />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Choose source' }));

    expect(
      await screen.findByRole('menuitem', {
        name: 'Sources could not be loaded',
        hidden: true,
      }),
    ).toBeInTheDocument();
  });
});
