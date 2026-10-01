import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Channels } from './Channels.jsx';
import {
  channelGroups,
  channelProfiles,
  channels as channelsApi,
  logos as logosApi,
  streams as streamsApi,
  streamProfiles as streamProfilesApi,
} from '../api/resources.js';
import { renderWithProviders } from '../test-utils.jsx';
import { notifyError } from '../notify.js';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  channels: {
    list: vi.fn(),
    create: vi.fn(),
    update: vi.fn(),
    remove: vi.fn(),
    bulkDelete: vi.fn(),
    streams: vi.fn(),
    setStreams: vi.fn(),
    move: vi.fn(),
  },
  channelGroups: { list: vi.fn() },
  channelProfiles: { list: vi.fn(), setMembership: vi.fn() },
  streams: { list: vi.fn(), bulkDelete: vi.fn() },
  logos: { list: vi.fn() },
  streamProfiles: { list: vi.fn() },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

/** Shaped exactly as `channels::serialize` emits it. */
const CHANNELS = [
  {
    id: 1,
    uuid: 'a',
    // Overridden: the provider row says one thing, the effective view another.
    name: 'PROVIDER PLOV',
    channel_number: 99,
    channel_group_id: 7,
    logo_id: 3,
    tvg_id: '21300',
    epg_data_id: 11,
    stream_profile_id: null,
    hidden_from_output: false,
    streams: [100, 101],
    override: {
      name: 'PLOV-DT',
      channel_number: 2.1,
      channel_group_id: 8,
      logo_id: null,
      tvg_id: null,
      tvc_guide_stationid: null,
      epg_data_id: null,
      stream_profile_id: null,
    },
    effective_name: 'PLOV-DT',
    effective_channel_number: 2.1,
    effective_tvg_id: '21300',
    effective_epg_data_id: 11,
    epg_name: 'KAZ St. Louis',
    // The override moved it to Locals; the base row still says group 7.
    group_name: 'Locals',
    logo_url: 'http://logos.test/kaz.png',
  },
  {
    id: 2,
    uuid: 'b',
    name: 'KPLR-DT',
    channel_number: 11.1,
    channel_group_id: 8,
    logo_id: null,
    tvg_id: null,
    epg_data_id: null,
    stream_profile_id: null,
    hidden_from_output: false,
    streams: [],
    override: null,
    effective_name: 'KPLR-DT',
    effective_channel_number: 11.1,
    // A provider label and no guide data: having a `tvg-id` says nothing about
    // whether this channel has listings.
    effective_tvg_id: '34808',
    effective_epg_data_id: null,
    epg_name: null,
    group_name: 'Default Group',
    logo_url: null,
  },
  {
    id: 3,
    uuid: 'c',
    name: 'Unnumbered',
    channel_number: null,
    channel_group_id: null,
    logo_id: null,
    tvg_id: null,
    epg_data_id: null,
    stream_profile_id: null,
    // Kept out of HDHR, M3U and EPG output — invisible to Plex.
    hidden_from_output: true,
    streams: [],
    override: null,
    effective_name: 'Unnumbered',
    effective_channel_number: null,
    // Mapped by the fuzzy matcher, which writes `epg_data_id` and never
    // `tvg_id` — the pairing a column serialized from `effective_tvg_id`
    // reports as "no EPG".
    effective_tvg_id: null,
    effective_epg_data_id: 12,
    epg_name: 'CW Plus',
    group_name: null,
    logo_url: null,
  },
];

const GROUPS = [
  { id: 7, name: 'Default Group', channel_count: 1, stream_count: 40 },
  { id: 8, name: 'Locals', channel_count: 1, stream_count: 11 },
];

const STREAMS = [
  { id: 100, name: '1stAlrt', url: 'http://a', channel_group_id: 7, is_custom: false },
  { id: 101, name: 'ANTENNA', url: 'http://b', channel_group_id: 8, is_custom: false },
];

function bodyRows(label) {
  const body = screen.getByRole('table', { name: label }).querySelector('tbody');
  return within(body).queryAllByRole('row');
}

/**
 * Reads a column by its header rather than its position, so inserting a column
 * does not silently repoint every assertion at the wrong one.
 */
function cellText(label, header) {
  const table = screen.getByRole('table', { name: label });
  const headers = [...table.querySelectorAll('thead tr:first-child th')];
  const index = headers.findIndex((th) => th.textContent.trim() === header);
  if (index === -1) throw new Error(`no column headed "${header}" in ${label}`);
  return bodyRows(label).map(
    (row) => row.querySelectorAll('td')[index]?.textContent ?? '',
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  channelsApi.list.mockResolvedValue(CHANNELS);
  channelsApi.streams.mockResolvedValue([]);
  channelGroups.list.mockResolvedValue(GROUPS);
  channelProfiles.list.mockResolvedValue([{ id: 1, name: 'Default', channels: [] }]);
  streamsApi.list.mockResolvedValue({ results: STREAMS, count: 51 });
  logosApi.list.mockResolvedValue([
    { id: 3, name: 'kaz', url: 'http://logos.test/kaz.png' },
  ]);
  streamProfilesApi.list.mockResolvedValue([{ id: 1, name: 'Proxy' }]);
  channelsApi.move.mockResolvedValue({ id: 3, channel_number: 6.6 });
});

describe('dragging a channel', () => {
  it('moves it between the rows it was dropped between, and only then', async () => {
    renderWithProviders(<Channels />);
    await screen.findByText('KPLR-DT');
    expect(cellText('Channels', 'Name')).toEqual(['PLOV-DT', 'KPLR-DT', 'Unnumbered']);

    // Dragged up over KPLR-DT: the rows move under the pointer...
    const grip = screen.getByRole('button', { name: 'Move Unnumbered' });
    fireEvent.dragStart(grip);
    fireEvent.dragOver(bodyRows('Channels')[1]);
    expect(cellText('Channels', 'Name')).toEqual(['PLOV-DT', 'Unnumbered', 'KPLR-DT']);
    // ...and nothing is written until the mouse comes up, because the number
    // depends on where it finally lands.
    expect(channelsApi.move).not.toHaveBeenCalled();

    fireEvent.dragEnd(grip);
    await waitFor(() =>
      expect(channelsApi.move).toHaveBeenCalledWith(3, { after: 1, before: 2 }),
    );
    // The server decides the number, so the lineup is re-read rather than
    // guessed at.
    await waitFor(() => expect(channelsApi.list).toHaveBeenCalledTimes(2));
  });

  it('names an end of the list as having no neighbour there', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('KPLR-DT');

    // To the bottom, where there is no row below to be numbered under.
    screen.getByRole('button', { name: 'Move KPLR-DT' }).focus();
    await user.keyboard('{ArrowDown}');

    await waitFor(() =>
      expect(channelsApi.move).toHaveBeenCalledWith(2, { after: 3, before: null }),
    );

    // And to the top, where there is no row above: the server numbers it from
    // the group's range rather than from a neighbour.
    screen.getByRole('button', { name: 'Move KPLR-DT' }).focus();
    await user.keyboard('{ArrowUp}');

    await waitFor(() =>
      expect(channelsApi.move).toHaveBeenLastCalledWith(2, { after: null, before: 1 }),
    );
  });

  it('says why a refused move was refused, and re-reads the lineup', async () => {
    const user = userEvent.setup();
    channelsApi.move.mockRejectedValue(
      new ApiError('there is no room between 2.1 and 2.11; renumber Locals', {
        status: 400,
      }),
    );
    renderWithProviders(<Channels />);
    await screen.findByText('KPLR-DT');

    screen.getByRole('button', { name: 'Move Unnumbered' }).focus();
    await user.keyboard('{ArrowUp}');

    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith(
        'Could not move the channel',
        expect.objectContaining({ message: expect.stringContaining('no room') }),
      ),
    );
    // The order on screen was a guess; the server's is read back over it.
    await waitFor(() => expect(channelsApi.list).toHaveBeenCalledTimes(2));
  });
});

describe('Channels page', () => {
  it('shows both panes of the split', async () => {
    renderWithProviders(<Channels />);

    expect(await screen.findByRole('table', { name: 'Channels' })).toBeInTheDocument();
    expect(screen.getByRole('table', { name: 'Streams' })).toBeInTheDocument();
  });

  it('names the guide each channel is mapped to, not the provider label', async () => {
    renderWithProviders(<Channels />);
    await screen.findByText('KPLR-DT');

    // Row 3 is mapped and carries no `tvg-id` at all, which is what fuzzy
    // matching leaves behind; row 2 carries a label and maps to nothing. A
    // column serialized from `effective_tvg_id` gets both of them backwards.
    expect(cellText('Channels', 'EPG')).toEqual(['KAZ St. Louis', '—', 'CW Plus']);
  });

  it('keeps a fractional channel number as a float', async () => {
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    const numbers = cellText('Channels', '#');
    expect(numbers).toContain('2.1');
    expect(numbers).toContain('11.1');
    expect(numbers).not.toContain('2');
  });

  it('shows an unnumbered channel as a dash, never as zero', async () => {
    renderWithProviders(<Channels />);
    await screen.findByText('Unnumbered');

    const numbers = cellText('Channels', '#');
    expect(numbers).toContain('—');
    expect(numbers).not.toContain('0');
  });

  it('sorts channel numbers numerically rather than as strings', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('button', { name: /^#/ }));

    // Lexicographically "11.1" sorts before "2.1"; numerically it does not.
    // The unnumbered row is included on purpose: the server puts it last, and
    // an admin sorting a different order than Plex receives is the bug.
    expect(cellText('Channels', '#')).toEqual(['2.1', '11.1', '—']);
  });

  it('keeps unnumbered channels last on ascending, as the lineup does', async () => {
    const user = userEvent.setup();
    // Reversed input: a comparator that coerces null to 0 gives a different
    // answer depending on the order it started from.
    channelsApi.list.mockResolvedValue([...CHANNELS].reverse());
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('button', { name: /^#/ }));

    expect(cellText('Channels', '#')).toEqual(['2.1', '11.1', '—']);
  });

  it('puts unnumbered channels first on descending, matching the server', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    const header = screen.getByRole('button', { name: /^#/ });
    await user.click(header);
    await user.click(header);

    // `channel_number IS NULL DESC` puts nulls first.
    expect(cellText('Channels', '#')).toEqual(['—', '11.1', '2.1']);
  });

  it('filters the lineup by group', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('textbox', { name: 'Filter by group' }));
    await user.click(
      await screen.findByRole('option', { name: 'Locals (1)', hidden: true }),
    );

    // PLOV-DT's base `channel_group_id` is 7, but its override puts it in
    // Locals — which is what the Group column shows. Filtering on the base id
    // would return nothing for exactly this row.
    expect(cellText('Channels', 'Name')).toEqual(['PLOV-DT']);
  });

  it('filters the lineup by guide, offering only the guides in use', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('textbox', { name: 'Filter by guide' }));
    expect(
      await screen.findByRole('option', { name: 'KAZ St. Louis', hidden: true }),
    ).toBeInTheDocument();
    // The provider labels are not guides and have no business in this list.
    expect(
      screen.queryByRole('option', { name: '21300', hidden: true }),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole('option', { name: 'KAZ St. Louis', hidden: true }));
    expect(cellText('Channels', 'Name')).toEqual(['PLOV-DT']);
  });

  /** The question an operator actually has: which channels will Plex show an
   * empty guide strip for. A list of guide names cannot ask it. */
  it('filters down to the channels with no guide at all', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('textbox', { name: 'Filter by guide' }));
    await user.click(
      await screen.findByRole('option', { name: 'No guide', hidden: true }),
    );

    // KPLR-DT has a `tvg-id` and no mapping, so it is the one with no guide.
    expect(cellText('Channels', 'Name')).toEqual(['KPLR-DT']);
  });

  it('brings every channel back when the guide filter is cleared', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('textbox', { name: 'Filter by guide' }));
    await user.click(
      await screen.findByRole('option', { name: 'No guide', hidden: true }),
    );
    expect(cellText('Channels', 'Name')).toEqual(['KPLR-DT']);

    // Mantine marks its own clear button `aria-hidden`, so it has no role to
    // query; it is still the only button inside the field.
    const field = screen
      .getByRole('textbox', { name: 'Filter by guide' })
      .closest('[class*="mantine-Input-wrapper"]');
    await user.click(within(field).getByRole('button', { hidden: true }));
    expect(cellText('Channels', 'Name')).toEqual(['PLOV-DT', 'KPLR-DT', 'Unnumbered']);
  });

  it('surfaces a load failure rather than showing an empty lineup', async () => {
    channelsApi.list.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderWithProviders(<Channels />);

    expect(await screen.findByText('Channels could not be loaded.')).toBeInTheDocument();
  });

  it('copies each output URL from the header', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    const links = screen.getByRole('group', { name: 'Output links' });
    // Built from the Connect page's base picker, which defaults to this
    // browser — so these and that screen can never disagree.
    const expected = {
      HDHR: 'http://localhost:3000/hdhr/',
      M3U: 'http://localhost:3000/output/m3u',
      EPG: 'http://localhost:3000/output/epg',
    };

    for (const [label, url] of Object.entries(expected)) {
      await user.click(within(links).getByRole('button', { name: `Copy ${label} URL` }));
      await waitFor(async () => expect(await navigator.clipboard.readText()).toBe(url));
    }
  });

  it('deletes a single channel and reloads', async () => {
    const user = userEvent.setup();
    channelsApi.remove.mockResolvedValue(null);
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('button', { name: 'Delete PLOV-DT' }));
    await user.click(await screen.findByRole('button', { name: 'Delete', exact: true }));

    await waitFor(() => expect(channelsApi.remove).toHaveBeenCalledWith(1));
    expect(channelsApi.list).toHaveBeenCalledTimes(2);
  });

  it('bulk-deletes the selected channels by numeric id', async () => {
    const user = userEvent.setup();
    channelsApi.bulkDelete.mockResolvedValue({ deleted: 1 });
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    const table = screen.getByRole('table', { name: 'Channels' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 1' }));
    await user.click(screen.getByRole('button', { name: 'Bulk actions' }));
    await user.click(await screen.findByRole('menuitem', { name: /Delete 1 channels/ }));
    await user.click(await screen.findByRole('button', { name: 'Delete', exact: true }));

    // Row ids are strings; the API takes numbers.
    await waitFor(() => expect(channelsApi.bulkDelete).toHaveBeenCalledWith([1]));
  });

  it('assigns the selection to a channel profile', async () => {
    const user = userEvent.setup();
    channelProfiles.setMembership.mockResolvedValue({ updated: 1 });
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    const table = screen.getByRole('table', { name: 'Channels' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 2' }));
    await user.click(screen.getByRole('button', { name: 'Bulk actions' }));
    await user.click(await screen.findByRole('menuitem', { name: 'Enable in Default' }));

    await waitFor(() =>
      expect(channelProfiles.setMembership).toHaveBeenCalledWith(1, [2], true),
    );
  });
});

describe('Channels page failure handling', () => {
  it('keeps the row when a delete is rejected', async () => {
    const user = userEvent.setup();
    channelsApi.remove.mockRejectedValue(new ApiError('In use', { status: 409 }));
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('button', { name: 'Delete PLOV-DT' }));
    await user.click(await screen.findByRole('button', { name: 'Delete', exact: true }));

    await waitFor(() => expect(channelsApi.remove).toHaveBeenCalled());
    // Not reloaded, so the row is still on screen for another attempt.
    expect(channelsApi.list).toHaveBeenCalledTimes(1);
    expect(screen.getByText('PLOV-DT')).toBeInTheDocument();
  });

  it('survives a rejected bulk delete', async () => {
    const user = userEvent.setup();
    channelsApi.bulkDelete.mockRejectedValue(new ApiError('Nope', { status: 500 }));
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    const table = screen.getByRole('table', { name: 'Channels' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 1' }));
    await user.click(screen.getByRole('button', { name: 'Bulk actions' }));
    await user.click(await screen.findByRole('menuitem', { name: /Delete 1 channels/ }));
    await user.click(await screen.findByRole('button', { name: 'Delete', exact: true }));

    await waitFor(() => expect(channelsApi.bulkDelete).toHaveBeenCalled());
    expect(screen.getByText('PLOV-DT')).toBeInTheDocument();
  });

  it('survives a rejected profile assignment', async () => {
    const user = userEvent.setup();
    channelProfiles.setMembership.mockRejectedValue(
      new ApiError('Nope', { status: 500 }),
    );
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    const table = screen.getByRole('table', { name: 'Channels' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 1' }));
    await user.click(screen.getByRole('button', { name: 'Bulk actions' }));
    await user.click(await screen.findByRole('menuitem', { name: 'Disable in Default' }));

    await waitFor(() => expect(channelProfiles.setMembership).toHaveBeenCalled());
    expect(screen.getByText('PLOV-DT')).toBeInTheDocument();
  });

  it('says when there are no profiles to assign to', async () => {
    const user = userEvent.setup();
    channelProfiles.list.mockResolvedValue([]);
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    const table = screen.getByRole('table', { name: 'Channels' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 1' }));
    await user.click(screen.getByRole('button', { name: 'Bulk actions' }));

    expect(
      await screen.findByRole('menuitem', { name: 'No profiles' }),
    ).toBeInTheDocument();
  });
});

describe('Streams pane', () => {
  it('asks the server for a page rather than filtering locally', async () => {
    renderWithProviders(<Channels />);

    await waitFor(() => expect(streamsApi.list).toHaveBeenCalled());
    expect(streamsApi.list).toHaveBeenLastCalledWith(
      expect.objectContaining({ page: 1, pageSize: 50 }),
    );
  });

  it('reports the server row count, not the length of the page', async () => {
    renderWithProviders(<Channels />);

    // Two rows on the page, 51 on the server.
    expect(await screen.findByText('1 to 2 of 51')).toBeInTheDocument();
  });

  it('sends the search term to the server', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    await user.type(screen.getByRole('textbox', { name: 'Search streams' }), 'antenna');

    await waitFor(() =>
      expect(streamsApi.list).toHaveBeenLastCalledWith(
        expect.objectContaining({ search: 'antenna', page: 1 }),
      ),
    );
  });

  it('translates a column sort into the server ordering vocabulary', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    const table = screen.getByRole('table', { name: 'Streams' });
    await user.click(within(table).getByRole('button', { name: /Name/ }));

    await waitFor(() =>
      expect(streamsApi.list).toHaveBeenLastCalledWith(
        expect.objectContaining({ ordering: 'name' }),
      ),
    );
  });

  it('appends a stream to a channel rather than replacing its failover list', async () => {
    const user = userEvent.setup();
    channelsApi.streams.mockResolvedValue([{ id: 100, name: '1stAlrt' }]);
    channelsApi.setStreams.mockResolvedValue([]);
    renderWithProviders(<Channels />);
    await screen.findByText('ANTENNA');

    await user.click(screen.getByRole('button', { name: 'Add ANTENNA to a channel' }));
    await user.click(await screen.findByRole('menuitem', { name: 'KPLR-DT' }));

    // Existing first, new one last: a new stream is a fallback, not a
    // replacement for whatever is currently working.
    await waitFor(() =>
      expect(channelsApi.setStreams).toHaveBeenCalledWith(2, [100, 101]),
    );
  });

  it('refuses to add a stream a channel already has', async () => {
    const user = userEvent.setup();
    channelsApi.streams.mockResolvedValue([{ id: 101, name: 'ANTENNA' }]);
    renderWithProviders(<Channels />);
    await screen.findByText('ANTENNA');

    await user.click(screen.getByRole('button', { name: 'Add ANTENNA to a channel' }));
    await user.click(await screen.findByRole('menuitem', { name: 'KPLR-DT' }));

    await waitFor(() => expect(channelsApi.streams).toHaveBeenCalled());
    expect(channelsApi.setStreams).not.toHaveBeenCalled();
  });

  it('creates a channel from a stream, carrying its group and guide id', async () => {
    const user = userEvent.setup();
    channelsApi.create.mockResolvedValue({ id: 9 });
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    await user.click(screen.getByRole('button', { name: 'Create channel from 1stAlrt' }));

    await waitFor(() =>
      expect(channelsApi.create).toHaveBeenCalledWith({
        name: '1stAlrt',
        channel_group_id: 7,
        tvg_id: null,
        streams: [100],
      }),
    );
  });

  it('bulk-deletes selected streams', async () => {
    const user = userEvent.setup();
    streamsApi.bulkDelete.mockResolvedValue({ deleted: 1 });
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    const table = screen.getByRole('table', { name: 'Streams' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 100' }));
    await user.click(screen.getByRole('button', { name: 'Delete' }));
    await user.click(
      await within(screen.getByRole('dialog')).findByRole('button', { name: 'Delete' }),
    );

    await waitFor(() => expect(streamsApi.bulkDelete).toHaveBeenCalledWith([100]));
  });
});

describe('Streams pane failure handling', () => {
  it('survives a rejected add-to-channel', async () => {
    const user = userEvent.setup();
    channelsApi.streams.mockResolvedValue([]);
    channelsApi.setStreams.mockRejectedValue(new ApiError('Nope', { status: 500 }));
    renderWithProviders(<Channels />);
    await screen.findByText('ANTENNA');

    await user.click(screen.getByRole('button', { name: 'Add ANTENNA to a channel' }));
    await user.click(await screen.findByRole('menuitem', { name: 'KPLR-DT' }));

    await waitFor(() => expect(channelsApi.setStreams).toHaveBeenCalled());
    expect(screen.getByText('ANTENNA')).toBeInTheDocument();
  });

  it('survives a rejected channel creation', async () => {
    const user = userEvent.setup();
    channelsApi.create.mockRejectedValue(new ApiError('Nope', { status: 400 }));
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    await user.click(screen.getByRole('button', { name: 'Create channel from 1stAlrt' }));

    await waitFor(() => expect(channelsApi.create).toHaveBeenCalled());
    expect(screen.getByText('1stAlrt')).toBeInTheDocument();
  });

  it('survives a rejected bulk delete', async () => {
    const user = userEvent.setup();
    streamsApi.bulkDelete.mockRejectedValue(new ApiError('Nope', { status: 500 }));
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    const table = screen.getByRole('table', { name: 'Streams' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 100' }));
    await user.click(screen.getByRole('button', { name: 'Delete' }));
    await user.click(
      await within(screen.getByRole('dialog')).findByRole('button', { name: 'Delete' }),
    );

    await waitFor(() => expect(streamsApi.bulkDelete).toHaveBeenCalled());
    expect(screen.getByText('1stAlrt')).toBeInTheDocument();
  });

  it('narrows the stream page by group on the server', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    await user.click(screen.getByRole('textbox', { name: 'Filter streams by group' }));
    await user.click(
      await screen.findByRole('option', { name: 'Locals (11)', hidden: true }),
    );

    await waitFor(() =>
      expect(streamsApi.list).toHaveBeenLastCalledWith(
        expect.objectContaining({ channelGroup: 8 }),
      ),
    );
  });

  it('says when there are no channels to add a stream to', async () => {
    const user = userEvent.setup();
    channelsApi.list.mockResolvedValue([]);
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    await user.click(screen.getByRole('button', { name: 'Add 1stAlrt to a channel' }));

    expect(
      await screen.findByRole('menuitem', { name: 'No channels' }),
    ).toBeInTheDocument();
  });
});

describe('the channel editor opened from the lineup', () => {
  it('opens with the channel loaded and closes again', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('button', { name: 'Edit PLOV-DT' }));

    const dialog = await screen.findByRole('dialog');
    // Channel 1 is overridden, so the editor shows the effective name.
    expect(within(dialog).getByRole('textbox', { name: /^Name/ })).toHaveValue('PLOV-DT');

    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
  });

  it('reloads the lineup after a save', async () => {
    const user = userEvent.setup();
    channelsApi.update.mockResolvedValue(CHANNELS[0]);
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('button', { name: 'Edit PLOV-DT' }));
    const dialog = await screen.findByRole('dialog');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.list).toHaveBeenCalledTimes(2));
  });

  it('opens an empty editor for a new channel', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('button', { name: 'Add' }));

    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByLabelText(/Name/)).toHaveValue('');
  });
});

describe('stream fetching efficiency', () => {
  it('fetches the stream page once per mount, not twice', async () => {
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    // The table reports its query on mount with the values the pane already
    // holds; replacing the object anyway would fire a second identical request.
    await waitFor(() => expect(streamsApi.list).toHaveBeenCalledTimes(1));
  });

  it('shows the spinner again on a refetch, not only on mount', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Channels />);
    await screen.findByText('1stAlrt');

    let release;
    streamsApi.list.mockReturnValue(new Promise((resolve) => (release = resolve)));

    await user.type(screen.getByRole('textbox', { name: 'Search streams' }), 'ant');

    const streamsPane = screen.getByRole('table', { name: 'Streams' }).closest('section');
    await waitFor(() =>
      expect(streamsPane.querySelector('.mantine-Loader-root')).toBeTruthy(),
    );

    release({ results: STREAMS, count: 2 });
  });

  it('will not start a second add while one is in flight', async () => {
    const user = userEvent.setup();
    let release;
    channelsApi.streams.mockReturnValue(new Promise((resolve) => (release = resolve)));
    renderWithProviders(<Channels />);
    await screen.findByText('ANTENNA');

    await user.click(screen.getByRole('button', { name: 'Add ANTENNA to a channel' }));
    await user.click(await screen.findByRole('menuitem', { name: 'KPLR-DT' }));

    await user.click(screen.getByRole('button', { name: 'Add ANTENNA to a channel' }));
    const second = await screen.findByRole('menuitem', { name: 'KPLR-DT' });
    await user.click(second);

    // Appending is a read-modify-write; two in flight would each append to the
    // same stale copy and one would be lost.
    expect(channelsApi.streams).toHaveBeenCalledTimes(1);
    release([]);
  });
});

describe('output visibility', () => {
  it('reflects whether each channel reaches Plex', async () => {
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    expect(
      screen.getByRole('switch', { name: 'Include PLOV-DT in outputs' }),
    ).toBeChecked();
    // hidden_from_output: true — present in the lineup table, absent from
    // HDHR, /output/m3u and /output/epg.
    expect(
      screen.getByRole('switch', { name: 'Include Unnumbered in outputs' }),
    ).not.toBeChecked();
  });

  it('hides a channel from the outputs without deleting it', async () => {
    const user = userEvent.setup();
    channelsApi.update.mockResolvedValue({ ...CHANNELS[0], hidden_from_output: true });
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('switch', { name: 'Include PLOV-DT in outputs' }));

    await waitFor(() =>
      expect(channelsApi.update).toHaveBeenCalledWith(1, { hidden_from_output: true }),
    );
    expect(channelsApi.remove).not.toHaveBeenCalled();
  });

  it('brings a hidden channel back', async () => {
    const user = userEvent.setup();
    channelsApi.update.mockResolvedValue({ ...CHANNELS[2], hidden_from_output: false });
    renderWithProviders(<Channels />);
    await screen.findByText('Unnumbered');

    await user.click(
      screen.getByRole('switch', { name: 'Include Unnumbered in outputs' }),
    );

    await waitFor(() =>
      expect(channelsApi.update).toHaveBeenCalledWith(3, { hidden_from_output: false }),
    );
  });

  it('updates the one row rather than refetching the lineup', async () => {
    const user = userEvent.setup();
    channelsApi.update.mockResolvedValue({ ...CHANNELS[0], hidden_from_output: true });
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('switch', { name: 'Include PLOV-DT in outputs' }));

    await waitFor(() =>
      expect(
        screen.getByRole('switch', { name: 'Include PLOV-DT in outputs' }),
      ).not.toBeChecked(),
    );
    // A reload per toggle makes the table flash on every click.
    expect(channelsApi.list).toHaveBeenCalledTimes(1);
  });

  it('leaves the switch alone when the patch is rejected', async () => {
    const user = userEvent.setup();
    channelsApi.update.mockRejectedValue(new ApiError('Nope', { status: 500 }));
    renderWithProviders(<Channels />);
    await screen.findByText('PLOV-DT');

    await user.click(screen.getByRole('switch', { name: 'Include PLOV-DT in outputs' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(
      screen.getByRole('switch', { name: 'Include PLOV-DT in outputs' }),
    ).toBeChecked();
  });
});
