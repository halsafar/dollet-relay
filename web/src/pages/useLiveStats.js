import { useCallback, useEffect, useState } from 'react';

import { api } from '../api/client.js';
import { stats as statsApi } from '../api/resources.js';
import { tokenStore } from '../auth/tokenStore.js';
import { ws } from '../ws/client.js';

/**
 * How often to poll while the socket is not open.
 *
 * Slower than the server's 2 s push, because polling is the degraded path and
 * should not cost more than the thing it stands in for.
 */
const POLL_MS = 5000;

/**
 * Live session statistics, pushed over the WebSocket and polled when it is not.
 *
 * The fallback is the point. A stats page that silently stops updating is
 * worse than one that is obviously slow: the reading looks current, so an
 * operator watching a stream die sees nothing change and concludes it is fine.
 *
 * @param {boolean} enabled Both the socket and the poll are admin-only on the
 *   server, so a non-admin landing here would otherwise hold a socket that
 *   yields nothing and poll a 403 every five seconds.
 * @returns {{sessions: object[] | null, error: Error | null, live: boolean,
 *            status: string}}
 */
export function useLiveStats(enabled = true) {
  const [sessions, setSessions] = useState(null);
  const [error, setError] = useState(null);
  const [status, setStatus] = useState(() => ws.getStatus());

  useEffect(() => ws.onStatusChange(setStatus), []);

  useEffect(() => {
    if (!enabled) return undefined;
    let cancelled = false;

    (async () => {
      // A WebSocket handshake cannot retry a 401 the way a fetch can, so a
      // stale access token would cost a full backoff cycle before the page
      // showed anything. Polling would still work, but slowly and silently.
      if (tokenStore.isAccessExpired()) {
        try {
          await api.refreshAccessToken();
        } catch {
          return; // The session is gone; the route guard takes over.
        }
      }
      if (!cancelled) ws.connect();
    })();

    return () => {
      cancelled = true;
      ws.close();
    };
  }, [enabled]);

  useEffect(
    () =>
      ws.subscribe((data, envelope) => {
        if (envelope.type !== 'channel_stats') return;
        setSessions(Array.isArray(data) ? data : []);
        setError(null);
      }),
    [],
  );

  const poll = useCallback(async () => {
    try {
      const next = await statsApi.get();
      setSessions(next);
      setError(null);
      return true;
    } catch (failure) {
      setError(failure);
      return false;
    }
  }, []);

  useEffect(() => {
    // The socket is authoritative once it is open; polling alongside it would
    // double the load for no extra freshness.
    if (!enabled || status === 'open') return undefined;

    let live = true;
    const tick = () => {
      if (live) void poll();
    };

    // Immediately, so the page is never blank while the first push is pending
    // or a reconnect is backing off.
    tick();
    const timer = setInterval(tick, POLL_MS);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [enabled, status, poll]);

  return { sessions, error, live: status === 'open', status, refresh: poll };
}
