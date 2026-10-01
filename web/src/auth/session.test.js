import { beforeEach, describe, expect, it, vi } from 'vitest';

import { auth } from '../api/resources.js';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  auth: { login: vi.fn(), me: vi.fn() },
  USER_LEVELS: { STREAMER: 0, STANDARD: 1, ADMIN: 10 },
  USER_LEVEL_LABELS: { 0: 'Streamer', 1: 'Standard', 10: 'Administrator' },
}));

/** Reloaded per test because boot status is decided at module load. */
async function loadSession() {
  vi.resetModules();
  const tokens = await import('./tokenStore.js');
  const session = await import('./session.js');
  return { ...session, ...tokens };
}

const ADMIN = { id: 1, username: 'root', user_level: 10 };

beforeEach(() => {
  localStorage.clear();
  vi.clearAllMocks();
});

describe('session', () => {
  it('boots anonymous with no stored tokens', async () => {
    const { useSession } = await loadSession();
    expect(useSession.getState().status).toBe('anonymous');
  });

  it('boots into loading when a refresh token survived the reload', async () => {
    localStorage.setItem('dollet.tokens', JSON.stringify({ access: 'a', refresh: 'r' }));
    const { useSession } = await loadSession();
    expect(useSession.getState().status).toBe('loading');
  });

  it('stores the pair and resolves the profile on login', async () => {
    auth.login.mockResolvedValue({ access: 'a', refresh: 'r' });
    auth.me.mockResolvedValue(ADMIN);

    const { useSession, tokenStore } = await loadSession();
    await useSession.getState().login('root', 'hunter2');

    expect(auth.login).toHaveBeenCalledWith('root', 'hunter2');
    expect(tokenStore.getAccess()).toBe('a');
    expect(useSession.getState()).toMatchObject({ status: 'authenticated', user: ADMIN });
    expect(useSession.getState().isAdmin()).toBe(true);
  });

  it('leaves the session anonymous when login is rejected', async () => {
    auth.login.mockRejectedValue(
      new ApiError('No active account found', { status: 401 }),
    );

    const { useSession, tokenStore } = await loadSession();
    await expect(useSession.getState().login('root', 'wrong')).rejects.toThrow(
      'No active account found',
    );

    expect(tokenStore.hasTokens()).toBe(false);
    expect(useSession.getState().status).toBe('anonymous');
  });

  it('drops to anonymous when the stored tokens are rejected on boot', async () => {
    localStorage.setItem('dollet.tokens', JSON.stringify({ access: 'a', refresh: 'r' }));
    auth.me.mockRejectedValue(new ApiError('Expired', { status: 401 }));

    const { useSession, restoreSession, tokenStore } = await loadSession();
    restoreSession();
    await vi.waitFor(() => expect(useSession.getState().status).toBe('anonymous'));

    expect(tokenStore.hasTokens()).toBe(false);
  });

  it('keeps the session when the profile call fails for a non-auth reason', async () => {
    localStorage.setItem('dollet.tokens', JSON.stringify({ access: 'a', refresh: 'r' }));
    auth.me.mockRejectedValue(new ApiError('Could not reach the server.', { status: 0 }));

    const { useSession, restoreSession, tokenStore } = await loadSession();
    restoreSession();
    await vi.waitFor(() => expect(useSession.getState().status).toBe('authenticated'));

    // Bouncing to a login page that also cannot reach the server helps nobody.
    expect(tokenStore.hasTokens()).toBe(true);
    expect(useSession.getState().user).toBeNull();
  });

  it('goes anonymous when a non-auth failure has already cleared the tokens', async () => {
    localStorage.setItem('dollet.tokens', JSON.stringify({ access: 'a', refresh: 'r' }));

    const { useSession, restoreSession, tokenStore } = await loadSession();
    // What the API client does when a refresh is rejected: clear, then throw an
    // error whose status is not 401 — a wrapped network failure, say. Reading
    // the error class instead of the store would re-assert "authenticated".
    auth.me.mockImplementation(async () => {
      tokenStore.clear();
      throw new ApiError('Could not reach the server.', { status: 0 });
    });

    restoreSession();
    await vi.waitFor(() => expect(useSession.getState().status).toBe('anonymous'));
  });

  it('goes anonymous when the API client clears the tokens', async () => {
    auth.login.mockResolvedValue({ access: 'a', refresh: 'r' });
    auth.me.mockResolvedValue(ADMIN);

    const { useSession, tokenStore } = await loadSession();
    await useSession.getState().login('root', 'hunter2');

    // This is exactly what the client does after a failed refresh.
    tokenStore.clear();

    expect(useSession.getState()).toMatchObject({ status: 'anonymous', user: null });
  });

  it('clears tokens and user on an explicit logout', async () => {
    auth.login.mockResolvedValue({ access: 'a', refresh: 'r' });
    auth.me.mockResolvedValue(ADMIN);

    const { useSession, tokenStore } = await loadSession();
    await useSession.getState().login('root', 'hunter2');
    useSession.getState().logout();

    expect(tokenStore.hasTokens()).toBe(false);
    expect(useSession.getState().status).toBe('anonymous');
  });

  it('does not call the profile endpoint when there is nothing to authenticate with', async () => {
    const { useSession, restoreSession } = await loadSession();
    restoreSession();

    expect(auth.me).not.toHaveBeenCalled();
    expect(useSession.getState().status).toBe('anonymous');
  });
});
