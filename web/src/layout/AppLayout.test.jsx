import { beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Route, Routes } from 'react-router-dom';

import { AppLayout } from './AppLayout.jsx';
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

beforeEach(() => {
  vi.clearAllMocks();
  channels.count.mockResolvedValue(6);
  notifications.count.mockResolvedValue(0);
  fetchVersion.mockResolvedValue('0.1.0');
  useSession.setState({ status: 'authenticated', user: ADMIN });
});

/** The shell around two stand-in screens, so a link can be seen to arrive. */
function renderShell() {
  return renderWithProviders(
    <Routes>
      <Route element={<AppLayout />}>
        <Route path="/channels" element={<h1>Channels</h1>} />
        <Route path="/sources" element={<h1>Sources</h1>} />
      </Route>
    </Routes>,
    { route: '/channels' },
  );
}

/**
 * Whether the dock or the burger is shown is the stylesheet's decision, which
 * this DOM cannot see. What it can see is the drawer: that the burger opens
 * it, that it holds the navigation, and that choosing a screen puts it away.
 */
describe('the navigation drawer', () => {
  it('opens from the burger and closes once a screen is chosen', async () => {
    const user = userEvent.setup();
    renderShell();

    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Open navigation' }));

    const drawer = await screen.findByRole('dialog');
    const nav = within(drawer).getByRole('navigation', { name: 'Main' });
    await user.click(within(nav).getByRole('link', { name: 'Sources' }));

    expect(await screen.findByRole('heading', { name: 'Sources' })).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  });

  it('closes on the brand as well, which also goes home', async () => {
    const user = userEvent.setup();
    renderShell();
    await user.click(screen.getByRole('button', { name: 'Open navigation' }));

    const drawer = await screen.findByRole('dialog');
    await user.click(within(drawer).getByRole('link', { name: 'Dollet' }));

    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(screen.getByRole('heading', { name: 'Channels' })).toBeInTheDocument();
  });
});
