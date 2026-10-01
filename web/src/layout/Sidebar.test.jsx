import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';

import { Sidebar } from './Sidebar.jsx';
import { renderWithProviders } from '../test-utils.jsx';
import { useSession } from '../auth/session.js';
import { channels, fetchVersion, notifications } from '../api/resources.js';

vi.mock('../api/resources.js', () => ({
  channels: { count: vi.fn() },
  notifications: { count: vi.fn() },
  fetchVersion: vi.fn(),
  USER_LEVELS: { STREAMER: 0, STANDARD: 1, ADMIN: 10 },
  USER_LEVEL_LABELS: { 0: 'Streamer', 1: 'Standard', 10: 'Administrator' },
}));

const ADMIN = { id: 1, username: 'root', user_level: 10 };
const STREAMER = { id: 2, username: 'watcher', user_level: 0 };

beforeEach(() => {
  vi.clearAllMocks();
  channels.count.mockResolvedValue(6);
  notifications.count.mockResolvedValue(2);
  fetchVersion.mockResolvedValue('0.1.0');
  useSession.setState({ status: 'authenticated', user: ADMIN });
});

afterEach(() => {
  vi.useRealTimers();
});

describe('the navigation', () => {
  it('runs in the order an operator works, from adding a provider to administering the server', async () => {
    // No count and no badge, so each link's text is its label alone.
    channels.count.mockResolvedValue(null);
    notifications.count.mockResolvedValue(0);
    renderWithProviders(<Sidebar />);
    await waitFor(() => expect(notifications.count).toHaveBeenCalled());

    // The first link is the brand, which goes home rather than to a screen.
    const [, ...screens] = within(
      screen.getByRole('navigation', { name: 'Main' }),
    ).getAllByRole('link');
    expect(screens.map((link) => link.textContent)).toEqual([
      'Sources',
      'Groups',
      'Channels',
      'Logos',
      'TV Guide',
      'Connect',
      'Stats',
      'Notifications',
      'Users',
      'Settings',
    ]);
  });
});

describe('the notification badge', () => {
  it('shows how many conditions nobody has acknowledged', async () => {
    renderWithProviders(<Sidebar />);

    const link = await screen.findByRole('link', { name: /Notifications/ });
    expect(link).toHaveTextContent('2');
  });

  it('shows nothing at all when the inbox is empty', async () => {
    notifications.count.mockResolvedValue(0);
    renderWithProviders(<Sidebar />);

    await waitFor(() => expect(notifications.count).toHaveBeenCalled());
    // A badge reading "0" is a thing to look at that says there is nothing to
    // look at.
    expect(
      await screen.findByRole('link', { name: 'Notifications' }),
    ).toBeInTheDocument();
  });

  it('asks again while the tab is open, because the jobs raising these run on a timer', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    renderWithProviders(<Sidebar />);

    await waitFor(() => expect(notifications.count).toHaveBeenCalledTimes(1));
    notifications.count.mockResolvedValue(5);

    await vi.advanceTimersByTimeAsync(60_000);
    await waitFor(() => expect(notifications.count).toHaveBeenCalledTimes(2));

    const link = await screen.findByRole('link', { name: /Notifications/ });
    await waitFor(() => expect(link).toHaveTextContent('5'));
  });

  it('does not ask on behalf of a user the endpoint would refuse', async () => {
    useSession.setState({ status: 'authenticated', user: STREAMER });
    renderWithProviders(<Sidebar />);

    await waitFor(() => expect(channels.count).toHaveBeenCalled());
    // The route is admin-only, so a streamer polling it would collect a 403 a
    // minute for a link they are never shown.
    expect(notifications.count).not.toHaveBeenCalled();
    expect(screen.queryByRole('link', { name: /Notifications/ })).not.toBeInTheDocument();
  });
});
