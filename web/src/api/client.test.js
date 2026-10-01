import { beforeEach, describe, expect, it, vi } from 'vitest';

import { createApiClient } from './client.js';
import { ApiError } from './errors.js';

/** A minimal stand-in for the token store, so tests never touch localStorage. */
function fakeTokens({ access = 'access-1', refresh = 'refresh-1' } = {}) {
  const state = { access, refresh };
  return {
    state,
    getAccess: () => state.access,
    getRefresh: () => state.refresh,
    setTokens: vi.fn(({ access: next, refresh: nextRefresh }) => {
      state.access = next;
      if (nextRefresh !== undefined) state.refresh = nextRefresh;
    }),
    clear: vi.fn(() => {
      state.access = null;
      state.refresh = null;
    }),
  };
}

function jsonResponse(status, body, { contentType = 'application/json' } = {}) {
  return {
    status,
    ok: status >= 200 && status < 300,
    headers: { get: () => contentType },
    text: async () => (body === undefined ? '' : JSON.stringify(body)),
  };
}

describe('api client', () => {
  let tokens;

  beforeEach(() => {
    tokens = fakeTokens();
  });

  it('sends the access token and parses a JSON body', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(200, { id: 1 }));
    const api = createApiClient({ tokens, fetchImpl });

    await expect(api.get('/accounts/users/me/')).resolves.toEqual({ id: 1 });

    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe('/api/accounts/users/me/');
    expect(init.headers.Authorization).toBe('Bearer access-1');
    expect(init.method).toBe('GET');
  });

  it('omits the token when auth is explicitly disabled', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(200, {}));
    const api = createApiClient({ tokens, fetchImpl });

    await api.post('/accounts/token/', { username: 'a' }, { auth: false });

    expect(fetchImpl.mock.calls[0][1].headers.Authorization).toBeUndefined();
  });

  it('JSON-encodes a body and sets the content type', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(200, {}));
    const api = createApiClient({ tokens, fetchImpl });

    await api.patch('/core/settings/proxy_settings/', { ring_seconds: 30 });

    const [, init] = fetchImpl.mock.calls[0];
    expect(init.method).toBe('PATCH');
    expect(init.body).toBe('{"ring_seconds":30}');
    expect(init.headers['Content-Type']).toBe('application/json');
  });

  it('leaves FormData alone so fetch can set the multipart boundary', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(200, {}));
    const api = createApiClient({ tokens, fetchImpl });
    const form = new FormData();

    await api.post('/core/logos/', form);

    const [, init] = fetchImpl.mock.calls[0];
    expect(init.body).toBe(form);
    expect(init.headers['Content-Type']).toBeUndefined();
  });

  it('builds a query string and drops empty values', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(200, []));
    const api = createApiClient({ tokens, fetchImpl });

    await api.get('/channels/', {
      query: { search: 'hd', page: 2, group: null, name: '' },
    });

    expect(fetchImpl.mock.calls[0][0]).toBe('/api/channels/?search=hd&page=2');
  });

  it('returns null for 204 rather than trying to parse a body', async () => {
    const fetchImpl = vi.fn().mockResolvedValue({
      status: 204,
      ok: true,
      headers: { get: () => null },
      text: async () => {
        throw new Error('should not be read');
      },
    });
    const api = createApiClient({ tokens, fetchImpl });

    await expect(api.delete('/accounts/users/3/')).resolves.toBeNull();
  });

  it('normalises a non-2xx response into an ApiError', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(jsonResponse(400, { detail: 'name is required' }));
    const api = createApiClient({ tokens, fetchImpl });

    const error = await api.get('/channels/').catch((failure) => failure);
    expect(error).toBeInstanceOf(ApiError);
    expect(error.status).toBe(400);
    expect(error.message).toBe('name is required');
  });

  it('turns a thrown fetch into a network ApiError', async () => {
    const fetchImpl = vi.fn().mockRejectedValue(new TypeError('Failed to fetch'));
    const api = createApiClient({ tokens, fetchImpl });

    const error = await api.get('/channels/').catch((failure) => failure);
    expect(error.isNetworkError).toBe(true);
    expect(error.message).toBe('Could not reach the server.');
  });

  it('propagates an abort rather than disguising it as a network failure', async () => {
    const abort = new Error('aborted');
    abort.name = 'AbortError';
    const fetchImpl = vi.fn().mockRejectedValue(abort);
    const api = createApiClient({ tokens, fetchImpl });

    await expect(api.get('/channels/')).rejects.toBe(abort);
  });

  it('reports malformed JSON instead of throwing a SyntaxError at the caller', async () => {
    const fetchImpl = vi.fn().mockResolvedValue({
      status: 200,
      ok: true,
      headers: { get: () => 'application/json' },
      text: async () => '{ not json',
    });
    const api = createApiClient({ tokens, fetchImpl });

    const error = await api.get('/channels/').catch((failure) => failure);
    expect(error).toBeInstanceOf(ApiError);
    expect(error.message).toBe('The server returned malformed JSON.');
  });
});

describe('refresh on 401', () => {
  let tokens;

  beforeEach(() => {
    tokens = fakeTokens();
  });

  it('refreshes once and retries the original request', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, { detail: 'Token expired' }))
      .mockResolvedValueOnce(jsonResponse(200, { access: 'access-2' }))
      .mockResolvedValueOnce(jsonResponse(200, { id: 7 }));

    const api = createApiClient({ tokens, fetchImpl });

    await expect(api.get('/accounts/users/me/')).resolves.toEqual({ id: 7 });
    expect(fetchImpl).toHaveBeenCalledTimes(3);

    expect(fetchImpl.mock.calls[1][0]).toBe('/api/accounts/token/refresh/');
    expect(fetchImpl.mock.calls[1][1].body).toBe('{"refresh":"refresh-1"}');
    // The retry must carry the new token, not the one that just failed.
    expect(fetchImpl.mock.calls[2][1].headers.Authorization).toBe('Bearer access-2');
  });

  it('keeps the existing refresh token when the server does not rotate it', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockResolvedValueOnce(jsonResponse(200, { access: 'access-2' }))
      .mockResolvedValueOnce(jsonResponse(200, {}));

    const api = createApiClient({ tokens, fetchImpl });
    await api.get('/channels/');

    expect(tokens.setTokens).toHaveBeenCalledWith({
      access: 'access-2',
      refresh: 'refresh-1',
    });
  });

  it('stores a rotated refresh token', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockResolvedValueOnce(
        jsonResponse(200, { access: 'access-2', refresh: 'refresh-2' }),
      )
      .mockResolvedValueOnce(jsonResponse(200, {}));

    const api = createApiClient({ tokens, fetchImpl });
    await api.get('/channels/');

    expect(tokens.state.refresh).toBe('refresh-2');
  });

  it('collapses concurrent 401s onto a single refresh', async () => {
    let refreshCalls = 0;
    const fetchImpl = vi.fn(async (url, init) => {
      if (url.endsWith('/token/refresh/')) {
        refreshCalls += 1;
        // Yield, so both callers are waiting when the refresh resolves.
        await Promise.resolve();
        return jsonResponse(200, { access: 'access-2' });
      }
      return init.headers.Authorization === 'Bearer access-2'
        ? jsonResponse(200, { ok: true })
        : jsonResponse(401, {});
    });

    const api = createApiClient({ tokens, fetchImpl });

    const results = await Promise.all([
      api.get('/channels/'),
      api.get('/streams/'),
      api.get('/accounts/users/'),
    ]);

    expect(results).toEqual([{ ok: true }, { ok: true }, { ok: true }]);
    expect(refreshCalls).toBe(1);
  });

  it('clears the tokens and gives up when the refresh itself is rejected', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockResolvedValueOnce(jsonResponse(401, { detail: 'Token is invalid' }));

    const api = createApiClient({ tokens, fetchImpl });

    const error = await api.get('/channels/').catch((failure) => failure);
    expect(error.status).toBe(401);
    expect(error.message).toBe('Token is invalid');
    expect(tokens.clear).toHaveBeenCalled();
    expect(fetchImpl).toHaveBeenCalledTimes(2);
  });

  it('does not retry when there is no refresh token to use', async () => {
    tokens = fakeTokens({ refresh: null });
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(401, {}));
    const api = createApiClient({ tokens, fetchImpl });

    await expect(api.get('/channels/')).rejects.toBeInstanceOf(ApiError);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    expect(tokens.clear).toHaveBeenCalled();
  });

  it('clears the tokens when the retry is also rejected, without looping', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockResolvedValueOnce(jsonResponse(200, { access: 'access-2' }))
      .mockResolvedValueOnce(jsonResponse(401, { detail: 'Still no.' }));

    const api = createApiClient({ tokens, fetchImpl });

    await expect(api.get('/channels/')).rejects.toMatchObject({ status: 401 });
    expect(fetchImpl).toHaveBeenCalledTimes(3);
    expect(tokens.clear).toHaveBeenCalled();
  });

  it('does not try to refresh a failing refresh request', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(401, {}));
    const api = createApiClient({ tokens, fetchImpl });

    await expect(
      api.post('/accounts/token/refresh/', { refresh: 'x' }),
    ).rejects.toBeInstanceOf(ApiError);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
  });

  it('rejects a refresh response that carries no access token', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockResolvedValueOnce(jsonResponse(200, { nothing: true }));

    const api = createApiClient({ tokens, fetchImpl });

    const error = await api.get('/channels/').catch((failure) => failure);
    expect(error.message).toBe('The server returned no access token.');
    // A malformed success is a server bug, not proof the credential is dead.
    expect(tokens.clear).not.toHaveBeenCalled();
  });

  it('keeps the refresh token when the refresh endpoint fails with a 5xx', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockResolvedValueOnce(jsonResponse(500, { detail: 'Internal server error.' }));

    const api = createApiClient({ tokens, fetchImpl });

    const error = await api.get('/channels/').catch((failure) => failure);
    expect(error.status).toBe(500);
    // Throwing away a still-valid refresh token over a transient fault turns a
    // blip into a forced re-login.
    expect(tokens.clear).not.toHaveBeenCalled();
    expect(tokens.state.refresh).toBe('refresh-1');
  });

  it('keeps the refresh token when the refresh request cannot reach the server', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockRejectedValueOnce(new TypeError('Failed to fetch'));

    const api = createApiClient({ tokens, fetchImpl });

    const error = await api.get('/channels/').catch((failure) => failure);
    expect(error.isNetworkError).toBe(true);
    expect(tokens.clear).not.toHaveBeenCalled();
  });

  it('allows a fresh refresh after a previous one failed', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockResolvedValueOnce(jsonResponse(500, {}))
      .mockResolvedValueOnce(jsonResponse(401, {}))
      .mockResolvedValueOnce(jsonResponse(200, { access: 'access-2' }))
      .mockResolvedValueOnce(jsonResponse(200, { ok: true }));

    const api = createApiClient({ tokens, fetchImpl });

    await expect(api.get('/channels/')).rejects.toBeInstanceOf(ApiError);
    tokens.state.access = 'access-1';
    tokens.state.refresh = 'refresh-1';

    // The in-flight promise must have been released, or this hangs on a
    // settled promise that will never produce a token.
    await expect(api.get('/channels/')).resolves.toEqual({ ok: true });
  });
});
