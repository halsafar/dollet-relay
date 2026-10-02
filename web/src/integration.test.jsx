import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { renderWithProviders } from './test-utils.jsx';

/**
 * The whole stack with only `fetch` replaced.
 *
 * Every other test in this suite mocks the API layer out, so the page tests
 * never run `client.js` and the client tests never run the session store that
 * reacts to it. These exercise the real `client.js` + `resources.js` +
 * `session.js` against a fake server, so a disagreement between them fails.
 */

/** Builds a `fetch` that answers from a routing table, recording every call. */
function fakeServer(routes) {
  const calls = [];
  const fetchImpl = vi.fn(async (url, init = {}) => {
    const method = init.method ?? 'GET';
    const path = String(url).split('?')[0];
    calls.push({ method, path, url: String(url), init });

    const handler =
      routes[`${method} ${path}`] ??
      (method === 'GET' && path === '/api/accounts/initialize-superuser/'
        ? () => jsonResponse(200, { superuser_exists: true })
        : null);
    if (!handler) {
      return jsonResponse(404, { detail: 'Not found.' });
    }
    const body =
      init.body && typeof init.body === 'string' ? JSON.parse(init.body) : null;
    return handler({ body, headers: init.headers ?? {}, calls });
  });
  return { fetchImpl, calls };
}

function jsonResponse(status, body) {
  return {
    status,
    ok: status >= 200 && status < 300,
    headers: { get: () => 'application/json' },
    text: async () => (body === undefined ? '' : JSON.stringify(body)),
  };
}

/**
 * The envelope every `/api/` list answers in.
 *
 * `count` defaults to one more than the rows given, never to their length: a
 * fixture where the two agree cannot tell a client reading `count` from one
 * reading `results.length`, and that is exactly the bug pagination hides.
 */
function list(results, count = results.length + 1, page = 1, pages = 2) {
  return { results, count, page, pages };
}

const ADMIN = {
  id: 1,
  username: 'root',
  email: null,
  user_level: 10,
  is_active: true,
  stream_limit: 0,
};

/** Imported fresh per test: the token store reads localStorage at module load. */
async function boot(route = '/') {
  vi.resetModules();
  const { App } = await import('./App.jsx');
  const { restoreSession } = await import('./auth/session.js');
  const session = await import('./auth/session.js');
  const tokens = await import('./auth/tokenStore.js');

  restoreSession();
  const view = renderWithProviders(<App />, { route });
  return { view, useSession: session.useSession, tokenStore: tokens.tokenStore };
}

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('signing in end to end', () => {
  it('exchanges credentials for tokens and loads the profile', async () => {
    const user = userEvent.setup();
    const { fetchImpl, calls } = fakeServer({
      'POST /api/accounts/token/': () =>
        jsonResponse(200, { access: 'access-1', refresh: 'refresh-1' }),
      'GET /api/accounts/users/me/': () => jsonResponse(200, ADMIN),
      'GET /api/core/version/': () => jsonResponse(200, { version: '0.1.0' }),
      'GET /api/channels/channels/': () => jsonResponse(200, list([], 6, 1, 1)),
    });
    vi.stubGlobal('fetch', fetchImpl);

    const { tokenStore } = await boot('/login');

    await user.type(await screen.findByLabelText('Username'), 'root');
    await user.type(screen.getByLabelText('Password'), 'hunter2');
    await user.click(screen.getByRole('button', { name: 'Sign in' }));

    expect(await screen.findByRole('heading', { name: 'Channels' })).toBeInTheDocument();
    expect(tokenStore.getAccess()).toBe('access-1');

    // The profile request must carry the token the login just returned.
    const profile = calls.find((call) => call.path === '/api/accounts/users/me/');
    expect(profile.init.headers.Authorization).toBe('Bearer access-1');

    // And the count badge is real data, from the real endpoint.
    expect(await screen.findByText('(6)')).toBeInTheDocument();
  });

  it('shows the server detail message on bad credentials', async () => {
    const user = userEvent.setup();
    const { fetchImpl } = fakeServer({
      'POST /api/accounts/token/': () =>
        jsonResponse(401, {
          detail: 'Authentication credentials were not provided or are invalid.',
        }),
    });
    vi.stubGlobal('fetch', fetchImpl);

    await boot('/login');

    await user.type(await screen.findByLabelText('Username'), 'root');
    await user.type(screen.getByLabelText('Password'), 'wrong');
    await user.click(screen.getByRole('button', { name: 'Sign in' }));

    expect(
      await screen.findByText(
        'Authentication credentials were not provided or are invalid.',
      ),
    ).toBeInTheDocument();
  });
});

describe('token refresh end to end', () => {
  it('refreshes an expired access token and retries transparently', async () => {
    localStorage.setItem(
      'dollet.tokens',
      JSON.stringify({ access: 'stale', refresh: 'refresh-1' }),
    );

    const { fetchImpl, calls } = fakeServer({
      'GET /api/accounts/users/me/': ({ headers }) =>
        headers.Authorization === 'Bearer access-2'
          ? jsonResponse(200, ADMIN)
          : jsonResponse(401, { detail: 'Token expired' }),
      'POST /api/accounts/token/refresh/': () =>
        jsonResponse(200, { access: 'access-2' }),
      'GET /api/core/version/': () => jsonResponse(200, { version: '0.1.0' }),
      'GET /api/channels/channels/': () => jsonResponse(200, list([], 0, 1, 1)),
    });
    vi.stubGlobal('fetch', fetchImpl);

    const { useSession, tokenStore } = await boot('/channels');

    expect(await screen.findByRole('heading', { name: 'Channels' })).toBeInTheDocument();
    await waitFor(() => expect(useSession.getState().user).toEqual(ADMIN));

    expect(tokenStore.getAccess()).toBe('access-2');
    // Rotation is not offered by this server, so the refresh token is kept.
    expect(tokenStore.getRefresh()).toBe('refresh-1');
    expect(calls.filter((call) => call.path.endsWith('/token/refresh/'))).toHaveLength(1);
  });

  it('sends a user whose refresh token is rejected to the login page', async () => {
    localStorage.setItem(
      'dollet.tokens',
      JSON.stringify({ access: 'stale', refresh: 'dead' }),
    );

    const { fetchImpl } = fakeServer({
      'GET /api/accounts/users/me/': () => jsonResponse(401, { detail: 'Token expired' }),
      'POST /api/accounts/token/refresh/': () =>
        jsonResponse(401, { detail: 'Token is invalid or expired' }),
    });
    vi.stubGlobal('fetch', fetchImpl);

    const { tokenStore } = await boot('/users');

    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
    expect(tokenStore.hasTokens()).toBe(false);
  });

  it('keeps a still-valid refresh token when the refresh endpoint 500s', async () => {
    localStorage.setItem(
      'dollet.tokens',
      JSON.stringify({ access: 'stale', refresh: 'refresh-1' }),
    );

    const { fetchImpl } = fakeServer({
      'GET /api/accounts/users/me/': () => jsonResponse(401, { detail: 'Token expired' }),
      'POST /api/accounts/token/refresh/': () => jsonResponse(500, {}),
      'GET /api/core/version/': () => jsonResponse(200, { version: '0.1.0' }),
      'GET /api/channels/channels/': () => jsonResponse(500, {}),
    });
    vi.stubGlobal('fetch', fetchImpl);

    const { useSession, tokenStore } = await boot('/channels');

    // A transient server fault must not discard a credential that may still be
    // good, and must not strand the user in a shell that claims they are signed
    // in while every request goes out unauthenticated.
    await waitFor(() => expect(useSession.getState().status).not.toBe('loading'));
    expect(tokenStore.getRefresh()).toBe('refresh-1');
    expect(useSession.getState().status).toBe('authenticated');
  });
});

describe('the channels screen end to end', () => {
  const CHANNEL = {
    id: 1,
    uuid: 'a',
    name: 'PLOV-DT',
    channel_number: 2.1,
    channel_group_id: 7,
    logo_id: null,
    tvg_id: '21300',
    epg_data_id: null,
    stream_profile_id: null,
    hidden_from_output: false,
    streams: [],
    override: null,
    effective_name: 'PLOV-DT',
    effective_channel_number: 2.1,
    effective_tvg_id: '21300',
    effective_epg_data_id: null,
    epg_name: null,
    group_name: 'Default Group',
    logo_url: null,
  };

  function routes(overrides = {}) {
    return {
      'GET /api/accounts/users/me/': () => jsonResponse(200, ADMIN),
      'GET /api/core/version/': () => jsonResponse(200, { version: '0.1.0' }),
      'GET /api/core/streamprofiles/': () =>
        jsonResponse(200, list([{ id: 1, name: 'Proxy' }])),
      'GET /api/channels/channels/': () => jsonResponse(200, list([CHANNEL])),
      'GET /api/channels/groups/': () =>
        jsonResponse(
          200,
          list([{ id: 7, name: 'Default Group', channel_count: 1, stream_count: 2 }]),
        ),
      'GET /api/channels/profiles/': () => jsonResponse(200, list([])),
      'GET /api/channels/logos/': () => jsonResponse(200, list([])),
      // 51 streams over two pages, so the pager has something to be wrong about.
      'GET /api/channels/streams/': () =>
        jsonResponse(200, list([{ id: 100, name: '1stAlrt', channel_group_id: 7 }], 51)),
      ...overrides,
    };
  }

  it('loads both panes against the real resource layer', async () => {
    localStorage.setItem('dollet.tokens', JSON.stringify({ access: 'a', refresh: 'r' }));
    const { fetchImpl, calls } = fakeServer(routes());
    vi.stubGlobal('fetch', fetchImpl);

    await boot('/channels');

    expect(await screen.findByText('PLOV-DT')).toBeInTheDocument();
    expect(await screen.findByText('1stAlrt')).toBeInTheDocument();

    // The lineup is fetched whole, and says so: without `all=true` this returns
    // the first fifty channels and a table that looks complete. (The sidebar
    // badge hits the same path with page_size=1, which is why this looks for
    // the whole URL rather than the first matching call.)
    expect(calls.map((call) => call.url)).toContain('/api/channels/channels/?all=true');

    // Streams are the opposite — always paginated, and the one list that
    // refuses `all=true` outright.
    const streams = calls.find((call) => call.path === '/api/channels/streams/');
    expect(streams.url).toContain('page_size=50');
    expect(streams.url).not.toContain('all=');
  });

  it('collapses the mount burst of 401s onto a single refresh', async () => {
    localStorage.setItem(
      'dollet.tokens',
      JSON.stringify({ access: 'stale', refresh: 'refresh-1' }),
    );

    // This screen fires six requests in parallel on mount. Before the token is
    // refreshed every one of them 401s at once, which is precisely the race the
    // single-flight refresh exists for.
    const authed = (handler) => (context) =>
      context.headers.Authorization === 'Bearer access-2'
        ? handler(context)
        : jsonResponse(401, { detail: 'Token expired' });

    const base = routes();
    const guarded = Object.fromEntries(
      Object.entries(base).map(([key, handler]) => [key, authed(handler)]),
    );

    const { fetchImpl, calls } = fakeServer({
      ...guarded,
      'POST /api/accounts/token/refresh/': () =>
        jsonResponse(200, { access: 'access-2' }),
    });
    vi.stubGlobal('fetch', fetchImpl);

    await boot('/channels');

    expect(await screen.findByText('PLOV-DT')).toBeInTheDocument();
    expect(await screen.findByText('1stAlrt')).toBeInTheDocument();

    const refreshes = calls.filter((call) => call.path.endsWith('/token/refresh/'));
    expect(refreshes).toHaveLength(1);
  });
});

describe('settings end to end', () => {
  const GROUPS = [
    {
      id: 2,
      key: 'proxy_settings',
      name: 'Proxy Settings',
      value: { ring_seconds: 15, buffering_timeout: 5 },
    },
    {
      id: 3,
      key: 'network_access',
      name: 'Network Access',
      value: { UI: '10.0.0.0/8', STREAMS: '' },
    },
  ];

  function server(onPatch) {
    return fakeServer({
      'GET /api/accounts/users/me/': () => jsonResponse(200, ADMIN),
      'GET /api/core/version/': () => jsonResponse(200, { version: '0.1.0' }),
      'GET /api/channels/channels/': () => jsonResponse(200, list([], 0, 1, 1)),
      'GET /api/core/settings/': () => jsonResponse(200, list(GROUPS)),
      'PATCH /api/core/settings/network_access/': ({ body }) => {
        onPatch(body);
        return jsonResponse(200, { ...GROUPS[1], value: body.value });
      },
    });
  }

  it('sends the whole allowlist map, so no endpoint silently loses its CIDRs', async () => {
    localStorage.setItem('dollet.tokens', JSON.stringify({ access: 'a', refresh: 'r' }));
    const onPatch = vi.fn();
    const { fetchImpl } = server(onPatch);
    vi.stubGlobal('fetch', fetchImpl);

    const user = userEvent.setup();
    await boot('/settings');

    const streams = await screen.findByRole('textbox', { name: 'Streams' });
    await user.type(streams, '192.168.1.0/24');

    const networkSave = within(
      screen.getByRole('region', { name: 'Network Access' }),
    ).getByRole('button', { name: 'Save' });
    await user.click(networkSave);

    await waitFor(() => expect(onPatch).toHaveBeenCalled());

    // `network_access` is replaced wholesale by the backend, and a missing
    // endpoint key means "allow everyone" — so dropping `UI` here would open
    // the admin interface to the internet.
    expect(onPatch.mock.calls[0][0]).toEqual({
      value: { UI: '10.0.0.0/8', STREAMS: '192.168.1.0/24' },
    });
  });
});
