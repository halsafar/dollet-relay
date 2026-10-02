import { ApiError, toApiError } from './errors.js';
import { tokenStore } from '../auth/tokenStore.js';

/**
 * @typedef {object} TokenAccess
 * @property {() => string | null} getAccess
 * @property {() => string | null} getRefresh
 * @property {(tokens: { access: string, refresh?: string }) => void} setTokens
 * @property {() => void} clear
 */

/**
 * @typedef {object} RequestOptions
 * @property {'GET'|'POST'|'PATCH'|'DELETE'} [method]
 * @property {unknown} [body] JSON-encoded unless it is a `FormData` or a `Blob`.
 * @property {Record<string, string | number | boolean | null | undefined>} [query]
 * @property {Record<string, string>} [headers]
 * @property {boolean} [auth] Set false for endpoints that must not carry a token.
 * @property {boolean} [blob] Resolve a successful response as a `Blob` rather
 *   than reading it as text, for a file the user is saving.
 * @property {AbortSignal} [signal]
 */

const REFRESH_PATH = '/accounts/token/refresh/';

/**
 * The single chokepoint every request goes through.
 *
 * Built as a factory rather than a module-level singleton because the refresh
 * logic is the part most worth testing, and testing it needs a controllable
 * fetch and token store.
 *
 * Clearing the token store is the only signal this layer sends about a dead
 * session; the session store subscribes to it and the route guards follow.
 *
 * @param {{ baseUrl?: string, tokens: TokenAccess, fetchImpl?: typeof fetch }} deps
 */
export function createApiClient({
  baseUrl = '/api',
  tokens,
  fetchImpl = globalThis.fetch.bind(globalThis),
} = {}) {
  // Callers that 401 *while a refresh is already running* share that one
  // request, so a page firing six parallel requests on mount refreshes once
  // rather than six times. This is not a general lock: the slot is released
  // when the refresh settles, so a request that 401s afterwards starts a new
  // one. That is the wanted behaviour — the alternative is a request retrying
  // with a token that has since expired.
  /** @type {Promise<string> | null} */
  let refreshInFlight = null;

  function buildUrl(path, query) {
    const url = `${baseUrl}${path}`;
    if (!query) return url;
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(query)) {
      if (value === undefined || value === null || value === '') continue;
      params.append(key, String(value));
    }
    const qs = params.toString();
    return qs ? `${url}?${qs}` : url;
  }

  async function readBody(response) {
    if (response.status === 204) return null;
    const text = await response.text();
    if (!text) return null;
    const type = response.headers.get('content-type') ?? '';
    if (type.includes('json')) {
      try {
        return JSON.parse(text);
      } catch (cause) {
        throw new ApiError('The server returned malformed JSON.', {
          status: response.status,
          payload: text,
          cause,
        });
      }
    }
    return text;
  }

  async function send(path, options, accessToken) {
    const { method = 'GET', body, query, headers = {}, signal } = options;

    /** @type {Record<string, string>} */
    const finalHeaders = { Accept: 'application/json', ...headers };
    if (accessToken) finalHeaders.Authorization = `Bearer ${accessToken}`;

    let payload;
    if (body instanceof FormData) {
      payload = body; // fetch sets the multipart boundary itself.
    } else if (body instanceof Blob) {
      // Sent as the bytes they are, with the file's own type: a backup is a
      // database-sized zip, and reading it into a string to wrap it in JSON
      // would hold it in memory twice.
      payload = body;
    } else if (body !== undefined) {
      payload = JSON.stringify(body);
      finalHeaders['Content-Type'] = 'application/json';
    }

    try {
      return await fetchImpl(buildUrl(path, query), {
        method,
        headers: finalHeaders,
        body: payload,
        signal,
      });
    } catch (cause) {
      if (cause?.name === 'AbortError') throw cause;
      throw new ApiError('Could not reach the server.', { status: 0, cause });
    }
  }

  /** Refreshes the access token, collapsing concurrent callers onto one request. */
  function refreshAccessToken() {
    if (refreshInFlight) return refreshInFlight;

    refreshInFlight = (async () => {
      const refresh = tokens.getRefresh();
      if (!refresh) throw new ApiError('Your session has expired.', { status: 401 });

      const response = await send(
        REFRESH_PATH,
        { method: 'POST', body: { refresh } },
        null,
      );
      const body = await readBody(response);
      if (!response.ok) throw toApiError(response.status, body);

      const access = body?.access;
      if (!access) {
        throw new ApiError('The server returned no access token.', {
          status: response.status,
          payload: body,
        });
      }
      // The server may or may not rotate the refresh token; keep the old one
      // when none comes back.
      tokens.setTokens({ access, refresh: body.refresh ?? refresh });
      return access;
    })();

    return refreshInFlight.finally(() => {
      refreshInFlight = null;
    });
  }

  /**
   * @param {string} path Relative to `baseUrl`, e.g. `/accounts/users/`.
   * @param {RequestOptions} [options]
   * @returns {Promise<any>}
   */
  async function request(path, options = {}) {
    const useAuth = options.auth !== false;
    let response = await send(path, options, useAuth ? tokens.getAccess() : null);

    // Refresh and retry exactly once. A 401 on the retry means the refresh token
    // is dead too, and looping would just hammer the server.
    if (response.status === 401 && useAuth && path !== REFRESH_PATH) {
      try {
        const access = await refreshAccessToken();
        response = await send(path, options, access);
      } catch (error) {
        const failure =
          error instanceof ApiError
            ? error
            : new ApiError('Your session has expired.', { status: 401, cause: error });
        // Only a rejected credential is grounds for discarding it. A 500 or a
        // dropped connection from the refresh endpoint says nothing about
        // whether the refresh token is still valid, and throwing it away turns
        // a transient server fault into a forced re-login.
        if (failure.isAuthError) tokens.clear();
        throw failure;
      }
      if (response.status === 401) tokens.clear();
    }

    if (options.blob && response.ok) return response.blob();
    const body = await readBody(response);
    if (!response.ok) throw toApiError(response.status, body);
    return body;
  }

  return {
    request,
    refreshAccessToken,
    get: (path, options) => request(path, { ...options, method: 'GET' }),
    post: (path, body, options) => request(path, { ...options, method: 'POST', body }),
    patch: (path, body, options) => request(path, { ...options, method: 'PATCH', body }),
    delete: (path, options) => request(path, { ...options, method: 'DELETE' }),
  };
}

/** The application-wide client, for everything under `/api`. */
export const api = createApiClient({ tokens: tokenStore });

/**
 * The same client for the handlers mounted at the root rather than under
 * `/api` — `/proxy/ts/stream/*`, `/output/*`, `/hdhr/*` and `/ws`. Those are
 * URLs a client outside this project holds, so they cannot move behind the API
 * prefix; they still need the same bearer token and the same refresh-on-401.
 */
export const rootApi = createApiClient({ baseUrl: '', tokens: tokenStore });
