import { beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { App } from './App.jsx';
import { renderWithProviders } from './test-utils.jsx';
import { useSession } from './auth/session.js';
import {
  auth,
  channels,
  users as usersApi,
  settings as settingsApi,
} from './api/resources.js';
import { ApiError } from './api/errors.js';

vi.mock('./api/resources.js', () => ({
  auth: {
    login: vi.fn(),
    me: vi.fn(),
    bootstrap: vi.fn(),
    setupStatus: vi.fn().mockResolvedValue({ superuser_exists: true }),
  },
  users: { list: vi.fn().mockResolvedValue([]) },
  settings: { list: vi.fn().mockResolvedValue([]) },
  guide: {
    grid: vi.fn().mockResolvedValue({ channels: [] }),
    acceptSuggestion: vi.fn(),
    dismissSuggestion: vi.fn(),
  },
  channelProfiles: { list: vi.fn().mockResolvedValue([]) },
  channels: { count: vi.fn().mockResolvedValue(6) },
  notifications: {
    count: vi.fn().mockResolvedValue(0),
    list: vi.fn().mockResolvedValue([]),
    acknowledge: vi.fn(),
    acknowledgeAll: vi.fn(),
    remove: vi.fn(),
  },
  userAgents: { list: vi.fn().mockResolvedValue([]) },
  streamProfiles: { list: vi.fn().mockResolvedValue([]) },
  outputProfiles: { list: vi.fn().mockResolvedValue([]) },
  fetchVersion: vi.fn().mockResolvedValue('0.1.0'),
  USER_LEVELS: { STREAMER: 0, STANDARD: 1, ADMIN: 10 },
  USER_LEVEL_LABELS: { 0: 'Streamer', 1: 'Standard', 10: 'Administrator' },
}));

function renderApp(route) {
  return renderWithProviders(<App />, { route });
}

const ADMIN = { id: 1, username: 'root', user_level: 10 };

beforeEach(() => {
  vi.clearAllMocks();
  usersApi.list.mockResolvedValue([]);
  channels.count.mockResolvedValue(6);
  auth.setupStatus.mockResolvedValue({ superuser_exists: true });
  settingsApi.list.mockResolvedValue([]);
  useSession.setState({ status: 'anonymous', user: null });
});

describe('routing and guards', () => {
  it('sends an anonymous visitor to the login page', async () => {
    renderApp('/users');
    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
  });

  it('shows a spinner while the stored session is being resolved', () => {
    useSession.setState({ status: 'loading' });
    renderApp('/users');

    expect(screen.queryByRole('button', { name: 'Sign in' })).not.toBeInTheDocument();
    expect(screen.queryByRole('table')).not.toBeInTheDocument();
  });

  it('keeps a signed-in user off the login page', async () => {
    useSession.setState({ status: 'authenticated', user: ADMIN });
    renderApp('/login');

    expect(await screen.findByRole('heading', { name: 'Channels' })).toBeInTheDocument();
  });

  it('lands on Channels from the root path', async () => {
    useSession.setState({ status: 'authenticated', user: ADMIN });
    renderApp('/');

    expect(await screen.findByRole('heading', { name: 'Channels' })).toBeInTheDocument();
  });

  it('renders the 1.0 navigation and omits the out-of-scope entries', async () => {
    useSession.setState({ status: 'authenticated', user: ADMIN });
    renderApp('/channels');

    const nav = await screen.findByRole('navigation', { name: 'Main' });
    for (const label of [
      'Channels',
      'Groups',
      'TV Guide',
      'Sources',
      'Logos',
      'Notifications',
      'Stats',
      'Users',
      'Settings',
    ]) {
      expect(nav).toHaveTextContent(label);
    }
    for (const label of ['VODs', 'DVR', 'Plugins']) {
      expect(nav).not.toHaveTextContent(label);
    }
  });

  it('shows the signed-in user and the server version in the sidebar foot', async () => {
    useSession.setState({ status: 'authenticated', user: ADMIN });
    renderApp('/channels');

    expect(await screen.findByText('root')).toBeInTheDocument();
    expect(await screen.findByText('v0.1.0')).toBeInTheDocument();
  });

  it('resolves every navigation entry to a real page', async () => {
    useSession.setState({ status: 'authenticated', user: ADMIN });
    renderApp('/guide');

    // Nothing in the nav is a dead end.
    expect(await screen.findByRole('heading', { name: 'TV Guide' })).toBeInTheDocument();
  });

  it('shows a not-found page inside the shell for an unknown route', async () => {
    useSession.setState({ status: 'authenticated', user: ADMIN });
    renderApp('/nope');

    expect(await screen.findByText('That page does not exist.')).toBeInTheDocument();
    expect(screen.getByRole('navigation', { name: 'Main' })).toBeInTheDocument();
  });

  it('signs out and returns to the login page', async () => {
    const user = userEvent.setup();
    useSession.setState({ status: 'authenticated', user: ADMIN });
    renderApp('/channels');

    await user.click(await screen.findByRole('button', { name: 'Sign out' }));

    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
  });
});

describe('login', () => {
  it('signs in and lands on the app', async () => {
    const user = userEvent.setup();
    auth.login.mockResolvedValue({ access: 'a', refresh: 'r' });
    auth.me.mockResolvedValue(ADMIN);

    renderApp('/login');

    await user.type(await screen.findByLabelText('Username'), 'root');
    await user.type(screen.getByLabelText('Password'), 'hunter2');
    await user.click(screen.getByRole('button', { name: 'Sign in' }));

    expect(await screen.findByRole('heading', { name: 'Channels' })).toBeInTheDocument();
  });

  it('shows the server message when the credentials are wrong', async () => {
    const user = userEvent.setup();
    auth.login.mockRejectedValue(
      new ApiError('No active account found with the given credentials', { status: 401 }),
    );

    renderApp('/login');

    await user.type(await screen.findByLabelText('Username'), 'root');
    await user.type(screen.getByLabelText('Password'), 'wrong');
    await user.click(screen.getByRole('button', { name: 'Sign in' }));

    expect(
      await screen.findByText('No active account found with the given credentials'),
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Sign in' })).toBeInTheDocument();
  });

  it('does not call the API with empty fields', async () => {
    const user = userEvent.setup();
    renderApp('/login');

    await user.click(await screen.findByRole('button', { name: 'Sign in' }));

    await waitFor(() => expect(screen.getAllByText('Required')).toHaveLength(2));
    expect(auth.login).not.toHaveBeenCalled();
  });
});

describe('a fresh install', () => {
  it('offers to create the first administrator instead of a sign-in form', async () => {
    auth.setupStatus.mockResolvedValue({ superuser_exists: false });
    renderApp('/login');

    // Without this the login form 401s forever and the only way in is curl.
    expect(
      await screen.findByRole('button', { name: 'Create administrator' }),
    ).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Sign in' })).not.toBeInTheDocument();
    expect(screen.getByText(/no accounts yet/i)).toBeInTheDocument();
  });

  it('creates the administrator and lands signed in', async () => {
    const user = userEvent.setup();
    auth.setupStatus.mockResolvedValue({ superuser_exists: false });
    auth.bootstrap.mockResolvedValue({ access: 'a', refresh: 'r' });
    auth.me.mockResolvedValue(ADMIN);
    renderApp('/login');

    await user.type(await screen.findByLabelText('Username'), 'root');
    await user.type(screen.getByLabelText('Password'), 'hunter2');
    await user.click(screen.getByRole('button', { name: 'Create administrator' }));

    // The server answers with a token pair, so there is no second sign-in.
    await waitFor(() => expect(auth.bootstrap).toHaveBeenCalledWith('root', 'hunter2'));
    expect(await screen.findByRole('heading', { name: 'Channels' })).toBeInTheDocument();
    expect(auth.login).not.toHaveBeenCalled();
  });

  it('shows the sign-in form once an administrator exists', async () => {
    auth.setupStatus.mockResolvedValue({ superuser_exists: true });
    renderApp('/login');

    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Create administrator' }),
    ).not.toBeInTheDocument();
  });

  it('falls back to signing in when the setup check fails', async () => {
    auth.setupStatus.mockRejectedValue(new ApiError('Unreachable', { status: 0 }));
    renderApp('/login');

    // Offering to create an administrator on an instance that already has one
    // would be refused anyway, so the safe default is the sign-in form.
    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
    expect(screen.getByText(/Could not tell whether this instance/)).toBeInTheDocument();
  });

  it('reports a refused bootstrap rather than pretending it worked', async () => {
    const user = userEvent.setup();
    auth.setupStatus.mockResolvedValue({ superuser_exists: false });
    auth.bootstrap.mockRejectedValue(
      new ApiError('You do not have permission to perform this action.', { status: 403 }),
    );
    renderApp('/login');

    await user.type(await screen.findByLabelText('Username'), 'root');
    await user.type(screen.getByLabelText('Password'), 'hunter2');
    await user.click(screen.getByRole('button', { name: 'Create administrator' }));

    // The server refuses a second bootstrap, which is what a race looks like.
    expect(
      await screen.findByText('You do not have permission to perform this action.'),
    ).toBeInTheDocument();
  });
});
