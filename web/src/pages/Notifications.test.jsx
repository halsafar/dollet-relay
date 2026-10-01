import { beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Notifications } from './Notifications.jsx';
import { notifications as notificationsApi } from '../api/resources.js';
import { renderWithProviders } from '../test-utils.jsx';
import { notifyError } from '../notify.js';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  notifications: {
    list: vi.fn(),
    acknowledge: vi.fn(),
    acknowledgeAll: vi.fn(),
    remove: vi.fn(),
  },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

const HOUR = 3_600_000;

/**
 * The three rows the synthetic seed carries, in the order the server sends
 * them: unacknowledged first, and the acknowledged one last despite being the
 * most recently updated.
 */
const ROWS = [
  {
    id: 1003,
    kind: 'm3u.streams_kept',
    subject: 'account:1001',
    severity: 'info',
    title: 'Streams kept on Synth Standard',
    message: '2 streams the provider stopped carrying were kept rather than deleted.',
    detail: { m3u_account_id: 1001 },
    occurrences: 1,
    created_at: new Date(Date.now() - HOUR).toISOString(),
    updated_at: new Date(Date.now() - HOUR).toISOString(),
    acknowledged_at: null,
  },
  {
    id: 1001,
    kind: 'm3u.filter_broken',
    subject: 'account:1003',
    severity: 'warning',
    title: 'Filters skipped on Synth Retired',
    message: '1 of this account’s filter patterns will not compile.',
    detail: { m3u_account_id: 1003 },
    occurrences: 12,
    created_at: new Date(Date.now() - 48 * HOUR).toISOString(),
    updated_at: new Date(Date.now() - 2 * HOUR).toISOString(),
    acknowledged_at: null,
  },
  {
    id: 1002,
    kind: 'auto_sync.range_full',
    subject: 'group:1002',
    severity: 'error',
    title: 'Channel numbers exhausted in Synth News',
    message: 'Auto sync ran out of numbers between 200 and 299.',
    detail: { channel_group_id: 1002 },
    occurrences: 3,
    created_at: new Date(Date.now() - 72 * HOUR).toISOString(),
    updated_at: new Date(Date.now() - HOUR).toISOString(),
    acknowledged_at: new Date(Date.now() - 30 * HOUR).toISOString(),
  },
];

function cellText(header) {
  const table = screen.getByRole('table', { name: 'Notifications' });
  const headers = [...table.querySelectorAll('thead tr:first-child th')];
  const index = headers.findIndex((th) => th.textContent.trim() === header);
  if (index === -1) throw new Error(`no column headed "${header}"`);
  return [...table.querySelectorAll('tbody tr')].map(
    (row) => row.querySelectorAll('td')[index]?.textContent ?? '',
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  notificationsApi.list.mockResolvedValue(ROWS);
  notificationsApi.acknowledge.mockResolvedValue({ ...ROWS[0], acknowledged_at: 'now' });
  notificationsApi.acknowledgeAll.mockResolvedValue({ acknowledged: 2 });
  notificationsApi.remove.mockResolvedValue(null);
});

describe('Notifications', () => {
  it('lists what the jobs found, in the order the server sent it', async () => {
    renderWithProviders(<Notifications />);

    expect(
      await screen.findByText('Filters skipped on Synth Retired'),
    ).toBeInTheDocument();
    expect(cellText('Severity')).toEqual(['info', 'warning', 'error']);
    // The message, not only the title: the title says which account, the
    // message says what it will cost.
    expect(
      screen.getByText('Auto sync ran out of numbers between 200 and 299.'),
    ).toBeInTheDocument();
  });

  it('collapses a recurrence into a count rather than a row per occurrence', async () => {
    renderWithProviders(<Notifications />);
    await screen.findByText('Filters skipped on Synth Retired');

    const seen = cellText('Seen');
    expect(seen[1]).toContain('12 times');
    expect(seen[1]).toContain('2 hours ago');
    // Singular for the fresh one: "1 times" is the kind of thing nobody fixes.
    expect(seen[0]).toContain('once');
    expect(seen[0]).not.toContain('times');
  });

  it('says which rows still want attention and which have been seen', async () => {
    renderWithProviders(<Notifications />);
    await screen.findByText('Filters skipped on Synth Retired');

    const seen = cellText('Seen');
    expect(seen[0]).toContain('needs attention');
    expect(seen[2]).toContain('acknowledged');
  });

  it('acknowledges one row and reloads', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Notifications />);
    await screen.findByText('Filters skipped on Synth Retired');

    await user.click(
      screen.getByRole('button', {
        name: 'Acknowledge Filters skipped on Synth Retired',
      }),
    );

    expect(notificationsApi.acknowledge).toHaveBeenCalledWith(1001);
    await waitFor(() => expect(notificationsApi.list).toHaveBeenCalledTimes(2));
  });

  it('offers no acknowledge button on a row that has already been seen', async () => {
    renderWithProviders(<Notifications />);
    await screen.findByText('Channel numbers exhausted in Synth News');

    expect(
      screen.queryByRole('button', {
        name: 'Acknowledge Channel numbers exhausted in Synth News',
      }),
    ).not.toBeInTheDocument();
  });

  it('acknowledges everything at once, and offers not to when there is nothing', async () => {
    const user = userEvent.setup();
    const { unmount } = renderWithProviders(<Notifications />);
    await screen.findByText('Filters skipped on Synth Retired');

    await user.click(screen.getByRole('button', { name: 'Acknowledge all' }));
    expect(notificationsApi.acknowledgeAll).toHaveBeenCalled();

    unmount();
    notificationsApi.list.mockResolvedValue([ROWS[2]]);
    renderWithProviders(<Notifications />);
    await screen.findByText('Channel numbers exhausted in Synth News');

    expect(screen.getByRole('button', { name: 'Acknowledge all' })).toBeDisabled();
  });

  it('confirms a delete and says it will come back if the condition has not gone', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Notifications />);
    await screen.findByText('Filters skipped on Synth Retired');

    await user.click(
      screen.getByRole('button', { name: 'Delete Filters skipped on Synth Retired' }),
    );

    // Deleting and acknowledging are different answers, and the modal is the
    // only place that difference can be said.
    expect(await screen.findByText(/raises it again/)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Delete' }));

    expect(notificationsApi.remove).toHaveBeenCalledWith(1001);
  });

  it('says nothing needs attention rather than showing an empty table', async () => {
    notificationsApi.list.mockResolvedValue([]);
    renderWithProviders(<Notifications />);

    expect(await screen.findByText('Nothing needs your attention.')).toBeInTheDocument();
  });

  it('reads a condition raised moments ago in seconds rather than as nothing', async () => {
    notificationsApi.list.mockResolvedValue([
      { ...ROWS[0], updated_at: new Date(Date.now() - 20_000).toISOString() },
    ]);
    renderWithProviders(<Notifications />);
    await screen.findByText('Streams kept on Synth Standard');

    // The units stop at a minute, so the row a refresh raised while the page
    // was open must not fall off the bottom of the ladder.
    expect(cellText('Seen')[0]).toMatch(/second/);
  });

  it('says which action failed, since the server message will not', async () => {
    const user = userEvent.setup();
    const refused = new ApiError('Nope', { status: 403 });
    notificationsApi.acknowledge.mockRejectedValue(refused);
    notificationsApi.acknowledgeAll.mockRejectedValue(refused);
    notificationsApi.remove.mockRejectedValue(refused);

    renderWithProviders(<Notifications />);
    await screen.findByText('Filters skipped on Synth Retired');

    await user.click(
      screen.getByRole('button', {
        name: 'Acknowledge Filters skipped on Synth Retired',
      }),
    );
    await user.click(screen.getByRole('button', { name: 'Acknowledge all' }));
    await user.click(
      screen.getByRole('button', { name: 'Delete Filters skipped on Synth Retired' }),
    );
    await user.click(await screen.findByRole('button', { name: 'Delete' }));

    expect(notifyError).toHaveBeenCalledWith('Could not acknowledge', refused);
    expect(notifyError).toHaveBeenCalledWith(
      'Could not delete the notification',
      refused,
    );
  });

  it('distinguishes an empty inbox from one it could not load', async () => {
    notificationsApi.list.mockRejectedValue(new ApiError('Nope', { status: 500 }));
    renderWithProviders(<Notifications />);

    expect(
      await screen.findByText('Notifications could not be loaded.'),
    ).toBeInTheDocument();
  });
});
