import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Guide } from './Guide.jsx';
import { channelProfiles as profilesApi, guide as guideApi } from '../api/resources.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  guide: { grid: vi.fn(), acceptSuggestion: vi.fn(), dismissSuggestion: vi.fn() },
  channelProfiles: { list: vi.fn() },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

const HOUR = 3_600_000;
/** Pinned so "now", the window and every programme boundary are deterministic. */
const NOW = Date.parse('2026-02-10T19:30:00Z');
const WINDOW_START = Date.parse('2026-02-10T18:00:00Z');

const at = (hoursFromWindowStart) =>
  new Date(WINDOW_START + hoursFromWindowStart * HOUR).toISOString();

/**
 * Three channels covering the three states a lineup contains: guide data, no
 * guide data at all, and a match the fuzzy scorer would not decide. A lineup
 * with a fifth unmapped is normal, so an empty row is normal.
 */
const GRID = {
  start: at(0),
  end: at(24),
  channels: [
    {
      id: 1,
      uuid: 'a',
      name: 'KETC-HD',
      channel_number: 9.1,
      logo_url: 'http://logos.test/pbs.png',
      group_name: 'Default Group',
      epg_data_id: 11,
      epg_suggestion: null,
      programs: [
        {
          id: 100,
          // Began before the window opened: must clip, not vanish.
          start_time: at(-1),
          end_time: at(2),
          title: 'Primetime in Milan',
          sub_title: null,
          description: 'Coverage of the women’s team combined.',
          is_new: false,
          is_live: false,
        },
        {
          id: 101,
          start_time: at(2),
          end_time: at(3),
          title: 'PBS News Hour',
          sub_title: null,
          description: 'Evening news coverage.',
          is_new: true,
          is_live: false,
        },
      ],
    },
    {
      id: 2,
      uuid: 'b',
      name: 'Unmapped Local',
      channel_number: 30.1,
      logo_url: null,
      group_name: 'Locals',
      epg_data_id: null,
      epg_suggestion: null,
      programs: [],
    },
    {
      id: 3,
      uuid: 'c',
      name: 'VRIX HD',
      channel_number: 206,
      logo_url: null,
      group_name: 'Sports',
      epg_data_id: null,
      epg_suggestion: { epg_data_id: 77, name: 'VRIX', score: 72.5 },
      programs: [],
    },
  ],
};

function rowFor(name) {
  return screen.getByText(name).closest('div[class*="row"]');
}

beforeEach(() => {
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(NOW);
  vi.clearAllMocks();
  guideApi.grid.mockResolvedValue(GRID);
  guideApi.acceptSuggestion.mockResolvedValue({ epg_data_id: 77 });
  guideApi.dismissSuggestion.mockResolvedValue(null);
  profilesApi.list.mockResolvedValue([{ id: 1, name: 'Default', channels: [] }]);
});

afterEach(() => {
  vi.useRealTimers();
});

describe('the grid request', () => {
  it('asks for one window rather than one request per channel', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    // Dozens of channels times a /programs/ request each is the shape this avoids.
    expect(guideApi.grid).toHaveBeenCalledTimes(1);
    const [{ from, to }] = guideApi.grid.mock.calls[0];
    expect(to.getTime() - from.getTime()).toBe(24 * HOUR);
  });

  it('opens the window an hour back so the current programme is whole', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    const [{ from }] = guideApi.grid.mock.calls[0];
    expect(from.getTime()).toBeLessThan(NOW);
    expect(from.getMinutes()).toBe(0);
  });

  it('narrows to a channel profile', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('textbox', { name: 'Filter by channel profile' }));
    await user.click(
      await screen.findByRole('option', { name: 'Default', hidden: true }),
    );

    await waitFor(() =>
      expect(guideApi.grid).toHaveBeenLastCalledWith(
        expect.objectContaining({ channelProfile: 1 }),
      ),
    );
  });

  it('surfaces a load failure', async () => {
    guideApi.grid.mockRejectedValue(new ApiError('Guide unavailable.', { status: 503 }));
    renderWithProviders(<Guide />);

    expect(await screen.findByText('Guide unavailable.')).toBeInTheDocument();
  });
});

describe('rows', () => {
  it('keeps the lineup order the server sent', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    // `effective_channel` order, so the guide matches the lineup Plex receives.
    const names = ['KETC-HD', 'Unmapped Local', 'VRIX HD'].map((name) =>
      screen.getByText(name),
    );
    expect(names[0].compareDocumentPosition(names[1])).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    );
    expect(names[1].compareDocumentPosition(names[2])).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    );
  });

  it('shows the channel number beside the name', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText('9.1')).toBeInTheDocument();
    expect(screen.getByText('206')).toBeInTheDocument();
  });

  it('filters channels by name without refetching', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    await user.type(screen.getByRole('textbox', { name: 'Search channels' }), 'vrix');

    expect(screen.getByText('VRIX HD')).toBeInTheDocument();
    expect(screen.queryByText('KETC-HD')).not.toBeInTheDocument();
    // The window did not change, so the grid did not need reloading.
    expect(guideApi.grid).toHaveBeenCalledTimes(1);
  });
});

describe('channels with no guide data', () => {
  it('gives them a row that says so, rather than omitting them', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    // A missing row would read as the channel itself being gone.
    expect(screen.getByText('Unmapped Local')).toBeInTheDocument();
    expect(
      screen.getByText(/No guide data for this channel — no EPG source is matched/),
    ).toBeInTheDocument();
  });

  it('counts them in the header, because a fifth of this lineup is unmapped', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText(/3 channels · 2 without guide data/)).toBeInTheDocument();
  });

  it('says nothing about missing guides when every channel has one', async () => {
    guideApi.grid.mockResolvedValue({ ...GRID, channels: [GRID.channels[0]] });
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText('1 channel')).toBeInTheDocument();
    expect(screen.queryByText(/without guide data/)).not.toBeInTheDocument();
  });
});

describe('the ambiguous band', () => {
  it('offers the candidate on the row where the gap is noticed', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('VRIX HD');

    const row = rowFor('VRIX HD');
    expect(within(row).getByText('likely VRIX')).toBeInTheDocument();
    expect(within(row).getByText(/73% match/)).toBeInTheDocument();
  });

  it('summarises how many need deciding', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('VRIX HD');

    expect(
      screen.getByText(/1 channel has a likely guide match the matcher would not decide/),
    ).toBeInTheDocument();
  });

  it('accepts a suggestion and stops offering it', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    renderWithProviders(<Guide />);
    await screen.findByText('VRIX HD');

    await user.click(screen.getByRole('button', { name: 'Use VRIX for VRIX HD' }));

    await waitFor(() => expect(guideApi.acceptSuggestion).toHaveBeenCalledWith(3));
    await waitFor(() =>
      expect(screen.queryByText('likely VRIX')).not.toBeInTheDocument(),
    );
    // The row stays, now reading as an ordinary unmapped channel until the
    // next refresh fills it in.
    expect(screen.getByText('VRIX HD')).toBeInTheDocument();
  });

  it('dismisses a suggestion without assigning anything', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    renderWithProviders(<Guide />);
    await screen.findByText('VRIX HD');

    await user.click(
      screen.getByRole('button', { name: 'Dismiss the guide suggestion for VRIX HD' }),
    );

    await waitFor(() => expect(guideApi.dismissSuggestion).toHaveBeenCalledWith(3));
    expect(guideApi.acceptSuggestion).not.toHaveBeenCalled();
  });

  it('keeps offering it when the server rejects the change', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    guideApi.acceptSuggestion.mockRejectedValue(new ApiError('Gone', { status: 404 }));
    renderWithProviders(<Guide />);
    await screen.findByText('VRIX HD');

    await user.click(screen.getByRole('button', { name: 'Use VRIX for VRIX HD' }));

    await waitFor(() => expect(guideApi.acceptSuggestion).toHaveBeenCalled());
    expect(screen.getByText('likely VRIX')).toBeInTheDocument();
  });

  it('names the guide id when the candidate has no name', async () => {
    guideApi.grid.mockResolvedValue({
      ...GRID,
      channels: [
        {
          ...GRID.channels[2],
          epg_suggestion: { epg_data_id: 77, name: null, score: 60 },
        },
      ],
    });
    renderWithProviders(<Guide />);

    expect(await screen.findByText('likely guide 77')).toBeInTheDocument();
  });
});

describe('programmes', () => {
  it('renders a programme that started before the window, clipped', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    // The most visible way a guide looks broken is the current programme
    // simply missing from the row.
    const block = screen.getByText('Primetime in Milan').closest('[data-program]');
    expect(block).toBeInTheDocument();
    expect(block).toHaveAttribute('data-clipped-start', 'true');
    expect(block.style.left).toBe('0px');
  });

  it('does not mark a programme inside the window as clipped', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    const block = screen.getByText('PBS News Hour').closest('[data-program]');
    expect(block).not.toHaveAttribute('data-clipped-start');
    expect(block).not.toHaveAttribute('data-clipped-end');
  });

  it('marks the programme that is on air now', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    // NOW is 19:30; the film runs 17:00–20:00.
    expect(
      screen.getByText('Primetime in Milan').closest('[data-program]'),
    ).toHaveAttribute('data-on-air', 'true');
    expect(
      screen.getByText('PBS News Hour').closest('[data-program]'),
    ).not.toHaveAttribute('data-on-air');
  });

  it('shows the time range and flags a new episode', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    expect(screen.getByText(/New$/)).toBeInTheDocument();
  });
});

describe('navigating the window', () => {
  it('moves forward and reloads', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    const before = guideApi.grid.mock.calls[0][0].from.getTime();
    await user.click(screen.getByRole('button', { name: 'Later' }));

    await waitFor(() => expect(guideApi.grid).toHaveBeenCalledTimes(2));
    const after = guideApi.grid.mock.calls[1][0].from.getTime();
    expect(after - before).toBe(3 * HOUR);
  });

  it('moves backward', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    const before = guideApi.grid.mock.calls[0][0].from.getTime();
    await user.click(screen.getByRole('button', { name: 'Earlier' }));

    await waitFor(() => expect(guideApi.grid).toHaveBeenCalledTimes(2));
    expect(guideApi.grid.mock.calls[1][0].from.getTime() - before).toBe(-3 * HOUR);
  });

  it('returns to now', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    await user.click(screen.getByRole('button', { name: 'Later' }));
    await waitFor(() => expect(guideApi.grid).toHaveBeenCalledTimes(2));

    await user.click(screen.getByRole('button', { name: 'Now' }));
    await waitFor(() => expect(guideApi.grid).toHaveBeenCalledTimes(3));

    const first = guideApi.grid.mock.calls[0][0].from.getTime();
    expect(guideApi.grid.mock.calls[2][0].from.getTime()).toBe(first);
  });
});

describe('an empty lineup', () => {
  it('explains what the guide needs rather than showing a blank grid', async () => {
    guideApi.grid.mockResolvedValue({ start: at(0), end: at(24), channels: [] });
    renderWithProviders(<Guide />);

    expect(await screen.findByText('No channels to show')).toBeInTheDocument();
    expect(screen.getByText(/attach an EPG source/)).toBeInTheDocument();
  });
});

describe('staying current', () => {
  it('moves the on-air marker as time passes', async () => {
    renderWithProviders(<Guide />);
    await screen.findByText('KETC-HD');

    // 19:30: the film (17:00–20:00) is on air, the news (20:00–21:00) is not.
    expect(
      screen.getByText('Primetime in Milan').closest('[data-program]'),
    ).toHaveAttribute('data-on-air', 'true');

    // Half an hour later the news has started.
    await vi.advanceTimersByTimeAsync(31 * 60_000);

    await waitFor(() =>
      expect(screen.getByText('PBS News Hour').closest('[data-program]')).toHaveAttribute(
        'data-on-air',
        'true',
      ),
    );
    expect(
      screen.getByText('Primetime in Milan').closest('[data-program]'),
    ).not.toHaveAttribute('data-on-air');
  });
});

describe('virtualization', () => {
  const many = Array.from({ length: 60 }, (_, index) => ({
    id: 100 + index,
    uuid: `u${index}`,
    name: `Channel ${index}`,
    channel_number: index + 1,
    logo_url: null,
    group_name: 'Bulk',
    epg_data_id: null,
    epg_suggestion: null,
    programs: [],
  }));

  it('renders only a window of rows, not all sixty', async () => {
    guideApi.grid.mockResolvedValue({ start: at(0), end: at(24), channels: many });
    renderWithProviders(<Guide />);
    await screen.findByText('Channel 0');

    // Sixty channels times a 24-hour window is a lot of DOM; most of it is off
    // screen and never built.
    expect(screen.queryByText('Channel 59')).not.toBeInTheDocument();
    expect(screen.getByText(/60 channels/)).toBeInTheDocument();
  });

  it('renders different rows once scrolled', async () => {
    guideApi.grid.mockResolvedValue({ start: at(0), end: at(24), channels: many });
    const { container } = renderWithProviders(<Guide />);
    await screen.findByText('Channel 0');

    const scroller = container.querySelector('div[class*="grid"]');
    Object.defineProperty(scroller, 'scrollTop', { value: 74 * 30, writable: true });
    Object.defineProperty(scroller, 'clientHeight', { value: 600, writable: true });
    scroller.dispatchEvent(new Event('scroll', { bubbles: true }));

    await waitFor(() => expect(screen.getByText('Channel 30')).toBeInTheDocument());
    expect(screen.queryByText('Channel 0')).not.toBeInTheDocument();
  });
});
