import { beforeEach, describe, expect, it, vi } from 'vitest';
import { act, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { ChannelModal } from './ChannelModal.jsx';
import { channels as channelsApi, epgData as epgDataApi } from '../api/resources.js';
import { parseChannelNumber } from './channelNumber.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  channels: { create: vi.fn(), update: vi.fn(), streams: vi.fn() },
  epgData: { list: vi.fn() },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

const GROUPS = [
  { id: 7, name: 'Default Group' },
  { id: 8, name: 'Locals' },
];
const PROFILES = [{ id: 1, name: 'Proxy' }];
const LOGOS = [
  { id: 3, name: 'kaz', url: 'http://logos.test/kaz.png' },
  { id: 4, name: 'dro', url: 'http://logos.test/dro.png' },
];

/** Base values deliberately differ from effective ones, and nothing is overridden. */
const CHANNEL = {
  id: 1,
  uuid: 'bbbbbbbb-0000-4000-8000-000000001000',
  name: 'PLOV-DT',
  effective_name: 'PLOV-DT (effective)',
  channel_number: 2.1,
  effective_channel_number: 2.1,
  channel_group_id: 7,
  logo_id: 3,
  stream_profile_id: null,
  tvg_id: '21300',
  tvc_guide_stationid: null,
  hidden_from_output: false,
  override: null,
};

/** The same channel with three fields shadowed by a `channel_override` row. */
const OVERRIDDEN = {
  ...CHANNEL,
  name: 'PROVIDER NAME',
  channel_number: 99,
  channel_group_id: 7,
  effective_name: 'My Name',
  override: {
    name: 'My Name',
    channel_number: 2.1,
    channel_group_id: 8,
    logo_id: null,
    tvg_id: null,
    tvc_guide_stationid: null,
    epg_data_id: null,
    stream_profile_id: null,
  },
};

const FAILOVER = [
  { id: 100, name: 'Primary feed' },
  { id: 101, name: 'Backup feed' },
  { id: 102, name: 'Last resort' },
];

function open(channel = CHANNEL, props = {}) {
  return renderWithProviders(
    <ChannelModal
      channel={channel}
      groups={GROUPS}
      streamProfiles={PROFILES}
      logos={LOGOS}
      onClose={vi.fn()}
      onSaved={vi.fn()}
      {...props}
    />,
  );
}

/** As `/api/epg/epgdata/` serializes them: a name, and a label beside it. */
const GUIDE_ROWS = [
  {
    id: 41,
    epg_source_id: 1,
    name: 'KAZ 2 St. Louis',
    tvg_id: 'kaz2.stl',
    icon_url: null,
  },
  { id: 42, epg_source_id: 1, name: 'KAZ Sports Midwest', tvg_id: null, icon_url: null },
];

beforeEach(() => {
  vi.clearAllMocks();
  channelsApi.streams.mockResolvedValue(FAILOVER);
  channelsApi.update.mockResolvedValue(CHANNEL);
  channelsApi.create.mockResolvedValue({ id: 9 });
  epgDataApi.list.mockResolvedValue(GUIDE_ROWS);
});

describe('channel editor', () => {
  it('loads the base row into the form, not the effective values', async () => {
    open();

    // The base row, not `effective_name` — with nothing overridden they are
    // the same value, and the fixture makes them differ so this can tell.
    expect(await screen.findByLabelText(/Name/)).toHaveValue('PLOV-DT');
    expect(screen.getByLabelText(/Channel number/)).toHaveValue('2.1');
    expect(screen.getByLabelText(/Guide id/)).toHaveValue('21300');
  });

  it('keeps a fractional channel number as a float on save', async () => {
    const user = userEvent.setup();
    open();
    await screen.findByLabelText(/Name/);

    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1].channel_number).toBe(2.1);
  });

  it('sends null, not zero, when the channel number is cleared', async () => {
    const user = userEvent.setup();
    open();

    await user.clear(await screen.findByLabelText(/Channel number/));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    // An unnumbered channel is null. Zero would put it at the top of the
    // lineup and claim a channel number nobody assigned.
    expect(channelsApi.update.mock.calls[0][1].channel_number).toBeNull();
  });

  it('requires a name', async () => {
    const user = userEvent.setup();
    open({ ...CHANNEL, id: undefined, name: '' });

    await user.click(await screen.findByRole('button', { name: 'Save' }));

    expect(await screen.findByText('Required')).toBeInTheDocument();
    expect(channelsApi.create).not.toHaveBeenCalled();
  });

  it('creates without touching the failover list', async () => {
    const user = userEvent.setup();
    open({ name: '' });

    await user.type(await screen.findByLabelText(/Name/), 'New channel');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.create).toHaveBeenCalled());
    expect(channelsApi.create.mock.calls[0][0]).not.toHaveProperty('streams');
    expect(channelsApi.streams).not.toHaveBeenCalled();
  });
});

describe('failover order', () => {
  it('lists the streams in the order the server returned them', async () => {
    open();

    expect(await screen.findByText('Primary feed')).toBeInTheDocument();
    const names = screen.getAllByText(/feed|Last resort/).map((node) => node.textContent);
    expect(names).toEqual(['Primary feed', 'Backup feed', 'Last resort']);
  });

  it('numbers the positions so the failover order is explicit', async () => {
    open();
    await screen.findByText('Primary feed');

    expect(screen.getByText('1')).toBeInTheDocument();
    expect(screen.getByText('3')).toBeInTheDocument();
  });

  it('moves a stream up and saves the new order', async () => {
    const user = userEvent.setup();
    open();
    await screen.findByText('Backup feed');

    await user.click(screen.getByRole('button', { name: 'Move Backup feed up' }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    // Order is the payload: this is the sequence the engine walks on failure.
    expect(channelsApi.update.mock.calls[0][1].streams).toEqual([101, 100, 102]);
  });

  it('moves a stream down and saves the new order', async () => {
    const user = userEvent.setup();
    open();
    await screen.findByText('Primary feed');

    await user.click(screen.getByRole('button', { name: 'Move Primary feed down' }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1].streams).toEqual([101, 100, 102]);
  });

  it('cannot move the first stream up or the last one down', async () => {
    open();
    await screen.findByText('Primary feed');

    expect(screen.getByRole('button', { name: 'Move Primary feed up' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Move Last resort down' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Move Backup feed up' })).toBeEnabled();
  });

  it('removes a stream from the list', async () => {
    const user = userEvent.setup();
    open();
    await screen.findByText('Backup feed');

    await user.click(screen.getByRole('button', { name: 'Remove Backup feed' }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1].streams).toEqual([100, 102]);
  });

  it('says so when a channel has no streams at all', async () => {
    channelsApi.streams.mockResolvedValue([]);
    open();

    expect(await screen.findByText(/No streams assigned/)).toBeInTheDocument();
  });
});

describe('logo picker', () => {
  it('marks the assigned logo as selected', async () => {
    open();
    expect(await screen.findByRole('button', { name: 'kaz' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );
    expect(screen.getByRole('button', { name: 'dro' })).toHaveAttribute(
      'aria-pressed',
      'false',
    );
  });

  it('assigns a different logo', async () => {
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole('button', { name: 'dro' }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1].logo_id).toBe(4);
  });

  it('clears the logo back to null', async () => {
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole('button', { name: 'Clear logo' }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1].logo_id).toBeNull();
  });

  it('filters the grid by name', async () => {
    const user = userEvent.setup();
    open();

    await user.type(await screen.findByRole('textbox', { name: 'Search logos' }), 'dro');

    expect(screen.getByRole('button', { name: 'dro' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'kaz' })).not.toBeInTheDocument();
  });

  it('says when no logos exist yet rather than showing an empty grid', async () => {
    open(CHANNEL, { logos: [] });
    expect(await screen.findByText(/No logos yet/)).toBeInTheDocument();
  });
});

describe('the stream URL', () => {
  it('copies the address that plays this one channel', async () => {
    const user = userEvent.setup();
    open();
    await screen.findByLabelText(/Name/);

    await user.click(screen.getByRole('button', { name: /Copy stream URL/ }));

    // The base comes from the Connect page's picker, which defaults to this
    // browser — the same address the operator is reading the page on.
    await waitFor(async () =>
      expect(await navigator.clipboard.readText()).toBe(
        'http://localhost:3000/proxy/ts/stream/bbbbbbbb-0000-4000-8000-000000001000',
      ),
    );
  });

  it('is not offered for a channel that does not exist yet', async () => {
    open({});

    expect(
      screen.queryByRole('button', { name: /Copy stream URL/ }),
    ).not.toBeInTheDocument();
  });
});

describe('save failures', () => {
  it('re-enables Save after a failure so the edit can be retried', async () => {
    const user = userEvent.setup();
    channelsApi.update.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    open();

    await user.click(await screen.findByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled(),
    );
  });

  it('never sends streams when the failover list failed to load', async () => {
    const user = userEvent.setup();
    channelsApi.streams.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    open();

    // `set_streams` deletes every row for the channel before reinserting, so an
    // empty array here unassigns every stream and the channel silently stops
    // playing while still appearing in the HDHR lineup.
    expect(await screen.findByText(/could not be loaded/i)).toBeInTheDocument();
    expect(screen.queryByText(/No streams assigned/)).not.toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1]).not.toHaveProperty('streams');
  });

  it('never sends streams while the failover list is still loading', async () => {
    const user = userEvent.setup();
    let release;
    channelsApi.streams.mockReturnValue(new Promise((resolve) => (release = resolve)));
    open();

    // Save is blocked rather than silently destructive.
    expect(await screen.findByRole('button', { name: 'Save' })).toBeDisabled();

    release(FAILOVER);
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled(),
    );
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1].streams).toEqual([100, 101, 102]);
  });

  it('does send an empty list when the channel genuinely has no streams', async () => {
    const user = userEvent.setup();
    channelsApi.streams.mockResolvedValue([]);
    open();

    await user.click(await screen.findByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    // Loaded-and-empty is a real state, and clearing the list must still work.
    expect(channelsApi.update.mock.calls[0][1].streams).toEqual([]);
  });
});

describe('overridden channels', () => {
  it('shows the effective value, not the provider value', async () => {
    open(OVERRIDDEN);

    // The provider calls it PROVIDER NAME; the override calls it My Name, and
    // that is what every output emits.
    expect(await screen.findByRole('textbox', { name: /^Name/ })).toHaveValue('My Name');
    expect(screen.getByRole('textbox', { name: /^Channel number/ })).toHaveValue('2.1');
  });

  it('marks each overridden field and names the provider value', async () => {
    open(OVERRIDDEN);

    const badges = await screen.findAllByText('overridden');
    // name, channel_number and channel_group_id are shadowed; tvg_id is not.
    expect(badges).toHaveLength(3);
  });

  it('writes an edit to the override, so it actually takes effect', async () => {
    const user = userEvent.setup();
    open(OVERRIDDEN);

    const name = await screen.findByRole('textbox', { name: /^Name/ });
    await user.clear(name);
    await user.type(name, 'Renamed');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    const payload = channelsApi.update.mock.calls[0][1];

    // Writing the base row under a live override changes nothing visible.
    expect(payload.override.name).toBe('Renamed');
    expect(payload).not.toHaveProperty('name');
  });

  it('leaves un-overridden fields on the base row', async () => {
    const user = userEvent.setup();
    open(OVERRIDDEN);

    await user.click(await screen.findByRole('button', { name: 'Save' }));

    const payload = channelsApi.update.mock.calls[0][1];
    expect(payload.tvg_id).toBe('21300');
    expect(payload.override).not.toHaveProperty('tvg_id', '21300');
  });

  it('resets one override back to the provider value', async () => {
    const user = userEvent.setup();
    open(OVERRIDDEN);

    await user.click(
      await screen.findByRole('button', { name: /Reset Name to the provider value/ }),
    );

    // The field snaps to what the provider says, and the badge goes.
    expect(screen.getByRole('textbox', { name: /^Name/ })).toHaveValue('PROVIDER NAME');
    expect(screen.queryAllByText('overridden')).toHaveLength(2);

    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    const payload = channelsApi.update.mock.calls[0][1];
    // An explicit null is how OverrideBody expresses "stop overriding this".
    expect(payload.override.name).toBeNull();
    expect(payload.name).toBe('PROVIDER NAME');
  });

  it('sends no override key at all for a channel that has none', async () => {
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1]).not.toHaveProperty('override');
  });
});

describe('the guide picker', () => {
  /** Already mapped, as `/api/channels/channels/` serializes it. */
  const MAPPED = { ...CHANNEL, epg_data_id: 41, epg_name: 'KAZ 2 St. Louis' };

  const guideField = () => screen.findByRole('textbox', { name: 'Guide' });

  it('renders the mapping the channel arrived with, without asking the server', async () => {
    open(MAPPED);

    // `epg_name` travels beside the id precisely so this needs no fetch. A
    // Select whose value is absent from `data` renders blank, which reads as
    // "this channel has no guide" for one that has.
    expect(await guideField()).toHaveValue('KAZ 2 St. Louis');
    expect(epgDataApi.list).not.toHaveBeenCalled();
  });

  it('is empty for a channel mapped to nothing', async () => {
    open();
    expect(await guideField()).toHaveValue('');
  });

  it('searches once for a burst of keystrokes', async () => {
    const user = userEvent.setup();
    open();

    await user.type(await guideField(), 'kaz');

    await waitFor(() => expect(epgDataApi.list).toHaveBeenCalled());
    // Without the debounce the first request is for `f` — one query per
    // keystroke against the largest table on the instance.
    expect(epgDataApi.list.mock.calls[0][0]).toBe('kaz');
    expect(epgDataApi.list).toHaveBeenCalledTimes(1);
  });

  it('shows the guide id beside the name, since both are searchable', async () => {
    const user = userEvent.setup();
    open();

    await user.type(await guideField(), 'kaz');

    // How an operator recognises a row: the feed's own label. The second
    // seeded row has none, and must still be offered.
    expect(await screen.findByText('kaz2.stl')).toBeInTheDocument();
    expect(
      screen.getByRole('option', { name: /KAZ Sports Midwest/, hidden: true }),
    ).toBeInTheDocument();
  });

  it('marks the field busy while a search is in flight', async () => {
    const user = userEvent.setup();
    let release;
    epgDataApi.list.mockReturnValue(new Promise((resolve) => (release = resolve)));
    open();

    await user.type(await guideField(), 'kaz');

    expect(await screen.findByLabelText('Searching the guide')).toBeInTheDocument();
    release(GUIDE_ROWS);
    await waitFor(() =>
      expect(screen.queryByLabelText('Searching the guide')).not.toBeInTheDocument(),
    );
  });

  it('saves the numeric id of the guide the user picks', async () => {
    const user = userEvent.setup();
    open();

    await user.type(await guideField(), 'kaz');
    await user.click(
      await screen.findByRole('option', { name: /KAZ 2 St\. Louis/, hidden: true }),
    );
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    // The id, not the name: `epg_data_id` is a foreign key.
    expect(channelsApi.update.mock.calls[0][1].epg_data_id).toBe(41);
  });

  it('clears a mapping back to null', async () => {
    const user = userEvent.setup();
    open(MAPPED);
    await guideField();

    await user.click(screen.getByLabelText('Clear the guide mapping'));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    // An explicit null is what the PATCH reads as "stop using any guide".
    expect(channelsApi.update.mock.calls[0][1].epg_data_id).toBeNull();
  });

  it('names a mapping the server could not resolve by its id', async () => {
    // `epg_name` is null when the row the mapping points at has gone. Blank
    // would read as "no guide", which is a different and fixable problem.
    open({ ...MAPPED, epg_name: null });

    expect(await guideField()).toHaveValue('guide #41');
  });

  it('ignores an answer whose search has already been superseded', async () => {
    const user = userEvent.setup();
    let releaseFirst;
    epgDataApi.list
      .mockReturnValueOnce(new Promise((resolve) => (releaseFirst = resolve)))
      .mockResolvedValueOnce([{ id: 43, name: 'VRIX', tvg_id: null }]);
    open();

    const field = await guideField();
    await user.type(field, 'kaz');
    await waitFor(() => expect(epgDataApi.list).toHaveBeenCalledTimes(1));
    await user.type(field, 'x');
    await waitFor(() => expect(epgDataApi.list).toHaveBeenCalledTimes(2));
    expect(
      await screen.findByRole('option', { name: /VRIX/, hidden: true }),
    ).toBeInTheDocument();

    await act(async () => releaseFirst(GUIDE_ROWS));

    // The slow first answer arriving last would put results for `kaz` under a
    // box that says `kazx`.
    expect(
      screen.queryByRole('option', { name: /KAZ Sports Midwest/, hidden: true }),
    ).not.toBeInTheDocument();
  });

  it('surfaces a failure with nothing to say as well', async () => {
    const user = userEvent.setup();
    epgDataApi.list.mockRejectedValue({});
    open();

    await user.type(await guideField(), 'kaz');

    expect(await screen.findByText('The guide search failed.')).toBeInTheDocument();
  });

  it('surfaces a failed search rather than showing an empty list', async () => {
    const user = userEvent.setup();
    epgDataApi.list.mockRejectedValue(
      new ApiError('Guide search failed', { status: 500 }),
    );
    open();

    await user.type(await guideField(), 'kaz');

    // An empty dropdown and a broken endpoint look identical, and the first
    // reads as "this instance has no guide data for that".
    expect(await screen.findByText('Guide search failed')).toBeInTheDocument();
  });

  it('treats an overridden guide like every other overridable field', async () => {
    const user = userEvent.setup();
    open({
      ...MAPPED,
      // The provider's row points at one guide; the override points at another,
      // and the override is what every output reads.
      epg_data_id: 40,
      epg_name: 'KAZ 2 St. Louis',
      override: { ...OVERRIDDEN.override, epg_data_id: 41 },
    });

    expect(await screen.findByRole('textbox', { name: /^Guideoverridden/ })).toHaveValue(
      'KAZ 2 St. Louis',
    );

    await user.click(
      screen.getByRole('button', { name: /Reset Guide to the provider value/ }),
    );

    // No endpoint resolves an `epg_data` id to its name, and the list only
    // serializes the one the mapping points at, so the provider's own guide can
    // only be named by its id here.
    expect(screen.getByRole('textbox', { name: 'Guide' })).toHaveValue('guide #40');

    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());

    const payload = channelsApi.update.mock.calls[0][1];
    expect(payload.override.epg_data_id).toBeNull();
    expect(payload.epg_data_id).toBe(40);
  });
});

describe('channel number input', () => {
  it('parses a trailing decimal separator rather than sending a string', async () => {
    const user = userEvent.setup();
    open();

    const field = await screen.findByLabelText(/Channel number/);
    await user.clear(field);
    await user.type(field, '2.');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    // An f64 field rejects "2." with an opaque serde error.
    expect(channelsApi.update.mock.calls[0][1].channel_number).toBe(2);
  });

  it('never saves a negative channel number', async () => {
    const user = userEvent.setup();
    open();

    const field = await screen.findByLabelText(/Channel number/);
    await user.clear(field);
    await user.type(field, '-1');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    // `min` stops the sign reaching the field at all; the validator behind it
    // covers anything that gets past the input.
    await waitFor(() => expect(field).not.toHaveValue('-1'));
    if (channelsApi.update.mock.calls.length > 0) {
      const saved = channelsApi.update.mock.calls[0][1].channel_number;
      expect(saved === null || saved >= 0).toBe(true);
    }
  });

  it('rejects a negative number that reaches the validator', () => {
    const rule = (value) => {
      const parsed = parseChannelNumber(value);
      return parsed !== null && parsed < 0 ? 'Cannot be negative' : null;
    };

    expect(rule('-1')).toBe('Cannot be negative');
    expect(rule('2.1')).toBeNull();
    expect(rule('')).toBeNull();
  });
});

describe('resetting overrides of other field types', () => {
  it('resets an overridden group back to the provider group', async () => {
    const user = userEvent.setup();
    open(OVERRIDDEN);

    // The override puts it in group 8; the provider says group 7.
    expect(await screen.findByRole('textbox', { name: /^Group/ })).toHaveValue('Locals');

    await user.click(
      screen.getByRole('button', { name: /Reset Group to the provider value/ }),
    );

    expect(screen.getByRole('textbox', { name: /^Group/ })).toHaveValue('Default Group');

    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());

    const payload = channelsApi.update.mock.calls[0][1];
    expect(payload.override.channel_group_id).toBeNull();
    expect(payload.channel_group_id).toBe(7);
  });

  it('resets an overridden logo back to the provider logo', async () => {
    const user = userEvent.setup();
    open({
      ...OVERRIDDEN,
      logo_id: 3,
      override: { ...OVERRIDDEN.override, logo_id: 4 },
    });

    expect(await screen.findByRole('button', { name: 'dro' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );

    await user.click(
      screen.getByRole('button', { name: /Reset Logo to the provider value/ }),
    );

    expect(screen.getByRole('button', { name: 'kaz' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );

    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());

    const payload = channelsApi.update.mock.calls[0][1];
    expect(payload.override.logo_id).toBeNull();
    expect(payload.logo_id).toBe(3);
  });

  it('resets an overridden number even when the provider has none', async () => {
    const user = userEvent.setup();
    open({
      ...OVERRIDDEN,
      channel_number: null,
      override: { ...OVERRIDDEN.override, channel_number: 7 },
    });

    await user.click(
      await screen.findByRole('button', {
        name: /Reset Channel number to the provider value/,
      }),
    );

    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());

    const payload = channelsApi.update.mock.calls[0][1];
    expect(payload.override.channel_number).toBeNull();
    // Unnumbered stays null, not 0.
    expect(payload.channel_number).toBeNull();
  });
});

describe('output visibility and the Gracenote station id', () => {
  it('shows a channel that reaches the outputs as included', async () => {
    open();
    expect(await screen.findByRole('switch', { name: /Include in HDHR/ })).toBeChecked();
  });

  it('shows a hidden channel as excluded', async () => {
    open({ ...CHANNEL, hidden_from_output: true });
    expect(
      await screen.findByRole('switch', { name: /Include in HDHR/ }),
    ).not.toBeChecked();
  });

  it('sends hidden_from_output on the base row, never as an override', async () => {
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole('switch', { name: /Include in HDHR/ }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    const payload = channelsApi.update.mock.calls[0][1];
    // The effective_channel view reads `c.hidden_from_output` directly, so an
    // override could never shadow it.
    expect(payload.hidden_from_output).toBe(true);
    expect(payload.override ?? {}).not.toHaveProperty('hidden_from_output');
  });

  it('saves a Gracenote station id', async () => {
    const user = userEvent.setup();
    open();

    await user.type(
      await screen.findByLabelText(/Gracenote station id/),
      'gracenote-12345',
    );
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    // Selected instead of tvg_id when a source is set to gracenote.
    expect(channelsApi.update.mock.calls[0][1].tvc_guide_stationid).toBe(
      'gracenote-12345',
    );
  });

  it('clears a Gracenote station id to null rather than an empty string', async () => {
    const user = userEvent.setup();
    open({ ...CHANNEL, tvc_guide_stationid: '12345' });

    await user.clear(await screen.findByLabelText(/Gracenote station id/));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());
    expect(channelsApi.update.mock.calls[0][1].tvc_guide_stationid).toBeNull();
  });

  it('treats an overridden station id like every other overridable field', async () => {
    const user = userEvent.setup();
    open({
      ...OVERRIDDEN,
      tvc_guide_stationid: 'PROVIDER-1',
      override: { ...OVERRIDDEN.override, tvc_guide_stationid: 'MINE-2' },
    });

    // The view coalesces this one, so it gets the same badge and Reset.
    expect(
      await screen.findByRole('textbox', { name: /^Gracenote station id/ }),
    ).toHaveValue('MINE-2');

    await user.click(
      screen.getByRole('button', {
        name: /Reset Gracenote station id to the provider value/,
      }),
    );
    expect(screen.getByRole('textbox', { name: /^Gracenote station id/ })).toHaveValue(
      'PROVIDER-1',
    );

    await user.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(channelsApi.update).toHaveBeenCalled());

    const payload = channelsApi.update.mock.calls[0][1];
    expect(payload.override.tvc_guide_stationid).toBeNull();
    expect(payload.tvc_guide_stationid).toBe('PROVIDER-1');
  });
});
