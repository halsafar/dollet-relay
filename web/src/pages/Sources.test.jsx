import { beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Sources } from './Sources.jsx';
import { describeStaleDays } from './staleStreams.js';
import {
  epgSources as epgApi,
  jobs as jobsApi,
  m3uAccounts as m3uApi,
} from '../api/resources.js';
import { renderWithProviders } from '../test-utils.jsx';
import { notifyDone, notifyError } from '../notify.js';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  m3uAccounts: {
    list: vi.fn(),
    create: vi.fn(),
    update: vi.fn(),
    remove: vi.fn(),
    refresh: vi.fn(),
    refreshAll: vi.fn(),
    profiles: vi.fn(),
    createProfile: vi.fn(),
    removeProfile: vi.fn(),
    filters: vi.fn(),
    createFilter: vi.fn(),
    removeFilter: vi.fn(),
    groups: vi.fn(),
    setGroup: vi.fn(),
  },
  epgSources: {
    list: vi.fn(),
    create: vi.fn(),
    update: vi.fn(),
    remove: vi.fn(),
    refresh: vi.fn(),
    refreshAll: vi.fn(),
    ambiguous: vi.fn(),
    match: vi.fn(),
  },
  jobs: { list: vi.fn(), cancel: vi.fn() },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

/** Shaped exactly as `m3u::serialize` emits it — note: no password field. */
const ACCOUNT = {
  id: 2,
  name: 'HD Homerun',
  account_type: 'standard',
  server_url: 'http://192.168.40.94/lineup.m3u',
  file_path: null,
  username: 'provider-user',
  has_password: true,
  max_streams: 0,
  is_active: true,
  locked: false,
  priority: 0,
  refresh_interval_hours: 12,
  stale_stream_days: 7,
  status: 'success',
  progress: 1,
  last_message: '61 streams: 0 new, 2 updated, 59 unchanged, 0 stale, 0 removed',
  updated_at: '2026-09-11T18:11:47Z',
  next_run_at: '2026-09-12T06:11:47Z',
};

/** A second account mid-refresh, so the running and idle paths both render. */
const FETCHING = {
  ...ACCOUNT,
  id: 3,
  name: 'Backup provider',
  status: 'running',
  progress: 0.4,
  last_message: 'Reading playlist…',
  has_password: false,
  username: null,
  stale_stream_days: 0,
};

const SOURCE = {
  id: 1,
  name: 'SaintLouisMO-OTA.xml',
  source_type: 'xmltv',
  url: null,
  file_path: '/data/epgs/SaintLouisMO-OTA.xml',
  username: null,
  has_password: false,
  is_active: true,
  priority: 0,
  refresh_interval_hours: 24,
  status: 'success',
  progress: 1,
  last_message:
    '5909 guide channels, 2754 programmes for 26 mapped, 23 auto-matched, 3 need a decision',
  updated_at: '2026-09-11T18:00:00Z',
  next_run_at: null,
};

const RUNNING_JOB = {
  key: 'm3u_refresh:3',
  kind: 'm3u_refresh',
  payload: { m3u_account_id: 3 },
  state: 'running',
  progress: 0.4,
  running: true,
  message: 'Reading playlist…',
  last_error: null,
  next_run_at: null,
};

function cellText(label, header) {
  const table = screen.getByRole('table', { name: label });
  const headers = [...table.querySelectorAll('thead tr:first-child th')];
  const index = headers.findIndex((th) => th.textContent.trim() === header);
  if (index === -1) throw new Error(`no column headed "${header}" in ${label}`);
  return [...table.querySelectorAll('tbody tr')].map(
    (row) => row.querySelectorAll('td')[index]?.textContent ?? '',
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  m3uApi.list.mockResolvedValue([ACCOUNT]);
  m3uApi.refresh.mockResolvedValue({ started: true, job: 'm3u_refresh:2' });
  m3uApi.refreshAll.mockResolvedValue({ started: [] });
  m3uApi.groups.mockResolvedValue([]);
  m3uApi.profiles.mockResolvedValue([]);
  m3uApi.filters.mockResolvedValue([]);
  epgApi.list.mockResolvedValue([SOURCE]);
  epgApi.refresh.mockResolvedValue({ started: true });
  epgApi.refreshAll.mockResolvedValue({ started: [] });
  jobsApi.list.mockResolvedValue([]);
  jobsApi.cancel.mockResolvedValue({ cancelling: true });
  epgApi.ambiguous.mockResolvedValue([]);
  epgApi.match.mockResolvedValue({ matched: 2, need_a_decision: 3 });
});

describe('credentials', () => {
  it('never renders the username in the account table', async () => {
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    // This table is the most screenshot-able surface in the application.
    expect(screen.queryByText('provider-user')).not.toBeInTheDocument();
    expect(cellText('M3U accounts', 'URL / File')).toEqual([
      'http://192.168.40.94/lineup.m3u',
    ]);
  });

  it('never puts a password anywhere, because the API never sends one', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Edit HD Homerun' }));
    const dialog = await screen.findByRole('dialog');

    // `has_password` is a boolean; the value itself is write-only server-side.
    expect(within(dialog).getByLabelText(/Password/)).toHaveValue('');
    expect(within(dialog).getByText(/write-only/i)).toBeInTheDocument();
  });

  it('omits the password from the payload when it is left blank', async () => {
    const user = userEvent.setup();
    m3uApi.update.mockResolvedValue(ACCOUNT);
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Edit HD Homerun' }));
    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(m3uApi.update).toHaveBeenCalled());
    // Sending '' would clear a working provider password.
    expect(m3uApi.update.mock.calls[0][1]).not.toHaveProperty('password');
  });

  it('sends a password only when one was typed', async () => {
    const user = userEvent.setup();
    m3uApi.update.mockResolvedValue(ACCOUNT);
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Edit HD Homerun' }));
    const dialog = await screen.findByRole('dialog');
    await user.type(within(dialog).getByLabelText(/Password/), 'new-secret');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(m3uApi.update).toHaveBeenCalled());
    expect(m3uApi.update.mock.calls[0][1].password).toBe('new-secret');
  });
});

describe('what a refresh did', () => {
  it('shows the server summary verbatim', async () => {
    renderWithProviders(<Sources />);

    expect(
      await screen.findByText(
        '61 streams: 0 new, 2 updated, 59 unchanged, 0 stale, 0 removed',
      ),
    ).toBeInTheDocument();
  });

  it('shows when each source last refreshed', async () => {
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    expect(cellText('M3U accounts', 'Refreshed')[0]).not.toBe('Never');
  });

  it('says Never for a source that has not run', async () => {
    epgApi.list.mockResolvedValue([{ ...SOURCE, updated_at: null, status: 'idle' }]);
    renderWithProviders(<Sources />);
    await screen.findByText('SaintLouisMO-OTA.xml');

    expect(cellText('Guide sources', 'Refreshed')).toEqual(['Never']);
  });

  it('surfaces a failed refresh as an error, not as success', async () => {
    m3uApi.list.mockResolvedValue([
      { ...ACCOUNT, status: 'failed', last_message: 'connection refused' },
    ]);
    renderWithProviders(<Sources />);

    expect(await screen.findByText('connection refused')).toBeInTheDocument();
    expect(cellText('M3U accounts', 'Status')).toEqual(['failed']);
  });

  it('shows progress while a refresh is running', async () => {
    m3uApi.list.mockResolvedValue([FETCHING]);
    renderWithProviders(<Sources />);

    expect(await screen.findByText('Reading playlist…')).toBeInTheDocument();
    expect(cellText('M3U accounts', 'Status')).toEqual(['running']);
  });
});

describe('triggering and cancelling a refresh', () => {
  it('starts a refresh for one account', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Refresh HD Homerun' }));

    await waitFor(() => expect(m3uApi.refresh).toHaveBeenCalledWith(2));
  });

  it('starts a refresh for one EPG source', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('SaintLouisMO-OTA.xml');

    await user.click(
      screen.getByRole('button', { name: 'Refresh SaintLouisMO-OTA.xml' }),
    );

    await waitFor(() => expect(epgApi.refresh).toHaveBeenCalledWith(1));
  });

  it('offers cancel instead of refresh while a job is running', async () => {
    m3uApi.list.mockResolvedValue([FETCHING]);
    jobsApi.list.mockResolvedValue([RUNNING_JOB]);
    renderWithProviders(<Sources />);
    await screen.findByText('Backup provider');

    expect(
      await screen.findByRole('button', { name: 'Cancel refresh of Backup provider' }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Refresh Backup provider' }),
    ).not.toBeInTheDocument();
  });

  it('cancels by job key, matched through the payload rather than the key text', async () => {
    const user = userEvent.setup();
    m3uApi.list.mockResolvedValue([FETCHING]);
    jobsApi.list.mockResolvedValue([RUNNING_JOB]);
    renderWithProviders(<Sources />);

    await user.click(
      await screen.findByRole('button', { name: 'Cancel refresh of Backup provider' }),
    );

    await waitFor(() => expect(jobsApi.cancel).toHaveBeenCalledWith('m3u_refresh:3'));
  });

  it('ignores a job that has already finished', async () => {
    m3uApi.list.mockResolvedValue([FETCHING]);
    // The row still says running after a crash; the scheduler does not.
    jobsApi.list.mockResolvedValue([{ ...RUNNING_JOB, running: false }]);
    renderWithProviders(<Sources />);
    await screen.findByText('Backup provider');

    expect(
      screen.getByRole('button', { name: 'Refresh Backup provider' }),
    ).toBeInTheDocument();
  });

  it('refreshes everything at once', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    const [m3uAll] = screen.getAllByRole('button', { name: 'Refresh all' });
    await user.click(m3uAll);

    await waitFor(() => expect(m3uApi.refreshAll).toHaveBeenCalled());
  });

  it('reports a refresh that could not be started', async () => {
    const user = userEvent.setup();
    m3uApi.refresh.mockRejectedValue(
      new ApiError('a refresh is already running', { status: 409 }),
    );
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Refresh HD Homerun' }));

    await waitFor(() => expect(m3uApi.refresh).toHaveBeenCalled());
    expect(screen.getByText('HD Homerun')).toBeInTheDocument();
  });
});

function match(id, channel, candidate, score) {
  return {
    channel_id: id,
    channel_name: channel,
    epg_data_id: id * 10,
    candidate_name: candidate,
    candidate_tvg_id: null,
    score,
  };
}

describe('the ambiguous EPG band', () => {
  it('names the channels that need a decision and their candidates', async () => {
    epgApi.ambiguous.mockResolvedValue([
      match(1, 'PLOV-DT', 'KAZ 2 St. Louis', 71.4),
      match(2, 'WGN America', 'WGN', 68),
      match(3, 'Glimberra Net', 'Glimberra Network', 66.5),
    ]);
    renderWithProviders(<Sources />);

    // Three outcomes, not two: rendering ambiguous as "no guide" makes a
    // resolvable channel look broken.
    expect(
      await screen.findByText('3 channels need a guide decision'),
    ).toBeInTheDocument();
    expect(screen.getByText(/not a confident one/i)).toBeInTheDocument();

    // The count alone cannot be acted on. Naming them is the point of the
    // endpoint this reads.
    expect(screen.getByText('PLOV-DT')).toBeInTheDocument();
    expect(screen.getByText(/KAZ 2 St. Louis \(71% match\)/)).toBeInTheDocument();

    expect(screen.getByRole('link', { name: /on the TV Guide/ })).toHaveAttribute(
      'href',
      '/guide',
    );
  });

  it('uses the singular for one channel', async () => {
    epgApi.ambiguous.mockResolvedValue([match(1, 'PLOV-DT', 'KAZ 2', 70)]);
    renderWithProviders(<Sources />);

    expect(
      await screen.findByText('1 channel needs a guide decision'),
    ).toBeInTheDocument();
  });

  it('counts the rest rather than printing an unbounded list', async () => {
    epgApi.ambiguous.mockResolvedValue(
      Array.from({ length: 8 }, (_, index) =>
        match(index + 1, `Channel ${index + 1}`, `Guide ${index + 1}`, 70),
      ),
    );
    renderWithProviders(<Sources />);

    expect(
      await screen.findByText('8 channels need a guide decision'),
    ).toBeInTheDocument();
    expect(screen.getByText('Channel 5')).toBeInTheDocument();
    expect(screen.queryByText('Channel 6')).not.toBeInTheDocument();
    expect(screen.getByText('and 3 more.')).toBeInTheDocument();
  });

  it('says nothing when the matcher decided everything', async () => {
    epgApi.ambiguous.mockResolvedValue([]);
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    expect(screen.queryByText(/need a guide decision/)).not.toBeInTheDocument();
  });
});

describe('matching unmapped channels', () => {
  const button = () => screen.findByRole('button', { name: 'Match unmapped channels' });

  it('says what it will and will not do before it is pressed', async () => {
    renderWithProviders(<Sources />);

    // The button writes `epg_data_id` across the catalogue, so what it refuses
    // to do is as load-bearing as what it does.
    const alert = (await button()).closest('[class*="Alert-root"]');
    expect(alert).toHaveTextContent(/no guide/);
    expect(alert).toHaveTextContent(/never touched/);
    expect(alert).toHaveTextContent(/becomes a decision/);
  });

  it('posts and reports both counts', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);

    await user.click(await button());

    await waitFor(() => expect(epgApi.match).toHaveBeenCalledTimes(1));
    // A count of what it assigned is only half the answer: the rest landed in
    // the band the scorer refuses to call, and nothing else on this page would
    // say so.
    expect(notifyDone).toHaveBeenCalledWith('Matched 2 channels, 3 need a decision');
  });

  it('counts one of each the way a sentence does', async () => {
    const user = userEvent.setup();
    epgApi.match.mockResolvedValue({ matched: 1, need_a_decision: 1 });
    renderWithProviders(<Sources />);

    await user.click(await button());

    await waitFor(() => expect(notifyDone).toHaveBeenCalled());
    expect(notifyDone).toHaveBeenCalledWith('Matched 1 channel, 1 needs a decision');
  });

  it('says only what happened when the matcher decided everything', async () => {
    const user = userEvent.setup();
    epgApi.match.mockResolvedValue({ matched: 4, need_a_decision: 0 });
    renderWithProviders(<Sources />);

    await user.click(await button());

    await waitFor(() => expect(notifyDone).toHaveBeenCalled());
    // Nothing to decide is not news; "0 need a decision" is noise on the one
    // run that went perfectly.
    expect(notifyDone).toHaveBeenCalledWith('Matched 4 channels');
  });

  it('reloads the decisions so the new ones appear at once', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    epgApi.ambiguous.mockResolvedValue([match(1, 'PLOV-DT', 'KAZ 2 St. Louis', 71)]);
    await user.click(await button());

    // The alert above the button is the list this run just added to; leaving it
    // stale is the same failure as a count with no names.
    expect(
      await screen.findByText('1 channel needs a guide decision'),
    ).toBeInTheDocument();
  });

  it('cannot be fired twice while the first run is in flight', async () => {
    const user = userEvent.setup();
    let release;
    epgApi.match.mockReturnValue(new Promise((resolve) => (release = resolve)));
    renderWithProviders(<Sources />);

    await user.click(await button());

    await waitFor(() =>
      expect(screen.getByRole('button', { name: /Match unmapped/ })).toBeDisabled(),
    );
    await user.click(screen.getByRole('button', { name: /Match unmapped/ }));
    expect(epgApi.match).toHaveBeenCalledTimes(1);

    release({ matched: 0, need_a_decision: 0 });
    await waitFor(() =>
      expect(
        screen.getByRole('button', { name: 'Match unmapped channels' }),
      ).toBeEnabled(),
    );
  });

  it('surfaces a failure and stays pressable', async () => {
    const user = userEvent.setup();
    epgApi.match.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    renderWithProviders(<Sources />);

    await user.click(await button());

    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith(
        'Could not match channels',
        expect.anything(),
      ),
    );
    expect(await button()).toBeEnabled();
  });
});

describe('stale stream deletion', () => {
  it('spells out that nothing is deleted at zero', () => {
    expect(describeStaleDays(0)).toMatch(/Nothing is deleted automatically/);
    expect(describeStaleDays(null)).toMatch(/Nothing is deleted automatically/);
  });

  it('spells out what a provider outage costs', () => {
    // The number that decides whether a hiccup takes out the lineup.
    expect(describeStaleDays(7)).toMatch(/deleted after 7 days/);
    expect(describeStaleDays(7)).toMatch(/outage lasting longer than that/);
    expect(describeStaleDays(1)).toMatch(/after 1 day\b/);
  });

  it('shows the consequence beside the field', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Edit HD Homerun' }));
    const dialog = await screen.findByRole('dialog');

    expect(within(dialog).getByText(/deleted after 7 days/)).toBeInTheDocument();
  });

  it('is absent for an EPG source, which has no such setting', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('SaintLouisMO-OTA.xml');

    await user.click(screen.getByRole('button', { name: 'Edit SaintLouisMO-OTA.xml' }));
    const dialog = await screen.findByRole('dialog');

    expect(
      within(dialog).queryByLabelText(/Delete stale streams/),
    ).not.toBeInTheDocument();
  });
});

describe('editing sources', () => {
  it('creates an M3U account', async () => {
    const user = userEvent.setup();
    m3uApi.create.mockResolvedValue({ id: 9 });
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Add M3U' }));
    const dialog = await screen.findByRole('dialog');
    await user.type(within(dialog).getByLabelText(/Name/), 'New provider');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(m3uApi.create).toHaveBeenCalledWith(
        expect.objectContaining({ name: 'New provider', account_type: 'standard' }),
      ),
    );
  });

  it('explains what a dummy EPG source does and asks for no URL', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('SaintLouisMO-OTA.xml');

    await user.click(screen.getByRole('button', { name: 'Add EPG' }));
    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByRole('textbox', { name: 'Type' }));
    await user.click(
      await screen.findByRole('option', { name: 'Dummy (generated)', hidden: true }),
    );

    expect(
      within(dialog).getByText(/generates placeholder programmes/i),
    ).toBeInTheDocument();
    expect(within(dialog).queryByLabelText(/^URL/)).not.toBeInTheDocument();
  });

  it('deletes only after a confirmation that names the consequence', async () => {
    const user = userEvent.setup();
    m3uApi.remove.mockResolvedValue(null);
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Delete HD Homerun' }));
    expect(await screen.findByText(/Its streams go with it/)).toBeInTheDocument();
    expect(m3uApi.remove).not.toHaveBeenCalled();

    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete' }),
    );
    await waitFor(() => expect(m3uApi.remove).toHaveBeenCalledWith(2));
  });

  it('will not delete a locked source', async () => {
    m3uApi.list.mockResolvedValue([{ ...ACCOUNT, locked: true }]);
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    expect(screen.getByRole('button', { name: 'Delete HD Homerun' })).toBeDisabled();
  });

  it('toggles a source active without opening the editor', async () => {
    const user = userEvent.setup();
    m3uApi.update.mockResolvedValue({ ...ACCOUNT, is_active: false });
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('switch', { name: 'Activate HD Homerun' }));

    await waitFor(() =>
      expect(m3uApi.update).toHaveBeenCalledWith(2, { is_active: false }),
    );
  });

  it('surfaces a load failure for each table independently', async () => {
    m3uApi.list.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    renderWithProviders(<Sources />);

    expect(await screen.findByText('Accounts could not be loaded.')).toBeInTheDocument();
    // The EPG table is unaffected.
    expect(await screen.findByText('SaintLouisMO-OTA.xml')).toBeInTheDocument();
  });

  it('tells a new user what each empty table means', async () => {
    m3uApi.list.mockResolvedValue([]);
    epgApi.list.mockResolvedValue([]);
    renderWithProviders(<Sources />);

    expect(await screen.findByText(/No providers yet/)).toBeInTheDocument();
    expect(screen.getByText(/no programme information/)).toBeInTheDocument();
  });
});

describe('import rules', () => {
  it('summarises the groups and sends the operator to the Groups page for the rest', async () => {
    const user = userEvent.setup();
    m3uApi.groups.mockResolvedValue([
      { channel_group: 7, name: 'Default Group', enabled: true, auto_channel_sync: true },
      { channel_group: 8, name: 'Adult', enabled: false, auto_channel_sync: false },
    ]);
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Import rules for HD Homerun' }));

    expect(
      await screen.findByText('Importing 1 of 2 groups, 1 auto-synced.'),
    ).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Manage groups →' })).toHaveAttribute(
      'href',
      '/groups?account=2',
    );
    // The toggle moved with the rest of a group's settings: one place to edit
    // a provider link, so two screens cannot disagree about it.
    expect(
      screen.queryByRole('switch', { name: 'Import Adult' }),
    ).not.toBeInTheDocument();
  });

  it('says when a refresh has not produced any groups yet', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Import rules for HD Homerun' }));

    expect(await screen.findByText(/No groups yet/)).toBeInTheDocument();
  });
});

describe('polling while work is in flight', () => {
  it('re-reads the tables while a refresh is running', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    m3uApi.list.mockResolvedValue([FETCHING]);
    renderWithProviders(<Sources />);
    await screen.findByText('Backup provider');

    m3uApi.list.mockClear();
    await vi.advanceTimersByTimeAsync(5000);

    expect(m3uApi.list).toHaveBeenCalled();
    vi.useRealTimers();
  });

  it('does not poll an idle server', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    m3uApi.list.mockClear();
    await vi.advanceTimersByTimeAsync(10_000);

    // Watching nothing happen every two seconds is a load source with no reader.
    expect(m3uApi.list).not.toHaveBeenCalled();
    vi.useRealTimers();
  });
});

describe('more editing paths', () => {
  it('refreshes every EPG source through the import endpoint', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('SaintLouisMO-OTA.xml');

    const all = screen.getAllByRole('button', { name: 'Refresh all' });
    await user.click(all[1]);

    await waitFor(() => expect(epgApi.refreshAll).toHaveBeenCalled());
  });

  it('creates an EPG source', async () => {
    const user = userEvent.setup();
    epgApi.create.mockResolvedValue({ id: 9 });
    renderWithProviders(<Sources />);
    await screen.findByText('SaintLouisMO-OTA.xml');

    await user.click(screen.getByRole('button', { name: 'Add EPG' }));
    const dialog = await screen.findByRole('dialog');
    await user.type(within(dialog).getByLabelText(/Name/), 'Local XMLTV');
    await user.type(within(dialog).getByLabelText(/^URL/), 'http://guide.test/x.xml');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(epgApi.create).toHaveBeenCalledWith(
        expect.objectContaining({
          name: 'Local XMLTV',
          source_type: 'xmltv',
          url: 'http://guide.test/x.xml',
        }),
      ),
    );
  });

  it('labels the URL as a server URL for an Xtream account', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Add M3U' }));
    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByRole('textbox', { name: 'Type' }));
    await user.click(
      await screen.findByRole('option', { name: 'Xtream Codes', hidden: true }),
    );

    expect(within(dialog).getByLabelText(/Server URL/)).toBeInTheDocument();
  });

  it('requires a name', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Add M3U' }));
    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    expect(await within(dialog).findByText('Required')).toBeInTheDocument();
    expect(m3uApi.create).not.toHaveBeenCalled();
  });

  it('abandons an edit on cancel', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Edit HD Homerun' }));
    await user.click(
      within(await screen.findByRole('dialog')).getByRole('button', { name: 'Cancel' }),
    );

    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(m3uApi.update).not.toHaveBeenCalled();
  });

  it('reports a failed delete and keeps the row', async () => {
    const user = userEvent.setup();
    m3uApi.remove.mockRejectedValue(new ApiError('In use', { status: 409 }));
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('button', { name: 'Delete HD Homerun' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete' }),
    );

    await waitFor(() => expect(m3uApi.remove).toHaveBeenCalled());
    expect(screen.getByText('HD Homerun')).toBeInTheDocument();
  });

  it('reports a failed activation toggle', async () => {
    const user = userEvent.setup();
    m3uApi.update.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    renderWithProviders(<Sources />);
    await screen.findByText('HD Homerun');

    await user.click(screen.getByRole('switch', { name: 'Activate HD Homerun' }));

    await waitFor(() => expect(m3uApi.update).toHaveBeenCalled());
    expect(screen.getByRole('switch', { name: 'Activate HD Homerun' })).toBeChecked();
  });

  it('reports a cancel that finds nothing to stop', async () => {
    const user = userEvent.setup();
    m3uApi.list.mockResolvedValue([FETCHING]);
    jobsApi.list.mockResolvedValue([RUNNING_JOB]);
    jobsApi.cancel.mockResolvedValue({ cancelling: false });
    renderWithProviders(<Sources />);

    await user.click(
      await screen.findByRole('button', { name: 'Cancel refresh of Backup provider' }),
    );

    await waitFor(() => expect(jobsApi.cancel).toHaveBeenCalled());
  });

  it('reports a cancel that failed outright', async () => {
    const user = userEvent.setup();
    m3uApi.list.mockResolvedValue([FETCHING]);
    jobsApi.list.mockResolvedValue([RUNNING_JOB]);
    jobsApi.cancel.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    renderWithProviders(<Sources />);

    await user.click(
      await screen.findByRole('button', { name: 'Cancel refresh of Backup provider' }),
    );

    await waitFor(() => expect(jobsApi.cancel).toHaveBeenCalled());
    expect(screen.getByText('Backup provider')).toBeInTheDocument();
  });

  it('shows a source with no URL and no file as a dash', async () => {
    epgApi.list.mockResolvedValue([{ ...SOURCE, url: null, file_path: null }]);
    renderWithProviders(<Sources />);
    await screen.findByText('SaintLouisMO-OTA.xml');

    expect(cellText('Guide sources', 'URL / File')).toEqual(['—']);
  });

  it('shows a dash when a source has never reported a result', async () => {
    epgApi.list.mockResolvedValue([{ ...SOURCE, last_message: null, status: 'idle' }]);
    renderWithProviders(<Sources />);
    await screen.findByText('SaintLouisMO-OTA.xml');

    expect(cellText('Guide sources', 'Last result')).toEqual(['—']);
  });
});

describe('table columns', () => {
  it('filters accounts by status', async () => {
    const user = userEvent.setup();
    m3uApi.list.mockResolvedValue([ACCOUNT, FETCHING]);
    renderWithProviders(<Sources />);
    await screen.findByText('Backup provider');

    const table = screen.getByRole('table', { name: 'M3U accounts' });
    await user.type(
      within(table).getByRole('textbox', { name: 'Search status' }),
      'runn',
    );

    expect(cellText('M3U accounts', 'Name')).toEqual(['Backup provider']);
  });

  it('filters accounts by what the last refresh reported', async () => {
    const user = userEvent.setup();
    m3uApi.list.mockResolvedValue([ACCOUNT, FETCHING]);
    renderWithProviders(<Sources />);
    await screen.findByText('Backup provider');

    const table = screen.getByRole('table', { name: 'M3U accounts' });
    await user.type(
      within(table).getByRole('textbox', { name: 'Search last_message' }),
      'unchanged',
    );

    expect(cellText('M3U accounts', 'Name')).toEqual(['HD Homerun']);
  });

  it('sorts accounts by when they last refreshed', async () => {
    const user = userEvent.setup();
    m3uApi.list.mockResolvedValue([
      { ...ACCOUNT, name: 'Older', updated_at: '2026-01-01T00:00:00Z' },
      { ...ACCOUNT, id: 4, name: 'Newer', updated_at: '2026-09-01T00:00:00Z' },
    ]);
    renderWithProviders(<Sources />);
    await screen.findByText('Older');

    const table = screen.getByRole('table', { name: 'M3U accounts' });
    await user.click(within(table).getByRole('button', { name: /Refreshed/ }));

    expect(cellText('M3U accounts', 'Name')).toEqual(['Older', 'Newer']);
  });

  it('sorts EPG sources by when they last refreshed', async () => {
    const user = userEvent.setup();
    epgApi.list.mockResolvedValue([
      { ...SOURCE, name: 'Second', updated_at: '2026-09-01T00:00:00Z' },
      { ...SOURCE, id: 5, name: 'First', updated_at: '2026-01-01T00:00:00Z' },
    ]);
    renderWithProviders(<Sources />);
    await screen.findByText('Second');

    const table = screen.getByRole('table', { name: 'Guide sources' });
    await user.click(within(table).getByRole('button', { name: /Refreshed/ }));

    expect(cellText('Guide sources', 'Name')).toEqual(['First', 'Second']);
  });
});
