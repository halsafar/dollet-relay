import { tokenStore } from '../auth/tokenStore.js';

/**
 * @typedef {'idle' | 'connecting' | 'open' | 'reconnecting'} WsStatus
 */

const BASE_DELAY_MS = 500;
const MAX_DELAY_MS = 30_000;

/**
 * The token rides in the WebSocket subprotocol, which is the one request header
 * a browser allows on an upgrade. A query parameter would put the JWT in
 * the request line, where `TraceLayer` and any fronting proxy log it. The server
 * echoes `auth.jwt` back as the negotiated protocol.
 */
export const AUTH_PROTOCOL = 'auth.jwt';

/**
 * Live updates from the server: `{type, data}` envelopes over `/ws`.
 *
 * Three types, all admin-only and all `{type, data}`: `hello` once on
 * connect with `{admin}`, then `channel_stats` and `job_progress` on a
 * two-second tick. `job_progress` carries the same rows `/api/core/jobs/`
 * answers with, `running` included.
 *
 * Mounted by `useLiveStats`, which the Stats page uses. Nothing else connects:
 * a socket per page would be a reconnect loop on every navigation, and the
 * only frames the server sends are for that page.
 *
 * Written as a factory because the reconnect schedule is the interesting part
 * and testing it needs an injectable socket and clock.
 *
 * @param {{ url?: string, getToken?: () => string | null,
 *           WebSocketImpl?: typeof WebSocket,
 *           setTimeoutImpl?: typeof setTimeout,
 *           clearTimeoutImpl?: typeof clearTimeout,
 *           random?: () => number }} [deps]
 */
export function createWsClient({
  url = '/ws',
  getToken = () => tokenStore.getAccess(),
  WebSocketImpl = globalThis.WebSocket,
  setTimeoutImpl = globalThis.setTimeout.bind(globalThis),
  clearTimeoutImpl = globalThis.clearTimeout.bind(globalThis),
  random = Math.random,
} = {}) {
  /** @type {Set<(data: unknown, envelope: object) => void>} */
  const listeners = new Set();
  /** @type {Set<(status: WsStatus) => void>} */
  const statusListeners = new Set();

  let socket = null;
  let retryTimer = null;
  let attempt = 0;
  let closedByUs = false;
  /** @type {WsStatus} */
  let status = 'idle';

  function setStatus(next) {
    if (status === next) return;
    status = next;
    for (const listener of statusListeners) listener(status);
  }

  function endpoint() {
    if (/^wss?:/.test(url)) return url;
    const scheme = globalThis.location?.protocol === 'https:' ? 'wss:' : 'ws:';
    return `${scheme}//${globalThis.location?.host ?? 'localhost'}${url}`;
  }

  function dispatch(raw) {
    let envelope;
    try {
      envelope = JSON.parse(raw);
    } catch {
      console.warn('Discarding malformed WebSocket frame');
      return;
    }
    if (!envelope || typeof envelope.type !== 'string') return;

    for (const listener of listeners) {
      try {
        listener(envelope.data, envelope);
      } catch (error) {
        // One bad subscriber must not stop the others from seeing the frame.
        console.error('WebSocket subscriber threw', error);
      }
    }
  }

  /**
   * Exponential backoff with full jitter. Without the jitter every tab and
   * every client reconnects in lockstep after a server restart, which is
   * exactly when the server can least afford it.
   */
  function nextDelay() {
    const ceiling = Math.min(BASE_DELAY_MS * 2 ** attempt, MAX_DELAY_MS);
    attempt += 1;
    return Math.round(ceiling * (0.5 + random() * 0.5));
  }

  function scheduleReconnect() {
    if (closedByUs || retryTimer) return;
    setStatus('reconnecting');
    retryTimer = setTimeoutImpl(() => {
      retryTimer = null;
      open();
    }, nextDelay());
  }

  function detach(target) {
    if (!target) return;
    target.onopen = null;
    target.onmessage = null;
    target.onerror = null;
    target.onclose = null;
  }

  function open() {
    if (socket || closedByUs) return;
    setStatus(attempt === 0 ? 'connecting' : 'reconnecting');

    const token = getToken();
    const current = token
      ? new WebSocketImpl(endpoint(), [AUTH_PROTOCOL, token])
      : new WebSocketImpl(endpoint());
    socket = current;

    // Every handler checks it is still the live socket. A real WebSocket fires
    // `onclose` asynchronously, so a socket closed by `close()` reports it long
    // after a replacement has been opened — and an unguarded handler would then
    // null out the new socket and schedule a reconnect on top of it.
    current.onopen = () => {
      if (current !== socket) return;
      attempt = 0;
      setStatus('open');
    };
    current.onmessage = (event) => {
      if (current !== socket) return;
      dispatch(event.data);
    };
    current.onerror = () => {
      // `onclose` always follows, and that is where reconnect is scheduled.
    };
    current.onclose = () => {
      if (current !== socket) return;
      socket = null;
      if (closedByUs) setStatus('idle');
      else scheduleReconnect();
    };
  }

  return {
    connect() {
      closedByUs = false;
      open();
    },

    close() {
      closedByUs = true;
      if (retryTimer) {
        clearTimeoutImpl(retryTimer);
        retryTimer = null;
      }
      attempt = 0;
      const current = socket;
      socket = null;
      // Detach before closing: the late `onclose` from this socket must not be
      // able to touch state that now belongs to a later connection.
      detach(current);
      current?.close();
      setStatus('idle');
    },

    /**
     * @param {(data: unknown, envelope: {type: string, data: unknown}) => void} handler
     * @returns {() => void} Unsubscribe.
     */
    subscribe(handler) {
      listeners.add(handler);
      return () => listeners.delete(handler);
    },

    /** @param {(status: WsStatus) => void} handler @returns {() => void} */
    onStatusChange(handler) {
      statusListeners.add(handler);
      return () => statusListeners.delete(handler);
    },

    getStatus: () => status,
  };
}

export const ws = createWsClient();
