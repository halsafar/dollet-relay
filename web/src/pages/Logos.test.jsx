import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Logos } from './Logos.jsx';
import { logos as logosApi } from '../api/resources.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  logos: {
    list: vi.fn(),
    create: vi.fn(),
    update: vi.fn(),
    remove: vi.fn(),
    bulkDelete: vi.fn(),
    cleanup: vi.fn(),
  },
}));

vi.mock('../notify.js', () => ({
  notifyDone: vi.fn(),
  notifyQuiet: vi.fn(),
  notifyError: vi.fn(),
}));

/**
 * Shaped as `logos::serialize` emits it. The interesting values differ: one
 * logo is heavily used, one is used once, one is orphaned — so a control that
 * confuses them cannot pass.
 */
const LOGOS = [
  {
    id: 1,
    name: 'KAZ',
    url: 'http://logos.test/kaz.png',
    channel_count: 4,
    is_used: true,
  },
  {
    id: 2,
    name: 'DRO',
    url: 'http://logos.test/dro.png',
    channel_count: 1,
    is_used: true,
  },
  {
    id: 3,
    name: 'orphan',
    url: 'http://dead.invalid/gone.png',
    channel_count: 0,
    is_used: false,
  },
];

function cellText(header) {
  const table = screen.getByRole('table', { name: 'Logos' });
  const headers = [...table.querySelectorAll('thead tr:first-child th')];
  const index = headers.findIndex((th) => th.textContent.trim() === header);
  if (index === -1) throw new Error(`no column headed "${header}"`);
  return [...table.querySelectorAll('tbody tr')].map(
    (row) => row.querySelectorAll('td')[index]?.textContent ?? '',
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  logosApi.list.mockResolvedValue({ results: LOGOS, count: 120 });
  logosApi.remove.mockResolvedValue(null);
  logosApi.update.mockResolvedValue(LOGOS[0]);
  logosApi.create.mockResolvedValue({ id: 9 });
  logosApi.bulkDelete.mockResolvedValue({ deleted: 2 });
  logosApi.cleanup.mockResolvedValue({ deleted: 7 });
});

describe('Logos', () => {
  it('lists artwork with its name and URL', async () => {
    renderWithProviders(<Logos />);

    expect(await screen.findByText('KAZ')).toBeInTheDocument();
    expect(cellText('Name')).toEqual(['KAZ', 'DRO', 'orphan']);
    expect(cellText('URL')).toContain('http://logos.test/kaz.png');
  });

  it('renders each logo image with a no-referrer policy', async () => {
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    const image = screen.getByRole('img', { name: 'KAZ' });
    expect(image).toHaveAttribute('src', 'http://logos.test/kaz.png');
    // Provider hosts must not learn which instance is asking.
    expect(image).toHaveAttribute('referrerpolicy', 'no-referrer');
    expect(image).toHaveAttribute('loading', 'lazy');
  });

  it('shows how many channels use each logo, and which are orphaned', async () => {
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    const used = cellText('Used by');
    expect(used[0]).toBe('4 channels');
    // Singular, because "1 channels" is the kind of thing nobody fixes later.
    expect(used[1]).toBe('1 channel');
    expect(used[2]).toBe('Unused');
  });

  it('pages on the server rather than locally', async () => {
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    expect(logosApi.list).toHaveBeenLastCalledWith(
      expect.objectContaining({ page: 1, pageSize: 50 }),
    );
    // Three rows on the page, 120 on the server.
    expect(screen.getByText('1 to 3 of 120')).toBeInTheDocument();
  });

  it('fetches once per mount', async () => {
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await waitFor(() => expect(logosApi.list).toHaveBeenCalledTimes(1));
  });

  it('sends the search to the server, which matches name or URL', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.type(screen.getByRole('textbox', { name: 'Search name or URL' }), 'kaz');

    await waitFor(() =>
      expect(logosApi.list).toHaveBeenLastCalledWith(
        expect.objectContaining({ search: 'kaz', page: 1 }),
      ),
    );
  });

  it('translates a column sort into the server ordering vocabulary', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: /Name/ }));

    await waitFor(() =>
      expect(logosApi.list).toHaveBeenLastCalledWith(
        expect.objectContaining({ ordering: 'name' }),
      ),
    );
  });

  it('surfaces a load failure rather than an empty table', async () => {
    logosApi.list.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderWithProviders(<Logos />);

    expect(await screen.findByText('Logos could not be loaded.')).toBeInTheDocument();
  });
});

describe('broken artwork', () => {
  it('replaces an image that fails to load with a placeholder', async () => {
    renderWithProviders(<Logos />);
    await screen.findByText('orphan');

    const broken = screen.getByRole('img', { name: 'orphan' });
    fireEvent.error(broken);

    // One dead host leaves one placeholder, not a hole, and the rest of the
    // grid is untouched.
    expect(
      await screen.findByRole('img', { name: 'orphan (artwork unavailable)' }),
    ).toBeInTheDocument();
    expect(screen.getByRole('img', { name: 'KAZ' })).toBeInTheDocument();
    expect(screen.getByText('orphan')).toBeInTheDocument();
  });

  it('keeps every other row rendering when several fail', async () => {
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    fireEvent.error(screen.getByRole('img', { name: 'KAZ' }));
    fireEvent.error(screen.getByRole('img', { name: 'DRO' }));

    await waitFor(() =>
      expect(screen.getAllByRole('img', { name: /artwork unavailable/ })).toHaveLength(2),
    );
    expect(cellText('Name')).toEqual(['KAZ', 'DRO', 'orphan']);
  });
});

describe('deleting logos', () => {
  it('warns that channels lose their artwork, because the server will not', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Delete KAZ' }));

    // `ON DELETE SET NULL` means this succeeds silently and strips artwork
    // from four channels. The confirmation is the only warning there is.
    expect(await screen.findByText(/used by 4 channels/i)).toBeInTheDocument();
    expect(screen.getByText(/no artwork/i)).toBeInTheDocument();
  });

  it('says plainly when a logo is safe to remove', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('orphan');

    await user.click(screen.getByRole('button', { name: 'Delete orphan' }));

    expect(await screen.findByText(/not used by any channel/i)).toBeInTheDocument();
  });

  it('deletes only after the confirmation is accepted', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('orphan');

    await user.click(screen.getByRole('button', { name: 'Delete orphan' }));
    expect(logosApi.remove).not.toHaveBeenCalled();

    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete' }),
    );

    await waitFor(() => expect(logosApi.remove).toHaveBeenCalledWith(3));
    expect(logosApi.list).toHaveBeenCalledTimes(2);
  });

  it('abandons the delete when the confirmation is cancelled', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('orphan');

    await user.click(screen.getByRole('button', { name: 'Delete orphan' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Cancel' }),
    );

    expect(logosApi.remove).not.toHaveBeenCalled();
  });

  it('bulk-deletes the selection by numeric id', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    const table = screen.getByRole('table', { name: 'Logos' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 1' }));
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 3' }));
    await user.click(screen.getByRole('button', { name: 'Delete' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete' }),
    );

    await waitFor(() => expect(logosApi.bulkDelete).toHaveBeenCalledWith([1, 3]));
  });

  it('removes every unused logo after confirmation', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Remove unused' }));
    await user.click(
      await within(screen.getByRole('dialog')).findByRole('button', {
        name: 'Remove unused',
      }),
    );

    await waitFor(() => expect(logosApi.cleanup).toHaveBeenCalled());
    expect(logosApi.list).toHaveBeenCalledTimes(2);
  });

  it('survives a rejected delete', async () => {
    const user = userEvent.setup();
    logosApi.remove.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    renderWithProviders(<Logos />);
    await screen.findByText('orphan');

    await user.click(screen.getByRole('button', { name: 'Delete orphan' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete' }),
    );

    await waitFor(() => expect(logosApi.remove).toHaveBeenCalled());
    expect(screen.getByText('orphan')).toBeInTheDocument();
  });
});

describe('creating and renaming logos', () => {
  it('creates a logo from a URL', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Add logo' }));
    const dialog = screen.getByRole('dialog');
    await user.type(within(dialog).getByLabelText(/URL/), 'http://logos.test/kru.png');
    await user.type(within(dialog).getByLabelText(/Name/), 'KRU');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(logosApi.create).toHaveBeenCalledWith('KRU', 'http://logos.test/kru.png'),
    );
    expect(logosApi.list).toHaveBeenCalledTimes(2);
  });

  it('falls back to the URL when no name is given', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Add logo' }));
    const dialog = screen.getByRole('dialog');
    await user.type(within(dialog).getByLabelText(/URL/), 'http://logos.test/kru.png');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(logosApi.create).toHaveBeenCalledWith(
        'http://logos.test/kru.png',
        'http://logos.test/kru.png',
      ),
    );
  });

  it('requires a URL', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Add logo' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Save' }),
    );

    expect(await screen.findByText('Required')).toBeInTheDocument();
    expect(logosApi.create).not.toHaveBeenCalled();
  });

  it('renames an existing logo', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Edit KAZ' }));
    const dialog = screen.getByRole('dialog');
    await user.clear(within(dialog).getByLabelText(/Name/));
    await user.type(within(dialog).getByLabelText(/Name/), 'KAZ HD');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(logosApi.update).toHaveBeenCalledWith(1, {
        name: 'KAZ HD',
        url: 'http://logos.test/kaz.png',
      }),
    );
  });

  it('explains a duplicate URL instead of showing the SQLite message', async () => {
    const user = userEvent.setup();
    logosApi.create.mockRejectedValue(
      new ApiError('UNIQUE constraint failed: logo.url', { status: 409 }),
    );
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Add logo' }));
    const dialog = screen.getByRole('dialog');
    await user.type(within(dialog).getByLabelText(/URL/), 'http://logos.test/kaz.png');
    await user.click(within(dialog).getByRole('button', { name: 'Save' }));

    // The URL column is a unique index; the raw message is unreadable.
    expect(
      await within(dialog).findByText('A logo with that URL already exists.'),
    ).toBeInTheDocument();
    expect(screen.queryByText(/UNIQUE constraint/)).not.toBeInTheDocument();
  });

  it('previews the artwork as the URL is typed', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Add logo' }));
    const dialog = screen.getByRole('dialog');

    expect(within(dialog).getByRole('img', { name: 'No artwork' })).toBeInTheDocument();

    await user.type(within(dialog).getByLabelText(/URL/), 'http://logos.test/kru.png');

    await waitFor(() =>
      expect(within(dialog).getByRole('img', { name: 'Logo preview' })).toHaveAttribute(
        'src',
        'http://logos.test/kru.png',
      ),
    );
  });
});

describe('failure paths', () => {
  it('survives a rejected cleanup', async () => {
    const user = userEvent.setup();
    logosApi.cleanup.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    await user.click(screen.getByRole('button', { name: 'Remove unused' }));
    await user.click(
      await within(screen.getByRole('dialog')).findByRole('button', {
        name: 'Remove unused',
      }),
    );

    await waitFor(() => expect(logosApi.cleanup).toHaveBeenCalled());
    expect(screen.getByText('KAZ')).toBeInTheDocument();
  });

  it('survives a rejected bulk delete', async () => {
    const user = userEvent.setup();
    logosApi.bulkDelete.mockRejectedValue(new ApiError('Boom', { status: 500 }));
    renderWithProviders(<Logos />);
    await screen.findByText('KAZ');

    const table = screen.getByRole('table', { name: 'Logos' });
    await user.click(within(table).getByRole('checkbox', { name: 'Select row 1' }));
    await user.click(screen.getByRole('button', { name: 'Delete' }));
    await user.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'Delete' }),
    );

    await waitFor(() => expect(logosApi.bulkDelete).toHaveBeenCalled());
    expect(screen.getByText('KAZ')).toBeInTheDocument();
  });
});
