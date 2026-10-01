import { beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Users } from './Users.jsx';
import { users as usersApi } from '../api/resources.js';
import { useSession } from '../auth/session.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  users: { list: vi.fn(), create: vi.fn(), update: vi.fn(), remove: vi.fn() },
  USER_LEVELS: { STREAMER: 0, STANDARD: 1, ADMIN: 10 },
  USER_LEVEL_LABELS: { 0: 'Streamer', 1: 'Standard', 10: 'Administrator' },
}));

vi.mock('@mantine/notifications', () => ({
  notifications: { show: vi.fn() },
}));

const ROWS = [
  {
    id: 1,
    username: 'root',
    email: 'root@example.test',
    user_level: 10,
    is_active: true,
    stream_limit: 0,
  },
  {
    id: 2,
    username: 'plex',
    email: null,
    user_level: 0,
    is_active: false,
    stream_limit: 3,
  },
];

beforeEach(() => {
  vi.clearAllMocks();
  usersApi.list.mockResolvedValue(ROWS);
  useSession.setState({ status: 'authenticated', user: ROWS[0] });
});

describe('Users page', () => {
  it('lists users with their level, limit and active state', async () => {
    renderWithProviders(<Users />);

    expect(await screen.findByText('root')).toBeInTheDocument();
    expect(screen.getByText('Administrator')).toBeInTheDocument();
    expect(screen.getByText('Streamer')).toBeInTheDocument();
    expect(screen.getByText('Unlimited')).toBeInTheDocument();
    expect(screen.getByText('3')).toBeInTheDocument();
  });

  it('surfaces a load failure instead of showing an empty table', async () => {
    usersApi.list.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderWithProviders(<Users />);

    expect(await screen.findByText('Not found.')).toBeInTheDocument();
    expect(screen.getByText('Users could not be loaded.')).toBeInTheDocument();
  });

  it('creates a user and reloads the list', async () => {
    const user = userEvent.setup();
    usersApi.create.mockResolvedValue({ id: 3 });
    renderWithProviders(<Users />);
    await screen.findByText('root');

    await user.click(screen.getByRole('button', { name: 'Add user' }));
    const dialog = screen.getByRole('dialog');
    await user.type(within(dialog).getByLabelText(/Username/), 'newbie');
    await user.type(within(dialog).getByLabelText(/Password/), 'hunter2');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(usersApi.create).toHaveBeenCalledWith({
        username: 'newbie',
        email: null,
        password: 'hunter2',
        user_level: 1,
        stream_limit: 0,
        is_active: true,
      }),
    );
    expect(usersApi.list).toHaveBeenCalledTimes(2);
  });

  it('sends the Xtream password on create, not only on edit', async () => {
    // A create that drops `xc_password` still answers 201, so the user appears
    // and cannot sign in to any Xtream player.
    const user = userEvent.setup();
    usersApi.create.mockResolvedValue({ id: 3 });
    renderWithProviders(<Users />);
    await screen.findByText('root');

    await user.click(screen.getByRole('button', { name: 'Add user' }));
    const dialog = screen.getByRole('dialog');
    await user.type(within(dialog).getByLabelText(/Username/), 'newbie');
    await user.type(within(dialog).getByLabelText(/^Password/), 'hunter2');
    await user.type(within(dialog).getByLabelText(/^Xtream password/), 'letmein');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(usersApi.create).toHaveBeenCalledWith(
        expect.objectContaining({
          username: 'newbie',
          password: 'hunter2',
          custom_properties: { xc_password: 'letmein' },
        }),
      ),
    );
  });

  it('refuses to create a user with no password', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Users />);
    await screen.findByText('root');

    await user.click(screen.getByRole('button', { name: 'Add user' }));
    const dialog = screen.getByRole('dialog');
    await user.type(within(dialog).getByLabelText(/Username/), 'newbie');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    expect(
      await within(dialog).findByText('Required for a new user'),
    ).toBeInTheDocument();
    expect(usersApi.create).not.toHaveBeenCalled();
  });

  it('omits the password on edit when the field is left blank', async () => {
    const user = userEvent.setup();
    usersApi.update.mockResolvedValue(ROWS[1]);
    renderWithProviders(<Users />);
    await screen.findByText('plex');

    await user.click(screen.getByRole('button', { name: 'Edit plex' }));
    const dialog = screen.getByRole('dialog');
    await user.clear(within(dialog).getByLabelText(/Username/));
    await user.type(within(dialog).getByLabelText(/Username/), 'plex-renamed');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(usersApi.update).toHaveBeenCalled());
    const [id, payload] = usersApi.update.mock.calls[0];
    expect(id).toBe(2);
    expect(payload.username).toBe('plex-renamed');
    expect(payload).not.toHaveProperty('password');
  });

  it('deletes a user only after the confirmation', async () => {
    const user = userEvent.setup();
    usersApi.remove.mockResolvedValue(null);
    renderWithProviders(<Users />);
    await screen.findByText('plex');

    await user.click(screen.getByRole('button', { name: 'Delete plex' }));

    // Deleting a user takes their API key with it, so every client signed in
    // as them stops working, and there is no undo.
    expect(await screen.findByText(/their API key are deleted/)).toBeInTheDocument();
    expect(usersApi.remove).not.toHaveBeenCalled();

    await user.click(screen.getByRole('button', { name: 'Delete' }));

    await waitFor(() => expect(usersApi.remove).toHaveBeenCalledWith(2));
    expect(usersApi.list).toHaveBeenCalledTimes(2);
  });

  it('sets an Xtream password without touching the login password', async () => {
    const user = userEvent.setup();
    usersApi.update.mockResolvedValue({});
    renderWithProviders(<Users />);
    await screen.findByText('plex');

    await user.click(screen.getByRole('button', { name: 'Edit plex' }));
    await user.type(await screen.findByLabelText('Xtream password'), 'letmein');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    // Connect points people here to set this.
    await waitFor(() =>
      expect(usersApi.update).toHaveBeenCalledWith(
        2,
        expect.objectContaining({ custom_properties: { xc_password: 'letmein' } }),
      ),
    );
    // The login password is untouched, because its field was left alone.
    expect(usersApi.update.mock.calls[0][1]).not.toHaveProperty('password');
  });

  it('will not let the signed-in user delete themselves', async () => {
    renderWithProviders(<Users />);
    await screen.findByText('root');

    expect(screen.getByRole('button', { name: 'Delete root' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Delete plex' })).toBeEnabled();
  });

  it('reports how many rows are selected', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Users />);
    await screen.findByText('root');

    await user.click(screen.getByRole('checkbox', { name: 'Select row 1' }));

    expect(screen.getByText('1 selected')).toBeInTheDocument();
  });
});
