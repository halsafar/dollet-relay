import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const STORAGE_KEY = 'dollet.tokens';

/**
 * The store reads localStorage at module load, so every case that cares about
 * the initial read needs a fresh module instance.
 */
async function loadStore() {
  vi.resetModules();
  return import('./tokenStore.js');
}

/** A JWT with only the claim this code reads. The signature is never checked here. */
function tokenExpiringAt(epochSeconds) {
  const payload = btoa(JSON.stringify({ exp: epochSeconds }))
    .replace(/\+/g, '-')
    .replace(/\//g, '_')
    .replace(/=+$/, '');
  return `header.${payload}.signature`;
}

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  vi.useRealTimers();
});

describe('tokenStore', () => {
  it('starts empty when nothing is stored', async () => {
    const { tokenStore } = await loadStore();
    expect(tokenStore.getAccess()).toBeNull();
    expect(tokenStore.getRefresh()).toBeNull();
    expect(tokenStore.hasTokens()).toBe(false);
  });

  it('restores a persisted pair', async () => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ access: 'a', refresh: 'r' }));
    const { tokenStore } = await loadStore();

    expect(tokenStore.getAccess()).toBe('a');
    expect(tokenStore.getRefresh()).toBe('r');
    expect(tokenStore.hasTokens()).toBe(true);
  });

  it('ignores a corrupt entry rather than throwing at import time', async () => {
    localStorage.setItem(STORAGE_KEY, '{{{');
    const { tokenStore } = await loadStore();
    expect(tokenStore.hasTokens()).toBe(false);
  });

  it('ignores a stored entry whose fields are the wrong type', async () => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ access: 42, refresh: {} }));
    const { tokenStore } = await loadStore();
    expect(tokenStore.getAccess()).toBeNull();
    expect(tokenStore.getRefresh()).toBeNull();
  });

  it('persists a written pair', async () => {
    const { tokenStore } = await loadStore();
    tokenStore.setTokens({ access: 'a', refresh: 'r' });

    expect(JSON.parse(localStorage.getItem(STORAGE_KEY))).toEqual({
      access: 'a',
      refresh: 'r',
    });
  });

  it('keeps the current refresh token when one is not supplied', async () => {
    const { tokenStore } = await loadStore();
    tokenStore.setTokens({ access: 'a', refresh: 'r' });
    tokenStore.setTokens({ access: 'a2' });

    expect(tokenStore.getAccess()).toBe('a2');
    expect(tokenStore.getRefresh()).toBe('r');
  });

  it('removes the storage entry entirely on clear', async () => {
    const { tokenStore } = await loadStore();
    tokenStore.setTokens({ access: 'a', refresh: 'r' });
    tokenStore.clear();

    expect(tokenStore.hasTokens()).toBe(false);
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('notifies subscribers when the pair changes', async () => {
    const { tokenStore } = await loadStore();
    const seen = [];
    const unsubscribe = tokenStore.subscribe((state) => seen.push(state.access));

    tokenStore.setTokens({ access: 'a', refresh: 'r' });
    tokenStore.clear();
    unsubscribe();
    tokenStore.setTokens({ access: 'ignored', refresh: 'r' });

    expect(seen).toEqual(['a', null]);
  });

  it('survives a localStorage that throws', async () => {
    const setItem = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('QuotaExceededError');
    });

    const { tokenStore } = await loadStore();
    expect(() => tokenStore.setTokens({ access: 'a', refresh: 'r' })).not.toThrow();
    expect(tokenStore.getAccess()).toBe('a');

    setItem.mockRestore();
  });
});

describe('jwtExpiry', () => {
  it('reads exp out of the payload as milliseconds', async () => {
    const { jwtExpiry } = await loadStore();
    expect(jwtExpiry(tokenExpiringAt(1_700_000_000))).toBe(1_700_000_000_000);
  });

  it('returns null for anything it cannot read', async () => {
    const { jwtExpiry } = await loadStore();
    expect(jwtExpiry(null)).toBeNull();
    expect(jwtExpiry('not-a-jwt')).toBeNull();
    expect(jwtExpiry('a.!!!!.c')).toBeNull();
    expect(jwtExpiry(`header.${btoa('{"sub":1}')}.sig`)).toBeNull();
  });
});

describe('isAccessExpired', () => {
  it('is true when there is no token at all', async () => {
    const { tokenStore } = await loadStore();
    expect(tokenStore.isAccessExpired()).toBe(true);
  });

  it('is false for a token comfortably in the future', async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-01-01T00:00:00Z'));

    const { tokenStore } = await loadStore();
    tokenStore.setTokens({
      access: tokenExpiringAt(Date.now() / 1000 + 600),
      refresh: 'r',
    });

    expect(tokenStore.isAccessExpired()).toBe(false);
  });

  it('is true inside the skew window, so the caller refreshes first', async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-01-01T00:00:00Z'));

    const { tokenStore } = await loadStore();
    tokenStore.setTokens({
      access: tokenExpiringAt(Date.now() / 1000 + 10),
      refresh: 'r',
    });

    expect(tokenStore.isAccessExpired()).toBe(true);
    expect(tokenStore.isAccessExpired(0)).toBe(false);
  });

  it('treats an unreadable token as expired', async () => {
    const { tokenStore } = await loadStore();
    tokenStore.setTokens({ access: 'opaque', refresh: 'r' });
    expect(tokenStore.isAccessExpired()).toBe(true);
  });
});
