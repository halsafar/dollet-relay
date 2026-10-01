import { create } from 'zustand';

/**
 * Which address the UI shows client URLs on.
 *
 * The one genuinely hard question the Connect page has to answer. The server
 * cannot know how a client reaches it — the URLs inside a lineup are built
 * from the address that client used — so this is a choice the operator makes,
 * not a setting. It lives in `localStorage` for the same reason: it is this
 * browser's view of the deployment, and storing it server-side would hand one
 * operator's answer to everyone else.
 */
export const STORAGE_KEY = 'dollet.connect.baseUrl';

export const BASE_URL_SOURCES = {
  browser: 'browser',
  advertised: 'advertised',
  custom: 'custom',
};

/**
 * An absolute http(s) URL with no trailing slash, or null.
 *
 * `new URL` normalises case and the default port, which is what keeps
 * `HTTP://Host:80/` and `http://host` from reading as two addresses in the
 * seen-from list next to it.
 */
export function normalizeBaseUrl(value) {
  const text = String(value ?? '').trim();
  if (!text) return null;

  let url;
  try {
    url = new URL(text);
  } catch {
    return null;
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return null;

  return url.href.replace(/\/+$/, '');
}

/** Absent or unreadable storage is not an error; it just means no choice yet. */
function storedCustom() {
  try {
    return localStorage.getItem(STORAGE_KEY) ?? '';
  } catch {
    return '';
  }
}

function persist(value) {
  try {
    if (value) localStorage.setItem(STORAGE_KEY, value);
    else localStorage.removeItem(STORAGE_KEY);
  } catch {
    // A browser refusing storage costs the operator a retype, nothing more.
  }
}

/**
 * This browser's own origin, which is the default and the usual right answer.
 * Already normalised by the browser: no trailing slash, no default port.
 */
export function browserOrigin() {
  return window.location.origin;
}

export const useBaseUrlStore = create((set) => {
  const custom = storedCustom();
  return {
    // A remembered custom base is restored as the selection, not merely as
    // text in a field nobody reopened: the only reason to have typed one is to
    // be looking at those URLs.
    source: normalizeBaseUrl(custom) ? BASE_URL_SOURCES.custom : BASE_URL_SOURCES.browser,
    custom,
    /** `DOLLET_ADVERTISED_BASE_URL`, or null when the deployment has none. */
    advertised: null,

    setSource: (source) => set({ source }),
    setCustom: (value) => {
      persist(value);
      set({ custom: value });
    },
    setAdvertised: (advertised) => set({ advertised }),

    /** The "use this" button beside an address a client has actually used. */
    chooseBase: (value) => {
      persist(value);
      set({ source: BASE_URL_SOURCES.custom, custom: value });
    },
  };
});

/**
 * Falls back to this browser rather than showing a half-typed URL: every
 * address on the page derives from this, and the picker reports the problem
 * inline where it was made.
 */
export function resolveBaseUrl(state, origin) {
  if (state.source === BASE_URL_SOURCES.custom) {
    return normalizeBaseUrl(state.custom) ?? origin;
  }
  if (state.source === BASE_URL_SOURCES.advertised) {
    return normalizeBaseUrl(state.advertised) ?? origin;
  }
  return origin;
}

/** The chosen base, for anything that builds a client URL. */
export function useBaseUrl() {
  return useBaseUrlStore((state) => resolveBaseUrl(state, browserOrigin()));
}
