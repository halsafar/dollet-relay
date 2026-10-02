import { beforeEach, describe, expect, it, vi } from 'vitest';

import { api, rootApi } from './client.js';
import {
  auth,
  backups,
  channelGroups,
  channelProfiles,
  channels,
  fetchVersion,
  logos,
  settings,
  epgSources,
  guide,
  jobs,
  m3uAccounts,
  notifications,
  outputProfiles,
  stats,
  streamProfiles,
  streams,
  systemEvents,
  userAgents,
  users,
} from './resources.js';

vi.mock('./client.js', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    patch: vi.fn(),
    delete: vi.fn(),
    request: vi.fn(),
  },
  rootApi: { get: vi.fn(), post: vi.fn() },
}));

/**
 * Pins the paths and methods this SPA depends on.
 *
 * A drift between these and the routes the server registers is the most likely
 * way this frontend breaks; asserting them here turns that into a failing test
 * rather than a 404 at runtime.
 */
beforeEach(() => {
  vi.clearAllMocks();
  // The envelope, because every `/api/` list answers in one and the resource
  // layer refuses anything else. The counts are deliberately not the lengths:
  // a fixture where they agree cannot catch a pager reading the wrong field.
  api.get.mockResolvedValue({ results: [], count: 0, page: 1, pages: 1 });
  api.post.mockResolvedValue({});
  api.patch.mockResolvedValue({});
  api.delete.mockResolvedValue(null);
  api.request.mockResolvedValue([]);
  rootApi.get.mockResolvedValue([]);
  rootApi.post.mockResolvedValue({});
});

describe('auth endpoints', () => {
  it('posts credentials to the token endpoint without a bearer token', async () => {
    await auth.login('root', 'hunter2');
    expect(api.post).toHaveBeenCalledWith(
      '/accounts/token/',
      { username: 'root', password: 'hunter2' },
      { auth: false },
    );
  });

  it('reads the current profile from the users collection', async () => {
    await auth.me();
    expect(api.get).toHaveBeenCalledWith('/accounts/users/me/');
  });
});

describe('user endpoints', () => {
  it('lists, creates, updates and deletes at the documented paths', async () => {
    await users.list();
    expect(api.get).toHaveBeenCalledWith('/accounts/users/');

    await users.create({ username: 'a' });
    expect(api.post).toHaveBeenCalledWith('/accounts/users/', { username: 'a' });

    await users.update(4, { is_active: false });
    expect(api.patch).toHaveBeenCalledWith('/accounts/users/4/', { is_active: false });

    await users.remove(4);
    expect(api.delete).toHaveBeenCalledWith('/accounts/users/4/');
  });
});

describe('channel endpoints', () => {
  it('asks for the whole lineup explicitly', async () => {
    await channels.list();
    // Without `all`, this returns the first fifty channels and a table that
    // looks complete.
    expect(api.get).toHaveBeenCalledWith('/channels/channels/', {
      query: { all: 'true' },
    });
  });

  it('asks for a single row when it only wants the count', async () => {
    api.get.mockResolvedValue({ count: 49, results: [{ id: 1 }], page: 1, pages: 49 });
    await expect(channels.count()).resolves.toBe(49);
    expect(api.get).toHaveBeenCalledWith('/channels/channels/', {
      query: { page_size: 1 },
    });
  });

  it('reports no count rather than throwing when the request fails', async () => {
    api.get.mockRejectedValue(new Error('offline'));
    await expect(channels.count()).resolves.toBeNull();
  });

  it('creates, updates and deletes at the documented paths', async () => {
    await channels.create({ name: 'New' });
    expect(api.post).toHaveBeenCalledWith('/channels/channels/', { name: 'New' });

    await channels.update(4, { name: 'Renamed' });
    expect(api.patch).toHaveBeenCalledWith('/channels/channels/4/', { name: 'Renamed' });

    await channels.remove(4);
    expect(api.delete).toHaveBeenCalledWith('/channels/channels/4/');

    await channels.bulkDelete([1, 2]);
    expect(api.post).toHaveBeenCalledWith('/channels/channels/bulk-delete/', {
      ids: [1, 2],
    });
  });

  it('reads and replaces the failover list', async () => {
    await channels.streams(4);
    expect(api.get).toHaveBeenCalledWith('/channels/channels/4/streams/');

    api.request.mockResolvedValue([]);
    rootApi.get.mockResolvedValue([]);
    rootApi.post.mockResolvedValue({});
    await channels.setStreams(4, [9, 8]);
    expect(api.request).toHaveBeenCalledWith('/channels/channels/4/streams/', {
      method: 'PUT',
      body: { ids: [9, 8] },
    });
  });
});

describe('group, profile and logo endpoints', () => {
  it('lists groups', async () => {
    await channelGroups.list();
    expect(api.get).toHaveBeenCalledWith('/channels/groups/');
  });

  it('creates, updates, removes and renumbers a group', async () => {
    await channelGroups.create({ name: 'Kids', number_start: 700 });
    expect(api.post).toHaveBeenCalledWith('/channels/groups/', {
      name: 'Kids',
      number_start: 700,
    });

    await channelGroups.update(7, { number_end: null });
    expect(api.patch).toHaveBeenCalledWith('/channels/groups/7/', { number_end: null });

    await channelGroups.remove(7);
    expect(api.delete).toHaveBeenCalledWith('/channels/groups/7/');

    // The range is the group's own, so the call carries only the order to
    // walk it in — and nothing at all when that is the lineup's own.
    await channelGroups.renumber(7);
    expect(api.post).toHaveBeenCalledWith('/channels/groups/7/renumber/', undefined, {
      query: { order: undefined },
    });

    await channelGroups.renumber(7, 'guide');
    expect(api.post).toHaveBeenCalledWith('/channels/groups/7/renumber/', undefined, {
      query: { order: 'guide' },
    });
  });

  it('plans and applies ranges and a lineup-wide renumber', async () => {
    await channelGroups.planRanges([9, 8]);
    expect(api.post).toHaveBeenCalledWith('/channels/groups/plan-ranges/', {
      order: [9, 8],
    });

    await channelGroups.assignRanges([{ id: 9, number_start: 1000, number_end: 1999 }]);
    expect(api.post).toHaveBeenCalledWith('/channels/groups/assign-ranges/', {
      ranges: [{ id: 9, number_start: 1000, number_end: 1999 }],
    });

    await channelGroups.planRenumber('name');
    expect(api.post).toHaveBeenCalledWith('/channels/groups/plan-renumber/', undefined, {
      query: { order: 'name' },
    });

    await channelGroups.renumberAll('name');
    expect(api.post).toHaveBeenCalledWith('/channels/groups/renumber-all/', undefined, {
      query: { order: 'name' },
    });
  });

  it('lists profiles and sets membership in bulk', async () => {
    await channelProfiles.list();
    expect(api.get).toHaveBeenCalledWith('/channels/profiles/');

    await channelProfiles.setMembership(2, [5, 6], true);
    expect(api.post).toHaveBeenCalledWith('/channels/profiles/2/channels/bulk-update/', {
      channel_ids: [5, 6],
      enabled: true,
    });
  });

  it('asks for every logo at once, which the picker needs', async () => {
    await logos.all();
    expect(api.get).toHaveBeenCalledWith('/channels/logos/', {
      query: { all: 'true' },
    });
  });

  it('pages the logo manager on the server', async () => {
    api.get.mockResolvedValue({ count: 120, results: [{ id: 9 }], page: 3, pages: 5 });

    const page = await logos.list({
      page: 3,
      pageSize: 25,
      search: 'kaz',
      ordering: '-name',
    });

    expect(api.get).toHaveBeenCalledWith('/channels/logos/', {
      query: { page: 3, page_size: 25, search: 'kaz', ordering: '-name' },
    });
    expect(page).toEqual({ count: 120, results: [{ id: 9 }], page: 3, pages: 5 });
  });

  it('creates, renames, deletes and cleans up logos', async () => {
    await logos.create('KRU', 'http://logos.test/kru.png');
    expect(api.post).toHaveBeenCalledWith('/channels/logos/', {
      name: 'KRU',
      url: 'http://logos.test/kru.png',
    });

    await logos.update(4, { name: 'KRU HD' });
    expect(api.patch).toHaveBeenCalledWith('/channels/logos/4/', { name: 'KRU HD' });

    await logos.remove(4);
    expect(api.delete).toHaveBeenCalledWith('/channels/logos/4/');

    await logos.bulkDelete([1, 2]);
    expect(api.post).toHaveBeenCalledWith('/channels/logos/bulk-delete/', {
      ids: [1, 2],
    });

    await logos.cleanup();
    expect(api.post).toHaveBeenCalledWith('/channels/logos/cleanup/');
  });

  it('lists stream profiles from the core app', async () => {
    await streamProfiles.list();
    expect(api.get).toHaveBeenCalledWith('/core/streamprofiles/');
  });

  it('lists the user agents and output profiles the Settings page names', async () => {
    await userAgents.list();
    expect(api.get).toHaveBeenCalledWith('/core/useragents/');

    await outputProfiles.list();
    expect(api.get).toHaveBeenCalledWith('/core/outputprofiles/');
  });
});

describe('stream endpoints', () => {
  it('sends paging, search, group and ordering the server understands', async () => {
    api.get.mockResolvedValue({ count: 51, results: [{ id: 4 }], page: 2, pages: 3 });

    const page = await streams.list({
      page: 2,
      pageSize: 25,
      search: 'vrix',
      channelGroup: 7,
      ordering: '-name',
    });

    expect(api.get).toHaveBeenCalledWith('/channels/streams/', {
      query: {
        page: 2,
        page_size: 25,
        search: 'vrix',
        channel_group: 7,
        ordering: '-name',
      },
    });
    expect(page).toEqual({ count: 51, results: [{ id: 4 }], page: 2, pages: 3 });
  });

  it('never asks this one for everything, because it is the list that refuses', async () => {
    // `?all=true` here is a 400. There is no parameter to send it, which is the
    // only way to be sure no screen ever does.
    await streams.list();
    const [, options] = api.get.mock.calls.at(-1);
    expect(options.query.all).toBeUndefined();
    expect(options.query.page).toBe(1);
  });

  it('bulk-deletes by id', async () => {
    await streams.bulkDelete([3, 4]);
    expect(api.post).toHaveBeenCalledWith('/channels/streams/bulk-delete/', {
      ids: [3, 4],
    });
  });
});

describe('version endpoint', () => {
  it('reads the version without a token', async () => {
    api.get.mockResolvedValue({ version: '0.1.0' });
    await expect(fetchVersion()).resolves.toBe('0.1.0');
    expect(api.get).toHaveBeenCalledWith('/core/version/', { auth: false });
  });

  it('swallows a failure, because it only feeds a label', async () => {
    api.get.mockRejectedValue(new Error('offline'));
    await expect(fetchVersion()).resolves.toBeNull();
  });

  it('yields null when the body carries no version', async () => {
    api.get.mockResolvedValue({});
    await expect(fetchVersion()).resolves.toBeNull();
  });
});

describe('settings endpoints', () => {
  it('lists groups and patches one by key', async () => {
    await settings.list();
    expect(api.get).toHaveBeenCalledWith('/core/settings/');

    await settings.update('proxy_settings', { ring_seconds: 30 });
    // Wrapped in `value`: the handler's body type has one writable field.
    expect(api.patch).toHaveBeenCalledWith('/core/settings/proxy_settings/', {
      value: { ring_seconds: 30 },
    });
  });
});

describe('session control', () => {
  it('reads live stats through the /api prefix', async () => {
    // Admin-only, and called by nothing outside this project, so it sits
    // under /api with every other endpoint of that kind. Only the playback
    // URL `/proxy/ts/stream/*` stays at the root.
    await stats.get();
    expect(api.get).toHaveBeenCalledWith('/proxy/stats/');
    expect(rootApi.get).not.toHaveBeenCalledWith('/proxy/stats/');
  });
});

describe('system events', () => {
  it('asks for a bounded number of events', async () => {
    await systemEvents.list(50);
    expect(api.get).toHaveBeenCalledWith('/core/system-events/', {
      query: { limit: 50 },
    });
  });

  it('defaults the limit rather than asking for everything', async () => {
    await systemEvents.list();
    expect(api.get).toHaveBeenCalledWith('/core/system-events/', {
      query: { limit: 100 },
    });
  });
});

describe('notification endpoints', () => {
  it('lists, acknowledges and deletes at the documented paths', async () => {
    await notifications.list();
    expect(api.get).toHaveBeenCalledWith('/notifications/');

    await notifications.acknowledge(4);
    expect(api.post).toHaveBeenCalledWith('/notifications/4/acknowledge/');

    await notifications.acknowledgeAll();
    expect(api.post).toHaveBeenCalledWith('/notifications/acknowledge-all/');

    await notifications.remove(4);
    expect(api.delete).toHaveBeenCalledWith('/notifications/4/');
  });

  it('reads the badge from the count endpoint rather than from the list', async () => {
    api.get.mockResolvedValue({ unacknowledged: 3 });

    expect(await notifications.count()).toBe(3);
    expect(api.get).toHaveBeenCalledWith('/notifications/count/');
    expect(api.get).not.toHaveBeenCalledWith('/notifications/');
  });

  it('answers null rather than throwing when the count cannot be had', async () => {
    // The sidebar renders on every screen, so a number it cannot fetch must
    // not take the whole shell down with it.
    api.get.mockRejectedValue(new Error('offline'));
    expect(await notifications.count()).toBeNull();

    api.get.mockResolvedValue({});
    expect(await notifications.count()).toBeNull();
  });
});

describe('M3U endpoints', () => {
  it('covers the account collection and its refresh triggers', async () => {
    await m3uAccounts.list();
    expect(api.get).toHaveBeenCalledWith('/m3u/accounts/');

    await m3uAccounts.create({ name: 'a' });
    expect(api.post).toHaveBeenCalledWith('/m3u/accounts/', { name: 'a' });

    await m3uAccounts.update(2, { is_active: false });
    expect(api.patch).toHaveBeenCalledWith('/m3u/accounts/2/', { is_active: false });

    await m3uAccounts.remove(2);
    expect(api.delete).toHaveBeenCalledWith('/m3u/accounts/2/');

    await m3uAccounts.refresh(2);
    expect(api.post).toHaveBeenCalledWith('/m3u/refresh/2/');

    await m3uAccounts.refreshAll();
    expect(api.post).toHaveBeenCalledWith('/m3u/refresh/');
  });

  it('covers the per-account sub-resources', async () => {
    await m3uAccounts.profiles(2);
    expect(api.get).toHaveBeenCalledWith('/m3u/accounts/2/profiles/');

    await m3uAccounts.createProfile(2, { name: 'p' });
    expect(api.post).toHaveBeenCalledWith('/m3u/accounts/2/profiles/', { name: 'p' });

    await m3uAccounts.updateProfile(2, 5, { name: 'q' });
    expect(api.patch).toHaveBeenCalledWith('/m3u/accounts/2/profiles/5/', { name: 'q' });

    await m3uAccounts.removeProfile(2, 5);
    expect(api.delete).toHaveBeenCalledWith('/m3u/accounts/2/profiles/5/');

    await m3uAccounts.filters(2);
    expect(api.get).toHaveBeenCalledWith('/m3u/accounts/2/filters/');

    await m3uAccounts.createFilter(2, { regex_pattern: 'x' });
    expect(api.post).toHaveBeenCalledWith('/m3u/accounts/2/filters/', {
      regex_pattern: 'x',
    });

    await m3uAccounts.removeFilter(2, 6);
    expect(api.delete).toHaveBeenCalledWith('/m3u/accounts/2/filters/6/');

    await m3uAccounts.groups(2);
    expect(api.get).toHaveBeenCalledWith('/m3u/accounts/2/groups/');

    await m3uAccounts.setGroup(2, { channel_group: 7, enabled: false });
    expect(api.post).toHaveBeenCalledWith('/m3u/accounts/2/groups/', {
      channel_group: 7,
      enabled: false,
    });
  });
});

describe('EPG endpoints', () => {
  it('covers the source collection and its refresh triggers', async () => {
    await epgSources.list();
    expect(api.get).toHaveBeenCalledWith('/epg/sources/');

    await epgSources.create({ name: 'x' });
    expect(api.post).toHaveBeenCalledWith('/epg/sources/', { name: 'x' });

    await epgSources.update(1, { priority: 2 });
    expect(api.patch).toHaveBeenCalledWith('/epg/sources/1/', { priority: 2 });

    await epgSources.remove(1);
    expect(api.delete).toHaveBeenCalledWith('/epg/sources/1/');

    // Spelled exactly like M3U's: /{resource}/refresh/ and /{resource}/refresh/{id}/.
    await epgSources.refresh(1);
    expect(api.post).toHaveBeenCalledWith('/epg/refresh/1/');

    await epgSources.refreshAll();
    expect(api.post).toHaveBeenCalledWith('/epg/refresh/');
  });
});

describe('job endpoints', () => {
  it('lists jobs and cancels one by key', async () => {
    await jobs.list();
    expect(api.get).toHaveBeenCalledWith('/core/jobs/');

    await jobs.cancel('m3u_refresh:2');
    expect(api.post).toHaveBeenCalledWith('/core/jobs/m3u_refresh%3A2/cancel/');
  });

  it('escapes a key that would otherwise change the path', async () => {
    await jobs.cancel('weird/key');
    expect(api.post).toHaveBeenCalledWith('/core/jobs/weird%2Fkey/cancel/');
  });
});

describe('backup endpoints', () => {
  const NAME = 'dollet-backup-20261001-090807-manual.zip';

  it('lists, takes, restores and deletes by file name', async () => {
    await backups.list();
    expect(api.get).toHaveBeenCalledWith('/core/backups/');

    await backups.create();
    expect(api.post).toHaveBeenCalledWith('/core/backups/');

    await backups.restore(NAME);
    expect(api.post).toHaveBeenCalledWith(`/core/backups/${NAME}/restore/`);

    await backups.remove(NAME);
    expect(api.delete).toHaveBeenCalledWith(`/core/backups/${NAME}/`);
  });

  it('downloads as a file rather than as text', async () => {
    await backups.download(NAME);
    expect(api.get).toHaveBeenCalledWith(`/core/backups/${NAME}/download/`, {
      blob: true,
    });
  });

  it('uploads the file as the body, not wrapped in a form', async () => {
    const file = new Blob(['PK'], { type: 'application/zip' });
    await backups.upload(file);
    expect(api.post).toHaveBeenCalledWith('/core/backups/upload/', file);
  });
});

describe('stop endpoints', () => {
  it('stops a whole channel by uuid', async () => {
    await stats.stopChannel('11111111-1111-1111-1111-111111111111');
    expect(api.post).toHaveBeenCalledWith(
      '/proxy/ts/stop/11111111-1111-1111-1111-111111111111',
    );
  });

  it('evicts one client, carrying the id in the query string', async () => {
    await stats.stopClient('11111111-1111-1111-1111-111111111111', 'client-9');
    expect(api.post).toHaveBeenCalledWith(
      '/proxy/ts/stop_client/11111111-1111-1111-1111-111111111111',
      undefined,
      { query: { client_id: 'client-9' } },
    );
  });
});

describe('guide endpoints', () => {
  it('asks for one window rather than one request per channel', async () => {
    rootApi.get.mockResolvedValue(null);
    api.get.mockResolvedValue({ start: 'a', end: 'b', channels: [{ id: 1 }] });

    const from = new Date('2026-02-10T18:00:00Z');
    const to = new Date('2026-02-11T18:00:00Z');
    const grid = await guide.grid({ from, to, channelProfile: 3 });

    expect(api.get).toHaveBeenCalledWith('/epg/grid/', {
      query: {
        from: from.toISOString(),
        to: to.toISOString(),
        channel_profile: 3,
      },
    });
    expect(grid.channels).toEqual([{ id: 1 }]);
  });

  it('lets the server choose the window when none is given', async () => {
    api.get.mockResolvedValue({ channels: [] });
    const grid = await guide.grid();

    expect(api.get).toHaveBeenCalledWith('/epg/grid/', {
      query: { from: undefined, to: undefined, channel_profile: undefined },
    });
    expect(grid).toEqual({ start: null, end: null, channels: [] });
  });

  it('accepts and dismisses a guide suggestion by channel', async () => {
    await guide.acceptSuggestion(7);
    expect(api.post).toHaveBeenCalledWith('/epg/suggestions/7/');

    await guide.dismissSuggestion(7);
    expect(api.delete).toHaveBeenCalledWith('/epg/suggestions/7/');
  });
});
