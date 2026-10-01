import { createStore } from 'zustand/vanilla';

const STORAGE_KEY = 'dollet.tokens';

/**
 * @typedef {object} TokenState
 * @property {string | null} access
 * @property {string | null} refresh
 */

/** @returns {TokenState} */
function readStorage() {
  try {
    const raw = globalThis.localStorage?.getItem(STORAGE_KEY);
    if (!raw) return { access: null, refresh: null };
    const parsed = JSON.parse(raw);
    return {
      access: typeof parsed?.access === 'string' ? parsed.access : null,
      refresh: typeof parsed?.refresh === 'string' ? parsed.refresh : null,
    };
  } catch {
    // A corrupt entry must not brick the app into a permanent white screen.
    return { access: null, refresh: null };
  }
}

function writeStorage(state) {
  try {
    if (!state.refresh && !state.access) globalThis.localStorage?.removeItem(STORAGE_KEY);
    else globalThis.localStorage?.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    // Private-browsing quota failures degrade to a session that ends on reload.
  }
}

/**
 * Expiry of a JWT in epoch milliseconds, or null if it cannot be read.
 *
 * Only the `exp` claim is trusted for anything, and only to decide whether to
 * refresh before opening a WebSocket — the handshake cannot retry with a 401
 * the way a fetch can, so a stale token there costs a full backoff cycle.
 * Authorization decisions stay on the server.
 *
 * @param {string | null} token
 */
export function jwtExpiry(token) {
  if (!token) return null;
  const [, payload] = token.split('.');
  if (!payload) return null;
  try {
    const json = atob(payload.replace(/-/g, '+').replace(/_/g, '/'));
    const exp = JSON.parse(json)?.exp;
    return typeof exp === 'number' ? exp * 1000 : null;
  } catch {
    return null;
  }
}

const store = createStore(() => readStorage());

/**
 * Holds the JWT pair. Deliberately a vanilla store, not a React hook: the API
 * client reads it from outside React, and a second copy of this state is how
 * refresh races start.
 *
 * `localStorage` over an httpOnly cookie is deliberate: the same token works
 * for the WebSocket handshake, where headers cannot be set.
 */
export const tokenStore = {
  subscribe: store.subscribe,
  getState: store.getState,

  getAccess: () => store.getState().access,
  getRefresh: () => store.getState().refresh,

  /** @param {{ access: string, refresh?: string | null }} tokens */
  setTokens({ access, refresh }) {
    const next = { access, refresh: refresh ?? store.getState().refresh ?? null };
    store.setState(next, true);
    writeStorage(next);
  },

  clear() {
    const next = { access: null, refresh: null };
    store.setState(next, true);
    writeStorage(next);
  },

  /** True when there is any credential worth attempting a request with. */
  hasTokens: () => Boolean(store.getState().refresh || store.getState().access),

  /** Treats an unreadable token as expired, so the caller refreshes rather than guesses. */
  isAccessExpired(skewMs = 30_000) {
    const expiry = jwtExpiry(store.getState().access);
    return expiry === null || expiry - skewMs <= Date.now();
  },
};
