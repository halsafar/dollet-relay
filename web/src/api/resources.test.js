import { beforeEach, describe, expect, it, vi } from 'vitest';

import { api } from './client.js';
import { asSettingGroups, channels, stats, streams, users } from './resources.js';

vi.mock('./client.js', () => ({
  api: { get: vi.fn(), post: vi.fn(), patch: vi.fn(), delete: vi.fn(), request: vi.fn() },
  rootApi: { get: vi.fn(), post: vi.fn() },
}));

beforeEach(() => {
  vi.clearAllMocks();
});

/**
 * How a list body is read.
 *
 * A reader that returns `[]` for a body it does not recognise makes a renamed
 * field and an empty collection the same screen: a table with nothing in it
 * and no error anywhere. The interesting assertions are the ones about
 * throwing, not the ones about unwrapping.
 */
describe('reading the list envelope', () => {
  it('unwraps the results', async () => {
    api.get.mockResolvedValue({ results: [{ id: 1 }], count: 1, page: 1, pages: 1 });
    await expect(users.list()).resolves.toEqual([{ id: 1 }]);
  });

  it('treats an empty collection as a real answer', async () => {
    api.get.mockResolvedValue({ results: [], count: 0, page: 1, pages: 1 });
    await expect(users.list()).resolves.toEqual([]);
  });

  it('refuses a bare array where an envelope is required, naming the endpoint', async () => {
    api.get.mockResolvedValue([{ id: 1 }]);
    await expect(users.list()).rejects.toThrow(
      '/api/accounts/users/: expected {results, count, page, pages}, got an array.',
    );
  });

  it('refuses a body it does not recognise rather than calling it empty', async () => {
    api.get.mockResolvedValue({ detail: 'Not found.' });
    await expect(users.list()).rejects.toThrow(
      '/api/accounts/users/: expected {results, count, page, pages}, ' +
        'got an object with {detail}.',
    );

    api.get.mockResolvedValue(null);
    await expect(users.list()).rejects.toThrow('got null.');
  });

  it('keeps the paging counters, which is the whole reason for the envelope', async () => {
    // `count` is the collection; `results` is this page of it. A pager built
    // from `results.length` shows one page and calls it the end.
    api.get.mockResolvedValue({ results: [{ id: 1 }], count: 137, page: 2, pages: 3 });

    await expect(streams.list({ page: 2 })).resolves.toEqual({
      results: [{ id: 1 }],
      count: 137,
      page: 2,
      pages: 3,
    });
  });

  it('reads the count from the envelope for the sidebar badge', async () => {
    api.get.mockResolvedValue({ results: [{ id: 1 }], count: 49, page: 1, pages: 49 });
    await expect(channels.count()).resolves.toBe(49);
  });

  it('reports no count rather than a wrong one when the shape is unknown', async () => {
    api.get.mockResolvedValue([{ id: 1 }, { id: 2 }]);
    // Not 2. The badge is allowed to be absent; it is not allowed to say "2"
    // about a lineup of forty-nine.
    await expect(channels.count()).resolves.toBeNull();
  });
});

describe('the bodies that are still a bare array', () => {
  it('reads stats as a snapshot rather than a paged collection', async () => {
    api.get.mockResolvedValue([{ channel: 'a' }]);
    await expect(stats.get()).resolves.toEqual([{ channel: 'a' }]);
  });

  it('would also read it enveloped, so the conversion is not a blank page', async () => {
    api.get.mockResolvedValue({ results: [{ channel: 'a' }], count: 1 });
    await expect(stats.get()).resolves.toEqual([{ channel: 'a' }]);
  });

  it('still refuses anything that is neither', async () => {
    api.get.mockResolvedValue(null);
    await expect(stats.get()).rejects.toThrow(
      '/proxy/stats/: expected a JSON array, got null.',
    );
  });
});

describe('asSettingGroups', () => {
  const envelope = (results) => ({ results, count: results.length, page: 1, pages: 1 });

  it('normalises the row-per-group list form', () => {
    expect(
      asSettingGroups(
        envelope([
          { key: 'proxy_settings', name: 'Proxy Settings', value: { ring_seconds: 15 } },
        ]),
      ),
    ).toEqual([
      { key: 'proxy_settings', name: 'Proxy Settings', value: { ring_seconds: 15 } },
    ]);
  });

  it('falls back to the key when the server sends no display name', () => {
    const [group] = asSettingGroups(envelope([{ key: 'epg_settings', value: {} }]));
    expect(group.name).toBe('epg_settings');
  });

  it('does not invent groups out of a bare key-to-value map', () => {
    // Nothing sends it, and absorbing a shape nobody produces means a genuinely
    // wrong body renders as a settings page full of sections that do not exist.
    expect(() =>
      asSettingGroups({ system_settings: { max_system_events: 100 } }),
    ).toThrow('/api/core/settings/');
  });

  it('yields nothing for an empty group list', () => {
    expect(asSettingGroups(envelope([]))).toEqual([]);
  });
});
