import { beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { AccountDetail } from './AccountDetail.jsx';
import { m3uAccounts as m3uApi } from '../api/resources.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  m3uAccounts: {
    groups: vi.fn(),
    setGroup: vi.fn(),
    profiles: vi.fn(),
    createProfile: vi.fn(),
    removeProfile: vi.fn(),
    filters: vi.fn(),
    createFilter: vi.fn(),
    removeFilter: vi.fn(),
  },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

const ACCOUNT = { id: 2, name: 'HD Homerun' };

/** The locked default plus one the user added, so both branches render. */
const PROFILES = [
  {
    id: 1,
    name: 'Default',
    is_default: true,
    search_pattern: '',
    replace_pattern: '',
  },
  {
    id: 2,
    name: 'Port swap',
    is_default: false,
    search_pattern: ':8080/',
    replace_pattern: ':8081/',
  },
];

const FILTERS = [
  { id: 1, filter_type: 'name', regex_pattern: '(?i)adult', exclude: true, order: 0 },
  { id: 2, filter_type: 'group', regex_pattern: '^Sports', exclude: false, order: 1 },
];

function open() {
  return renderWithProviders(<AccountDetail account={ACCOUNT} onClose={vi.fn()} />);
}

beforeEach(() => {
  vi.clearAllMocks();
  m3uApi.groups.mockResolvedValue([]);
  m3uApi.profiles.mockResolvedValue(PROFILES);
  m3uApi.filters.mockResolvedValue(FILTERS);
  m3uApi.createProfile.mockResolvedValue({ id: 3 });
  m3uApi.removeProfile.mockResolvedValue(null);
  m3uApi.createFilter.mockResolvedValue({ id: 3 });
  m3uApi.removeFilter.mockResolvedValue(null);
});

describe('rewrite profiles', () => {
  it('shows each profile and what it rewrites', async () => {
    const user = userEvent.setup();
    open();

    await user.click(screen.getByRole('button', { name: /Rewrite profiles/ }));

    expect(await screen.findByText('Port swap')).toBeInTheDocument();
    expect(screen.getByText(/:8080\/ → :8081\//)).toBeInTheDocument();
  });

  it('will not offer to delete the locked default', async () => {
    const user = userEvent.setup();
    open();
    await user.click(screen.getByRole('button', { name: /Rewrite profiles/ }));

    expect(
      await screen.findByRole('button', { name: 'Delete profile Port swap' }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Delete profile Default' }),
    ).not.toBeInTheDocument();
  });

  it('adds a profile', async () => {
    const user = userEvent.setup();
    open();
    await user.click(screen.getByRole('button', { name: /Rewrite profiles/ }));
    await user.click(await screen.findByRole('button', { name: 'Add profile' }));

    await user.type(screen.getByRole('textbox', { name: 'Name' }), 'Mirror');
    await user.type(screen.getByRole('textbox', { name: 'Search pattern' }), 'a\\.test');
    await user.type(screen.getByRole('textbox', { name: /Replace pattern/ }), 'b.test');
    await user.click(screen.getByRole('button', { name: 'Add profile' }));

    await waitFor(() =>
      expect(m3uApi.createProfile).toHaveBeenCalledWith(2, {
        name: 'Mirror',
        search_pattern: 'a\\.test',
        replace_pattern: 'b.test',
      }),
    );
  });

  it('reports a pattern the server refuses to compile', async () => {
    const user = userEvent.setup();
    m3uApi.createProfile.mockRejectedValue(
      new ApiError('`(?<bad` will not compile: unclosed group', { status: 400 }),
    );
    open();
    await user.click(screen.getByRole('button', { name: /Rewrite profiles/ }));
    await user.click(await screen.findByRole('button', { name: 'Add profile' }));
    await user.type(screen.getByRole('textbox', { name: 'Name' }), 'Bad');
    await user.click(screen.getByRole('button', { name: 'Add profile' }));

    // Refused at the door rather than discovered at the next refresh, so the
    // draft stays open to be fixed.
    await waitFor(() => expect(m3uApi.createProfile).toHaveBeenCalled());
    expect(screen.getByRole('textbox', { name: 'Name' })).toBeInTheDocument();
  });

  it('abandons a draft profile', async () => {
    const user = userEvent.setup();
    open();
    await user.click(screen.getByRole('button', { name: /Rewrite profiles/ }));
    await user.click(await screen.findByRole('button', { name: 'Add profile' }));
    await user.click(screen.getByRole('button', { name: 'Cancel' }));

    expect(screen.queryByRole('textbox', { name: 'Name' })).not.toBeInTheDocument();
    expect(m3uApi.createProfile).not.toHaveBeenCalled();
  });

  it('deletes a profile', async () => {
    const user = userEvent.setup();
    open();
    await user.click(screen.getByRole('button', { name: /Rewrite profiles/ }));
    await user.click(
      await screen.findByRole('button', { name: 'Delete profile Port swap' }),
    );

    // Confirmed first: deleting a rewrite profile sends every stream it
    // rewrote back to the provider's own URL at the next refresh.
    expect(await screen.findByText(/back to the URL the provider gave/)).toBeInTheDocument();
    expect(m3uApi.removeProfile).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Delete' }));

    await waitFor(() => expect(m3uApi.removeProfile).toHaveBeenCalledWith(2, 2));
  });

  it('says when a provider has no rewrite profiles', async () => {
    const user = userEvent.setup();
    m3uApi.profiles.mockResolvedValue([]);
    open();
    await user.click(screen.getByRole('button', { name: /Rewrite profiles/ }));

    expect(
      await screen.findByText(/played at the URL the provider gave/),
    ).toBeInTheDocument();
  });
});

describe('import filters', () => {
  it('distinguishes an exclude filter from an include one', async () => {
    const user = userEvent.setup();
    open();

    await user.click(screen.getByRole('button', { name: /Import filters/ }));

    expect(await screen.findByText('exclude')).toBeInTheDocument();
    expect(screen.getByText('include')).toBeInTheDocument();
    expect(screen.getByText('(?i)adult')).toBeInTheDocument();
  });

  it('adds an exclude filter by default', async () => {
    const user = userEvent.setup();
    open();
    await user.click(screen.getByRole('button', { name: /Import filters/ }));
    await user.click(await screen.findByRole('button', { name: 'Add filter' }));

    await user.type(screen.getByRole('textbox', { name: 'Pattern' }), '^XXX');
    await user.click(screen.getByRole('button', { name: 'Add filter' }));

    await waitFor(() =>
      expect(m3uApi.createFilter).toHaveBeenCalledWith(2, {
        filter_type: 'name',
        regex_pattern: '^XXX',
        exclude: true,
      }),
    );
  });

  it('can be flipped to an include filter', async () => {
    const user = userEvent.setup();
    open();
    await user.click(screen.getByRole('button', { name: /Import filters/ }));
    await user.click(await screen.findByRole('button', { name: 'Add filter' }));

    await user.type(screen.getByRole('textbox', { name: 'Pattern' }), 'Sports');
    await user.click(screen.getByRole('switch', { name: /Exclude what this matches/ }));
    await user.click(screen.getByRole('button', { name: 'Add filter' }));

    await waitFor(() =>
      expect(m3uApi.createFilter).toHaveBeenCalledWith(
        2,
        expect.objectContaining({ exclude: false }),
      ),
    );
  });

  it('deletes a filter', async () => {
    const user = userEvent.setup();
    open();
    await user.click(screen.getByRole('button', { name: /Import filters/ }));
    await user.click(
      await screen.findByRole('button', { name: 'Delete filter (?i)adult' }),
    );

    expect(await screen.findByText(/imported at the next refresh/)).toBeInTheDocument();
    expect(m3uApi.removeFilter).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: 'Delete' }));

    await waitFor(() => expect(m3uApi.removeFilter).toHaveBeenCalledWith(2, 1));
  });

  it('says that no filters means everything is imported', async () => {
    const user = userEvent.setup();
    m3uApi.filters.mockResolvedValue([]);
    open();
    await user.click(screen.getByRole('button', { name: /Import filters/ }));

    expect(
      await screen.findByText(/Every stream the provider lists is imported/),
    ).toBeInTheDocument();
  });

  it('survives a rejected delete', async () => {
    const user = userEvent.setup();
    m3uApi.removeFilter.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    open();
    await user.click(screen.getByRole('button', { name: /Import filters/ }));
    await user.click(
      await screen.findByRole('button', { name: 'Delete filter (?i)adult' }),
    );
    await user.click(await screen.findByRole('button', { name: 'Delete' }));

    await waitFor(() => expect(m3uApi.removeFilter).toHaveBeenCalled());
    expect(screen.getByText('(?i)adult')).toBeInTheDocument();
  });
});

describe('groups', () => {
  it('summarises what the account imports and points at the Groups page', async () => {
    m3uApi.groups.mockResolvedValue([
      { channel_group: 8, name: 'Canada', enabled: true, auto_channel_sync: true },
      { channel_group: 9, name: 'Adult', enabled: false, auto_channel_sync: false },
    ]);
    open();

    expect(
      await screen.findByText('Importing 1 of 2 groups, 1 auto-synced.'),
    ).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Manage groups →' })).toHaveAttribute(
      'href',
      '/groups?account=2',
    );
    // Nothing here edits a group: that lives on the page the link goes to.
    expect(screen.queryByRole('switch', { name: /Import/ })).not.toBeInTheDocument();
  });

  it('says so when the account has no groups yet', async () => {
    m3uApi.groups.mockResolvedValue([]);
    open();

    expect(await screen.findByText(/No groups yet/)).toBeInTheDocument();
  });
});
