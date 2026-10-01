import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Groups } from './Groups.jsx';
import {
  channelGroups as groupsApi,
  m3uAccounts as m3uApi,
  settings as settingsApi,
} from '../api/resources.js';
import { notifyDone, notifyError } from '../notify.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  channelGroups: {
    list: vi.fn(),
    create: vi.fn(),
    update: vi.fn(),
    remove: vi.fn(),
    renumber: vi.fn(),
    planRanges: vi.fn(),
    assignRanges: vi.fn(),
    planRenumber: vi.fn(),
    renumberAll: vi.fn(),
  },
  m3uAccounts: {
    list: vi.fn(),
    setGroup: vi.fn(),
  },
  settings: {
    list: vi.fn(),
    update: vi.fn(),
  },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

const ACCOUNTS = [
  { id: 2, name: 'PrivateIPTV' },
  { id: 3, name: 'Backup' },
  // Has offered no group, so it gets no column.
  { id: 4, name: 'Unused' },
];

/** Shaped as `channels::group_json` emits it. */
const GROUPS = [
  {
    id: 8,
    name: 'Canada',
    number_start: 200,
    number_end: 299,
    channel_count: 14,
    stream_count: 16,
    links: [
      {
        m3u_account_id: 2,
        enabled: true,
        auto_channel_sync: false,
        numbering_mode: 'range',
      },
    ],
  },
  {
    id: 9,
    name: 'ZOR Plus',
    number_start: null,
    number_end: null,
    channel_count: 3,
    stream_count: 3,
    links: [
      {
        m3u_account_id: 2,
        enabled: true,
        auto_channel_sync: false,
        numbering_mode: 'range',
      },
      {
        m3u_account_id: 3,
        enabled: false,
        auto_channel_sync: false,
        numbering_mode: 'range',
      },
    ],
  },
  {
    id: 10,
    name: 'Empty',
    number_start: 500,
    number_end: null,
    channel_count: 0,
    stream_count: 0,
    links: [],
  },
];

function open(route = '/groups') {
  return renderWithProviders(<Groups />, { route });
}

/** The group names down the table, in the order it is showing them. */
function rowNames(table) {
  return within(table)
    .getAllByRole('row')
    .slice(1)
    .map((row) => within(row).getAllByRole('cell')[2].textContent);
}

beforeEach(() => {
  vi.clearAllMocks();
  groupsApi.list.mockResolvedValue(GROUPS);
  m3uApi.list.mockResolvedValue(ACCOUNTS);
  m3uApi.setGroup.mockResolvedValue({});
  groupsApi.update.mockResolvedValue({});
  settingsApi.list.mockResolvedValue([
    {
      key: 'numbering_settings',
      name: 'Numbering',
      value: { group_block_size: 100, channel_step: 1 },
    },
  ]);
  settingsApi.update.mockResolvedValue({});
});

describe('the table', () => {
  it('shows each group with its counts, its range and its standing with each provider', async () => {
    open();

    const table = await screen.findByRole('table', { name: 'Groups' });
    expect(within(table).getByText('Canada')).toBeInTheDocument();
    expect(within(table).getByText('14')).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Canada range start' })).toHaveValue(
      '200',
    );
    expect(screen.getByRole('textbox', { name: 'Canada range end' })).toHaveValue('299');

    // One column per provider that has offered a group; none for the unused one.
    expect(within(table).getByText('PrivateIPTV')).toBeInTheDocument();
    expect(within(table).getByText('Backup')).toBeInTheDocument();
    expect(within(table).queryByText('Unused')).not.toBeInTheDocument();

    expect(
      screen.getByRole('switch', { name: 'Import Canada from PrivateIPTV' }),
    ).toBeChecked();
    expect(
      screen.getByRole('switch', { name: 'Import ZOR Plus from Backup' }),
    ).not.toBeChecked();
    // Auto-sync means nothing for a provider the group is not imported from.
    expect(
      screen.getByRole('switch', { name: 'Auto-sync ZOR Plus from Backup' }),
    ).toBeDisabled();
    // A provider that never offered the group says so rather than showing
    // dead switches: Canada from Backup, and Empty from both.
    expect(within(table).getAllByText('Not offered')).toHaveLength(3);
  });

  it('narrows to one provider from the query string', async () => {
    open('/groups?account=3');

    const table = await screen.findByRole('table', { name: 'Groups' });
    expect(within(table).getByText('ZOR Plus')).toBeInTheDocument();
    expect(within(table).queryByText('Canada')).not.toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Filter by provider' })).toHaveValue(
      'Backup',
    );
  });
});

describe('a provider link', () => {
  it('previews what switching auto-sync on would create, then confirms it', async () => {
    const user = userEvent.setup();
    m3uApi.setGroup
      .mockRejectedValueOnce(
        new ApiError('switching auto-sync on creates a channel for every stream', {
          status: 409,
          payload: {
            cost: {
              channels_created: 2,
              channels: [
                { stream: 21, name: 'ZOR 1 HD', channel_number: 200 },
                { stream: 60, name: 'Snowvalen', channel_number: 201 },
              ],
            },
          },
        }),
      )
      .mockResolvedValue({});
    open();

    await user.click(
      await screen.findByRole('switch', { name: 'Auto-sync Canada from PrivateIPTV' }),
    );

    // The refusal is shown with each stream and the number it would take, and
    // flips nothing.
    expect(await screen.findByText('ZOR 1 HD')).toBeInTheDocument();
    expect(screen.getByText('#201')).toBeInTheDocument();
    expect(
      screen.getByRole('switch', { name: 'Auto-sync Canada from PrivateIPTV' }),
    ).not.toBeChecked();

    await user.click(screen.getByRole('button', { name: 'Turn on' }));
    await waitFor(() =>
      expect(m3uApi.setGroup).toHaveBeenLastCalledWith(2, {
        channel_group: 8,
        auto_channel_sync: true,
        confirm: true,
      }),
    );
  });

  it('shows what stopping an import costs before doing it', async () => {
    const user = userEvent.setup();
    m3uApi.setGroup
      .mockRejectedValueOnce(
        new ApiError('disabling this group deletes its streams on the next refresh', {
          status: 409,
          payload: {
            cost: {
              streams_deleted: 3,
              channels_affected: 2,
              channels_left_unplayable: 1,
            },
          },
        }),
      )
      .mockResolvedValue({});
    open();

    await user.click(
      await screen.findByRole('switch', { name: 'Import Canada from PrivateIPTV' }),
    );

    expect(await screen.findByText(/3 streams deleted; 1 channels/)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Stop importing' }));
    await waitFor(() =>
      expect(m3uApi.setGroup).toHaveBeenLastCalledWith(2, {
        channel_group: 8,
        enabled: false,
        confirm: true,
      }),
    );
  });

  it('applies an import change to every selected group at once', async () => {
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole('checkbox', { name: 'Select row 8' }));
    await user.click(screen.getByRole('checkbox', { name: 'Select row 9' }));
    await user.click(screen.getByRole('button', { name: 'Bulk actions' }));
    await user.click(
      await screen.findByRole('menuitem', { name: 'Stop importing from PrivateIPTV' }),
    );

    // Unconfirmed: a bulk disable must not send `confirm: true` up front, or
    // streams are deleted without asking. The server answers here because
    // nothing would be lost; when something would, it refuses and the test
    // below takes over.
    await waitFor(() => expect(m3uApi.setGroup).toHaveBeenCalledTimes(2));
    expect(m3uApi.setGroup).toHaveBeenCalledWith(2, {
      channel_group: 8,
      enabled: false,
      confirm: false,
    });
    expect(m3uApi.setGroup).toHaveBeenCalledWith(2, {
      channel_group: 9,
      enabled: false,
      confirm: false,
    });
    expect(notifyDone).toHaveBeenCalledWith('No longer importing 2 groups');
  });

  it('will not disable groups in bulk until the cost is confirmed', async () => {
    const user = userEvent.setup();
    const cost = { streams_deleted: 12, channels_left_unplayable: 3, channels: [] };
    m3uApi.setGroup
      .mockRejectedValueOnce(
        new ApiError('costs streams', { status: 409, payload: { cost } }),
      )
      .mockRejectedValueOnce(
        new ApiError('costs streams', { status: 409, payload: { cost } }),
      )
      .mockResolvedValue({});
    open();

    await user.click(await screen.findByRole('checkbox', { name: 'Select row 8' }));
    await user.click(screen.getByRole('checkbox', { name: 'Select row 9' }));
    await user.click(screen.getByRole('button', { name: 'Bulk actions' }));
    await user.click(
      await screen.findByRole('menuitem', { name: 'Stop importing from PrivateIPTV' }),
    );

    // Nothing is written until the operator sees the summed cost.
    expect(await screen.findByText(/2 of 2 groups have streams/)).toBeInTheDocument();
    expect(screen.getByText(/24 streams deleted; 6 channels/)).toBeInTheDocument();
    expect(m3uApi.setGroup).not.toHaveBeenCalledWith(
      2,
      expect.objectContaining({ confirm: true }),
    );

    await user.click(screen.getByRole('button', { name: 'Stop importing' }));

    await waitFor(() =>
      expect(m3uApi.setGroup).toHaveBeenCalledWith(2, {
        channel_group: 8,
        enabled: false,
        confirm: true,
      }),
    );
    expect(m3uApi.setGroup).toHaveBeenCalledWith(2, {
      channel_group: 9,
      enabled: false,
      confirm: true,
    });
  });
});

describe('the range', () => {
  it('is saved when its field is left, not on every keystroke', async () => {
    const user = userEvent.setup();
    open();

    await user.type(
      await screen.findByRole('textbox', { name: 'ZOR Plus range start' }),
      '600',
    );
    expect(groupsApi.update).not.toHaveBeenCalled();
    await user.tab();

    await waitFor(() =>
      expect(groupsApi.update).toHaveBeenCalledWith(9, {
        number_start: 600,
        number_end: null,
      }),
    );
  });

  it('renumbers a group into its range after saying what moves', async () => {
    const user = userEvent.setup();
    groupsApi.renumber.mockResolvedValue({ renumbered: 14, channels: [] });
    open();

    await user.click(await screen.findByRole('button', { name: 'Renumber Canada' }));
    expect(
      await screen.findByText(/14 channels in Canada take new numbers, from 200 to 299/),
    ).toBeInTheDocument();
    expect(screen.getByText(/re-scan the tuner/)).toBeInTheDocument();
    const dialog = screen.getByRole('dialog', { name: 'Renumber Canada' });
    await user.click(within(dialog).getByRole('button', { name: 'Renumber' }));

    await waitFor(() => expect(groupsApi.renumber).toHaveBeenCalledWith(8, 'current'));
    expect(notifyDone).toHaveBeenCalledWith('Renumbered 14 channels in Canada');
  });

  it('renumbers a group in a chosen order', async () => {
    const user = userEvent.setup();
    groupsApi.renumber.mockResolvedValue({ renumbered: 14, channels: [] });
    open();

    await user.click(await screen.findByRole('button', { name: 'Renumber Canada' }));
    const dialog = screen.getByRole('dialog', { name: 'Renumber Canada' });
    await user.click(within(dialog).getByRole('textbox', { name: 'Order by' }));
    await user.click(
      await screen.findByRole('option', { name: 'Guide name', hidden: true }),
    );
    await user.click(within(dialog).getByRole('button', { name: 'Renumber' }));

    await waitFor(() => expect(groupsApi.renumber).toHaveBeenCalledWith(8, 'guide'));
  });

  it('re-plans the lineup-wide renumber when the order changes', async () => {
    const user = userEvent.setup();
    groupsApi.planRenumber.mockResolvedValue({
      groups: [
        {
          id: 8,
          name: 'Canada',
          number_start: 200,
          number_end: 299,
          channels: [{ id: 1, name: 'ZOR1', from: 3, to: 200 }],
        },
      ],
      skipped: [],
    });
    groupsApi.renumberAll.mockResolvedValue({ renumbered: 1, groups: 1 });
    open();

    await screen.findByRole('table', { name: 'Groups' });
    await user.click(screen.getByRole('button', { name: 'Renumber all…' }));
    await waitFor(() => expect(groupsApi.planRenumber).toHaveBeenCalledWith('current'));

    const dialog = await screen.findByRole('dialog', { name: 'Renumber all' });
    await user.click(within(dialog).getByRole('textbox', { name: 'Order by' }));
    await user.click(
      await screen.findByRole('option', { name: 'Channel name', hidden: true }),
    );

    // The preview is the same code the button runs, so it is re-made rather
    // than left showing the plan for an order nobody chose.
    await waitFor(() => expect(groupsApi.planRenumber).toHaveBeenLastCalledWith('name'));
    await user.click(within(dialog).getByRole('button', { name: 'Apply' }));
    await waitFor(() => expect(groupsApi.renumberAll).toHaveBeenCalledWith('name'));
  });

  it('offers no renumber without a range, or without channels', async () => {
    open();

    expect(
      await screen.findByRole('button', { name: 'Renumber ZOR Plus' }),
    ).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Renumber Empty' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Renumber Canada' })).toBeEnabled();
  });
});

describe('the editor', () => {
  it('creates a group with its range', async () => {
    const user = userEvent.setup();
    groupsApi.create.mockResolvedValue({ id: 11 });
    open();

    await user.click(await screen.findByRole('button', { name: 'New group' }));
    const dialog = await screen.findByRole('dialog', { name: 'New group' });
    await user.type(within(dialog).getByRole('textbox', { name: 'Name' }), 'Kids');
    await user.type(within(dialog).getByRole('textbox', { name: 'Numbers from' }), '700');
    await user.click(within(dialog).getByRole('button', { name: 'Create' }));

    await waitFor(() =>
      expect(groupsApi.create).toHaveBeenCalledWith({
        name: 'Kids',
        number_start: 700,
        number_end: null,
      }),
    );
    expect(notifyDone).toHaveBeenCalledWith('Created Kids');
  });

  it('renames a group and keeps its range', async () => {
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole('button', { name: 'Edit Canada' }));
    const dialog = await screen.findByRole('dialog', { name: 'Edit Canada' });
    const name = within(dialog).getByRole('textbox', { name: 'Name' });
    await user.clear(name);
    await user.type(name, 'Canada East');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(groupsApi.update).toHaveBeenCalledWith(8, {
        name: 'Canada East',
        number_start: 200,
        number_end: 299,
      }),
    );
  });

  it('refuses an end below the start before asking the server', async () => {
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole('button', { name: 'Edit Canada' }));
    const dialog = await screen.findByRole('dialog', { name: 'Edit Canada' });
    const end = within(dialog).getByRole('textbox', { name: 'Numbers to' });
    await user.clear(end);
    await user.type(end, '150');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    expect(
      await within(dialog).findByText('Must be at or above the start'),
    ).toBeInTheDocument();
    expect(groupsApi.update).not.toHaveBeenCalled();
  });

  it('deletes a group after saying what happens to its channels', async () => {
    const user = userEvent.setup();
    groupsApi.remove.mockResolvedValue(null);
    open();

    await user.click(await screen.findByRole('button', { name: 'Delete Canada' }));
    expect(await screen.findByText(/14 channels keep their numbers/)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Delete' }));

    await waitFor(() => expect(groupsApi.remove).toHaveBeenCalledWith(8));
  });
});

describe('when the server refuses', () => {
  it('reports a refused link change and leaves the switch as it was', async () => {
    const user = userEvent.setup();
    m3uApi.setGroup.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    open();

    await user.click(
      await screen.findByRole('switch', { name: 'Import Canada from PrivateIPTV' }),
    );

    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith(
        'Could not change the group',
        expect.anything(),
      ),
    );
    expect(
      screen.getByRole('switch', { name: 'Import Canada from PrivateIPTV' }),
    ).toBeChecked();
  });

  it('reports a confirmed change that still failed', async () => {
    const user = userEvent.setup();
    m3uApi.setGroup
      .mockRejectedValueOnce(
        new ApiError('creates channels', {
          status: 409,
          payload: { cost: { channels_created: 1, channels: [] } },
        }),
      )
      .mockRejectedValueOnce(new ApiError('Boom', { status: 500 }));
    open();

    await user.click(
      await screen.findByRole('switch', { name: 'Auto-sync Canada from PrivateIPTV' }),
    );
    await user.click(await screen.findByRole('button', { name: 'Turn on' }));

    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith(
        'Could not change the group',
        expect.anything(),
      ),
    );
  });

  it('reports a range, a renumber and a delete it would not take', async () => {
    const user = userEvent.setup();
    groupsApi.update.mockRejectedValue(new ApiError('Boom', { status: 400 }));
    groupsApi.renumber.mockRejectedValue(new ApiError('Boom', { status: 400 }));
    groupsApi.remove.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    open();

    await user.type(
      await screen.findByRole('textbox', { name: 'ZOR Plus range start' }),
      '600',
    );
    await user.tab();
    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith(
        'Could not save the range',
        expect.anything(),
      ),
    );

    await user.click(screen.getByRole('button', { name: 'Renumber Canada' }));
    await user.click(await screen.findByRole('button', { name: 'Renumber' }));
    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith(
        'Could not renumber the group',
        expect.anything(),
      ),
    );

    await user.click(screen.getByRole('button', { name: 'Delete Canada' }));
    await user.click(await screen.findByRole('button', { name: 'Delete' }));
    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith(
        'Could not delete the group',
        expect.anything(),
      ),
    );
  });

  it('says how many of a bulk change did not go through', async () => {
    const user = userEvent.setup();
    m3uApi.setGroup
      .mockResolvedValueOnce({})
      .mockRejectedValueOnce(new ApiError('Boom', { status: 500 }));
    open();

    await user.click(await screen.findByRole('checkbox', { name: 'Select row 8' }));
    await user.click(screen.getByRole('checkbox', { name: 'Select row 9' }));
    await user.click(screen.getByRole('button', { name: 'Bulk actions' }));
    await user.click(
      await screen.findByRole('menuitem', { name: 'Import from PrivateIPTV' }),
    );

    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith('Some groups were not changed', {
        message: '1 of 2 could not be updated.',
      }),
    );
    expect(m3uApi.setGroup).toHaveBeenCalledWith(2, {
      channel_group: 8,
      enabled: true,
      confirm: false,
    });
  });

  it('reports a group the server would not create', async () => {
    const user = userEvent.setup();
    groupsApi.create.mockRejectedValue(new ApiError('exists', { status: 409 }));
    open();

    await user.click(await screen.findByRole('button', { name: 'New group' }));
    const dialog = await screen.findByRole('dialog', { name: 'New group' });
    await user.type(within(dialog).getByRole('textbox', { name: 'Name' }), 'Canada');
    await user.click(within(dialog).getByRole('button', { name: 'Create' }));

    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith(
        'Could not create the group',
        expect.anything(),
      ),
    );
    // Still open, so the name can be changed rather than retyped.
    expect(screen.getByRole('dialog', { name: 'New group' })).toBeInTheDocument();
  });
});

describe('the toolbar', () => {
  it('narrows to a provider chosen from the filter, and widens again', async () => {
    const user = userEvent.setup();
    open();

    const table = await screen.findByRole('table', { name: 'Groups' });
    await user.click(screen.getByRole('textbox', { name: 'Filter by provider' }));
    await user.click(await screen.findByRole('option', { name: 'Backup', hidden: true }));
    await waitFor(() =>
      expect(within(table).queryByText('Canada')).not.toBeInTheDocument(),
    );
    expect(within(table).getByText('ZOR Plus')).toBeInTheDocument();

    // Choosing the selected provider again deselects it.
    await user.click(screen.getByRole('textbox', { name: 'Filter by provider' }));
    await user.click(await screen.findByRole('option', { name: 'Backup', hidden: true }));
    expect(await within(table).findByText('Canada')).toBeInTheDocument();
  });

  it('sorts by where each range starts, unranged groups first', async () => {
    const user = userEvent.setup();
    open();

    const table = await screen.findByRole('table', { name: 'Groups' });
    await user.click(within(table).getByRole('button', { name: /Number range/ }));

    expect(rowNames(table)).toEqual(['ZOR Plus', 'Canada', 'Empty']);
  });

  it('drags a group into the order Assign ranges then uses', async () => {
    const user = userEvent.setup();
    groupsApi.planRanges.mockResolvedValue({ block_size: 100, ranges: [] });
    open();

    const table = await screen.findByRole('table', { name: 'Groups' });
    expect(rowNames(table)).toEqual(['Canada', 'ZOR Plus', 'Empty']);

    // Empty, dragged up over ZOR Plus: the rows swap under the pointer, with
    // the mouse still down.
    const grip = screen.getByRole('button', { name: 'Move Empty' });
    fireEvent.dragStart(grip);
    fireEvent.dragOver(within(table).getByText('ZOR Plus').closest('tr'));
    expect(rowNames(table)).toEqual(['Canada', 'Empty', 'ZOR Plus']);
    fireEvent.dragEnd(grip);

    expect(rowNames(table)).toEqual(['Canada', 'Empty', 'ZOR Plus']);

    await user.click(screen.getByRole('button', { name: 'Assign ranges…' }));
    await waitFor(() => expect(groupsApi.planRanges).toHaveBeenCalledWith([8, 10, 9]));
  });

  it('moves a group from the keyboard and keeps hold of the handle', async () => {
    const user = userEvent.setup();
    open();

    const table = await screen.findByRole('table', { name: 'Groups' });
    screen.getByRole('button', { name: 'Move Canada' }).focus();
    await user.keyboard('{ArrowDown}{ArrowDown}');

    expect(rowNames(table)).toEqual(['ZOR Plus', 'Empty', 'Canada']);
    expect(screen.getByRole('button', { name: 'Move Canada' })).toHaveFocus();

    // Past the last row there is nowhere to go, rather than off the end.
    await user.keyboard('{ArrowDown}');
    expect(rowNames(table)).toEqual(['ZOR Plus', 'Empty', 'Canada']);
  });

  it('leaves the groups a provider filter hides where they were', async () => {
    const user = userEvent.setup();
    groupsApi.planRanges.mockResolvedValue({ block_size: 100, ranges: [] });
    open('/groups?account=2');

    const table = await screen.findByRole('table', { name: 'Groups' });
    // Empty is not linked to PrivateIPTV, so it is not on screen to be dragged.
    expect(rowNames(table)).toEqual(['Canada', 'ZOR Plus']);
    screen.getByRole('button', { name: 'Move ZOR Plus' }).focus();
    await user.keyboard('{ArrowUp}');
    expect(rowNames(table)).toEqual(['ZOR Plus', 'Canada']);

    // Clearing the filter shows Empty still third: the two that moved swapped
    // their own places, and the hidden one kept its own.
    await user.click(screen.getByRole('textbox', { name: 'Filter by provider' }));
    await user.click(
      await screen.findByRole('option', { name: 'PrivateIPTV', hidden: true }),
    );
    expect(await within(table).findByText('Empty')).toBeInTheDocument();
    expect(rowNames(table)).toEqual(['ZOR Plus', 'Canada', 'Empty']);
  });

  it('takes over from a column sort rather than being swallowed by it', async () => {
    const user = userEvent.setup();
    open();

    const table = await screen.findByRole('table', { name: 'Groups' });
    // Sorted by channel count: Empty, ZOR Plus, Canada.
    await user.click(within(table).getByRole('button', { name: /Channels/ }));
    expect(rowNames(table)).toEqual(['Empty', 'ZOR Plus', 'Canada']);

    screen.getByRole('button', { name: 'Move Canada' }).focus();
    await user.keyboard('{ArrowUp}');

    // What was on screen, with the one row moved — not the source order back.
    expect(rowNames(table)).toEqual(['Empty', 'Canada', 'ZOR Plus']);
  });
});

describe('the numbering policy', () => {
  it('shows the settings and saves them when a field is left', async () => {
    const user = userEvent.setup();
    open();

    const step = await screen.findByRole('textbox', { name: 'Channel step' });
    await waitFor(() => expect(step).toHaveValue('1'));
    expect(screen.getByRole('textbox', { name: 'Group block size' })).toHaveValue('100');

    await user.clear(step);
    await user.type(step, '10');
    expect(settingsApi.update).not.toHaveBeenCalled();
    await user.tab();

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('numbering_settings', {
        group_block_size: 100,
        channel_step: 10,
      }),
    );
  });

  it('reports a refused setting by its field', async () => {
    const user = userEvent.setup();
    settingsApi.update.mockRejectedValue(
      new ApiError('rejected', {
        status: 400,
        payload: { fields: { channel_step: 'must be a number of at least 1' } },
      }),
    );
    open();

    const step = await screen.findByRole('textbox', { name: 'Channel step' });
    await waitFor(() => expect(step).toHaveValue('1'));
    await user.clear(step);
    await user.type(step, '0');
    await user.tab();

    await waitFor(() =>
      expect(notifyError).toHaveBeenCalledWith('Could not save the numbering settings', {
        message: 'must be a number of at least 1',
      }),
    );
  });

  it("assigns ranges from a preview, in the table's order", async () => {
    const user = userEvent.setup();
    groupsApi.planRanges.mockResolvedValue({
      block_size: 1000,
      ranges: [{ id: 9, name: 'ZOR Plus', number_start: 1000, number_end: 1999 }],
    });
    groupsApi.assignRanges.mockResolvedValue({ assigned: 1 });
    open();

    const table = await screen.findByRole('table', { name: 'Groups' });
    // Sorted by name, so the order sent is Canada, Empty, ZOR Plus — not the
    // order the rows arrived in.
    await user.click(within(table).getByRole('button', { name: /^Group/ }));
    await user.click(screen.getByRole('button', { name: 'Assign ranges…' }));

    await waitFor(() => expect(groupsApi.planRanges).toHaveBeenCalledWith([8, 10, 9]));
    const dialog = await screen.findByRole('dialog', { name: 'Assign ranges' });
    expect(within(dialog).getByText('ZOR Plus')).toBeInTheDocument();
    expect(within(dialog).getByText('1999')).toBeInTheDocument();
    expect(groupsApi.assignRanges).not.toHaveBeenCalled();

    await user.click(within(dialog).getByRole('button', { name: 'Apply' }));
    await waitFor(() =>
      expect(groupsApi.assignRanges).toHaveBeenCalledWith([
        { id: 9, name: 'ZOR Plus', number_start: 1000, number_end: 1999 },
      ]),
    );
    expect(notifyDone).toHaveBeenCalledWith('Assigned ranges to 1 group');
  });

  it('says so when every group already has a range', async () => {
    const user = userEvent.setup();
    groupsApi.planRanges.mockResolvedValue({ block_size: 100, ranges: [] });
    open();

    await screen.findByRole('table', { name: 'Groups' });
    await user.click(screen.getByRole('button', { name: 'Assign ranges…' }));

    const dialog = await screen.findByRole('dialog', { name: 'Assign ranges' });
    expect(
      within(dialog).getByText(/Every group already has a range/),
    ).toBeInTheDocument();
    expect(
      within(dialog).queryByRole('button', { name: 'Apply' }),
    ).not.toBeInTheDocument();
  });

  it('renumbers everything from a preview that names what it leaves alone', async () => {
    const user = userEvent.setup();
    groupsApi.planRenumber.mockResolvedValue({
      groups: [
        {
          id: 8,
          name: 'Canada',
          number_start: 200,
          number_end: 299,
          channels: [
            { id: 1, name: 'ZOR1', from: 3, to: 200 },
            { id: 2, name: 'ZOR2', from: 4, to: 210 },
          ],
        },
      ],
      skipped: [{ id: 9, name: 'ZOR Plus', reason: 'no range' }],
    });
    groupsApi.renumberAll.mockResolvedValue({ renumbered: 2, groups: 1, skipped: [] });
    open();

    await screen.findByRole('table', { name: 'Groups' });
    await user.click(screen.getByRole('button', { name: 'Renumber all…' }));

    const dialog = await screen.findByRole('dialog', { name: 'Renumber all' });
    expect(within(dialog).getByText('200 → 210')).toBeInTheDocument();
    expect(within(dialog).getByText(/ZOR Plus \(no range\)/)).toBeInTheDocument();
    expect(within(dialog).getByText(/re-scan the tuner/)).toBeInTheDocument();
    expect(groupsApi.renumberAll).not.toHaveBeenCalled();

    await user.click(within(dialog).getByRole('button', { name: 'Apply' }));
    await waitFor(() => expect(groupsApi.renumberAll).toHaveBeenCalled());
    expect(notifyDone).toHaveBeenCalledWith('Renumbered 2 channels in 1 group');
  });
});

describe('a provider that numbers its own channels', () => {
  it('keeps the group out of reach of a renumber, and says whose numbers they are', async () => {
    groupsApi.list.mockResolvedValue([
      {
        ...GROUPS[0],
        links: [
          {
            m3u_account_id: 2,
            enabled: true,
            auto_channel_sync: true,
            numbering_mode: 'provider',
          },
        ],
      },
    ]);
    open();

    expect(await screen.findByRole('button', { name: 'Renumber Canada' })).toBeDisabled();
  });

  it('is chosen per provider from the editor', async () => {
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole('button', { name: 'Edit Canada' }));
    const dialog = await screen.findByRole('dialog', { name: 'Edit Canada' });
    await user.click(
      within(dialog).getByRole('textbox', { name: 'Numbers from PrivateIPTV' }),
    );
    await user.click(
      await screen.findByRole('option', {
        name: "The provider's own numbers",
        hidden: true,
      }),
    );
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(m3uApi.setGroup).toHaveBeenCalledWith(2, {
        channel_group: 8,
        numbering_mode: 'provider',
      }),
    );
    // The group's own fields still go to the group.
    expect(groupsApi.update).toHaveBeenCalledWith(8, {
      name: 'Canada',
      number_start: 200,
      number_end: 299,
    });
  });
});
