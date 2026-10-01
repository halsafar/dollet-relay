import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { AUTH_PROTOCOL, createWsClient } from './client.js';

/**
 * Records every socket opened, so tests can drive each one directly.
 *
 * `close()` schedules `onclose` on a timer rather than firing it inline,
 * because that is what a real WebSocket does. A synchronous fake cannot
 * observe a stale socket reporting its close after a replacement has been
 * opened, which is the failure this suite exists to catch.
 */
class FakeSocket {
  static opened = [];

  constructor(url, protocols) {
    this.url = url;
    this.protocols = protocols;
    this.readyState = 0;
    FakeSocket.opened.push(this);
  }

  accept() {
    this.readyState = 1;
    this.onopen?.();
  }

  receive(payload) {
    this.onmessage?.({
      data: typeof payload === 'string' ? payload : JSON.stringify(payload),
    });
  }

  /** An unexpected drop, reported immediately as the transport notices it. */
  drop() {
    this.readyState = 3;
    this.onclose?.();
  }

  close() {
    this.readyState = 2;
    setTimeout(() => {
      this.readyState = 3;
      this.onclose?.();
    }, 0);
  }
}

function build(overrides = {}) {
  return createWsClient({
    WebSocketImpl: FakeSocket,
    getToken: () => 'access-1',
    // Full jitter picks a delay in [ceiling/2, ceiling]; pinning random to 1
    // makes the schedule deterministic without removing the jitter itself.
    random: () => 1,
    ...overrides,
  });
}

beforeEach(() => {
  FakeSocket.opened = [];
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe('websocket client', () => {
  it('carries the token in the subprotocol, never in the URL', () => {
    const client = build();
    client.connect();

    const [socket] = FakeSocket.opened;
    expect(socket.url).toBe('ws://localhost:3000/ws');
    expect(socket.url).not.toContain('access-1');
    expect(socket.protocols).toEqual([AUTH_PROTOCOL, 'access-1']);
  });

  it('opens without a subprotocol when there is no token', () => {
    const client = build({ getToken: () => null });
    client.connect();

    expect(FakeSocket.opened[0].url).toBe('ws://localhost:3000/ws');
    expect(FakeSocket.opened[0].protocols).toBeUndefined();
  });

  it('leaves an already-absolute url alone', () => {
    const client = build({ url: 'wss://example.test/ws' });
    client.connect();
    expect(FakeSocket.opened[0].url).toBe('wss://example.test/ws');
  });

  it('reports its status through the lifecycle', () => {
    const client = build();
    const seen = [];
    client.onStatusChange((status) => seen.push(status));

    expect(client.getStatus()).toBe('idle');
    client.connect();
    FakeSocket.opened[0].accept();
    client.close();

    expect(seen).toEqual(['connecting', 'open', 'idle']);
  });

  it('delivers an envelope to subscribers', () => {
    const client = build();
    const handler = vi.fn();

    client.subscribe(handler);
    client.connect();
    FakeSocket.opened[0].accept();
    FakeSocket.opened[0].receive({ type: 'channel_stats', data: { channels: 2 } });

    expect(handler).toHaveBeenCalledWith(
      { channels: 2 },
      { type: 'channel_stats', data: { channels: 2 } },
    );
  });

  it('stops delivering after unsubscribe', () => {
    const client = build();
    const handler = vi.fn();
    const unsubscribe = client.subscribe(handler);

    client.connect();
    FakeSocket.opened[0].accept();
    FakeSocket.opened[0].receive({ type: 'tick', data: 1 });
    unsubscribe();
    FakeSocket.opened[0].receive({ type: 'tick', data: 2 });

    expect(handler).toHaveBeenCalledTimes(1);
  });

  it('keeps delivering to other subscribers when one throws', () => {
    vi.spyOn(console, 'error').mockImplementation(() => {});
    const client = build();
    const good = vi.fn();

    client.subscribe(() => {
      throw new Error('bad subscriber');
    });
    client.subscribe(good);
    client.connect();
    FakeSocket.opened[0].accept();
    FakeSocket.opened[0].receive({ type: 'tick', data: 1 });

    expect(good).toHaveBeenCalled();
  });

  it('discards a malformed or typeless frame', () => {
    vi.spyOn(console, 'warn').mockImplementation(() => {});
    const client = build();
    const handler = vi.fn();
    client.subscribe(handler);
    client.connect();
    FakeSocket.opened[0].accept();

    FakeSocket.opened[0].receive('not json');
    FakeSocket.opened[0].receive({ data: 'no type' });

    expect(handler).not.toHaveBeenCalled();
  });

  it('reconnects with exponential backoff after an unexpected close', () => {
    const client = build();
    client.connect();
    FakeSocket.opened[0].accept();

    FakeSocket.opened[0].drop();
    expect(client.getStatus()).toBe('reconnecting');
    expect(FakeSocket.opened).toHaveLength(1);

    vi.advanceTimersByTime(500);
    expect(FakeSocket.opened).toHaveLength(2);

    // Second failure waits twice as long: 500 is no longer enough.
    FakeSocket.opened[1].drop();
    vi.advanceTimersByTime(500);
    expect(FakeSocket.opened).toHaveLength(2);
    vi.advanceTimersByTime(500);
    expect(FakeSocket.opened).toHaveLength(3);
  });

  it('caps the backoff rather than growing without bound', () => {
    const client = build();
    client.connect();

    for (let i = 0; i < 12; i += 1) {
      FakeSocket.opened.at(-1).drop();
      vi.advanceTimersByTime(30_000);
    }

    expect(FakeSocket.opened).toHaveLength(13);
  });

  it('resets the backoff once a connection succeeds', () => {
    const client = build();
    client.connect();

    FakeSocket.opened[0].drop();
    vi.advanceTimersByTime(500);
    FakeSocket.opened[1].drop();
    vi.advanceTimersByTime(1000);
    FakeSocket.opened[2].accept();

    FakeSocket.opened[2].drop();
    vi.advanceTimersByTime(500);
    expect(FakeSocket.opened).toHaveLength(4);
  });

  it('picks up a refreshed token on reconnect', () => {
    let token = 'access-1';
    const client = build({ getToken: () => token });
    client.connect();

    token = 'access-2';
    FakeSocket.opened[0].drop();
    vi.advanceTimersByTime(500);

    expect(FakeSocket.opened[1].protocols).toEqual([AUTH_PROTOCOL, 'access-2']);
  });

  it('does not reconnect after an explicit close', () => {
    const client = build();
    client.connect();
    FakeSocket.opened[0].accept();

    client.close();
    vi.advanceTimersByTime(60_000);

    expect(FakeSocket.opened).toHaveLength(1);
    expect(client.getStatus()).toBe('idle');
  });

  it('cancels a pending reconnect on close', () => {
    const client = build();
    client.connect();
    FakeSocket.opened[0].drop();

    client.close();
    vi.advanceTimersByTime(60_000);

    expect(FakeSocket.opened).toHaveLength(1);
  });

  it('survives close-then-connect without leaving two sockets live', () => {
    const client = build();
    client.connect();
    FakeSocket.opened[0].accept();

    // React StrictMode does exactly this on every dev mount.
    client.close();
    client.connect();
    expect(FakeSocket.opened).toHaveLength(2);

    // The first socket's close lands only now, after the replacement exists.
    vi.advanceTimersByTime(60_000);

    expect(FakeSocket.opened).toHaveLength(2);
    FakeSocket.opened[1].accept();
    expect(client.getStatus()).toBe('open');
  });

  it('ignores frames from a socket that has been replaced', () => {
    const client = build();
    const handler = vi.fn();
    client.subscribe(handler);

    client.connect();
    const stale = FakeSocket.opened[0];
    stale.accept();

    client.close();
    client.connect();

    stale.receive({ type: 'tick', data: 'from the dead socket' });

    expect(handler).not.toHaveBeenCalled();
  });

  it('does not open a second socket when connect is called twice', () => {
    const client = build();
    client.connect();
    client.connect();
    expect(FakeSocket.opened).toHaveLength(1);
  });

  it('closes cleanly when it was never connected', () => {
    const client = build();
    expect(() => client.close()).not.toThrow();
    expect(FakeSocket.opened).toHaveLength(0);
    expect(client.getStatus()).toBe('idle');
  });

  it('reads the access token from the token store by default', async () => {
    const { tokenStore } = await import('../auth/tokenStore.js');
    tokenStore.setTokens({ access: 'from-store', refresh: 'r' });

    const client = createWsClient({ WebSocketImpl: FakeSocket, random: () => 1 });
    client.connect();

    expect(FakeSocket.opened[0].protocols).toEqual([AUTH_PROTOCOL, 'from-store']);
    tokenStore.clear();
  });

  it('stops notifying a removed status listener', () => {
    const client = build();
    const handler = vi.fn();
    const unsubscribe = client.onStatusChange(handler);

    client.connect();
    unsubscribe();
    FakeSocket.opened[0].accept();

    expect(handler).toHaveBeenCalledTimes(1);
  });
});
