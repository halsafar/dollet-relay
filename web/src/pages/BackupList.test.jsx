import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { notifications } from '@mantine/notifications';

import { BackupList } from './BackupList.jsx';
import { backups, fetchVersion } from '../api/resources.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  backups: {
    list: vi.fn(),
    create: vi.fn(),
    download: vi.fn(),
    upload: vi.fn(),
    restore: vi.fn(),
    remove: vi.fn(),
  },
  fetchVersion: vi.fn(),
}));

vi.mock('@mantine/notifications', () => ({
  notifications: { show: vi.fn() },
}));

const SCHEDULED = {
  name: 'dollet-backup-20261001-030000-scheduled.zip',
  created_at: '2026-10-01T03:00:00Z',
  trigger: 'scheduled',
  size_bytes: 1_572_864,
  version: '0.9.1',
  schema: 2,
};

const UNREADABLE = {
  name: 'dollet-backup-20260901-120000-uploaded.zip',
  created_at: '2026-09-01T12:00:00Z',
  trigger: 'uploaded',
  size_bytes: 512,
  version: null,
  schema: null,
};

const MANUAL = {
  name: 'dollet-backup-20261001-090807-manual.zip',
  created_at: '2026-10-01T09:08:07Z',
  trigger: 'manual',
  size_bytes: 1_600_000,
  version: '0.9.1',
  schema: 2,
};

beforeEach(() => {
  vi.clearAllMocks();
  backups.list.mockResolvedValue([SCHEDULED, UNREADABLE]);
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function row(name) {
  return within(screen.getByRole('cell', { name }).closest('tr'));
}

/** The last notification shown, as `{title, message, color}`. */
function lastNotice() {
  return notifications.show.mock.calls.at(-1)?.[0];
}

describe('the backup list', () => {
  it('lists every backup with why it exists and how large it is', async () => {
    renderWithProviders(<BackupList />);

    expect(await screen.findByRole('cell', { name: SCHEDULED.name })).toBeVisible();
    expect(row(SCHEDULED.name).getByText('Scheduled')).toBeVisible();
    expect(row(SCHEDULED.name).getByText('1.5 MB')).toBeVisible();
    // Listed even when its metadata cannot be read, so it can still be deleted.
    expect(row(UNREADABLE.name).getByText('Uploaded')).toBeVisible();
    expect(row(UNREADABLE.name).getByText('512 B')).toBeVisible();
  });

  it('says so when there are none', async () => {
    backups.list.mockResolvedValue([]);
    renderWithProviders(<BackupList />);

    expect(await screen.findByText('No backups yet.')).toBeVisible();
  });

  it('shows a load failure instead of an empty list', async () => {
    backups.list.mockRejectedValue(new ApiError('Forbidden.', { status: 403 }));
    renderWithProviders(<BackupList />);

    expect(await screen.findByText('Forbidden.')).toBeVisible();
    expect(screen.queryByText('No backups yet.')).not.toBeInTheDocument();
  });

  it('shows a trigger it does not know by its own name', async () => {
    backups.list.mockResolvedValue([{ ...SCHEDULED, trigger: 'imported' }]);
    renderWithProviders(<BackupList />);

    expect(await screen.findByText('imported')).toBeVisible();
  });
});

describe('taking a backup', () => {
  it('adds the new backup to the list', async () => {
    const user = userEvent.setup();
    backups.create.mockResolvedValue(MANUAL);
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    backups.list.mockResolvedValue([MANUAL, SCHEDULED, UNREADABLE]);
    await user.click(screen.getByRole('button', { name: /Back up now/ }));

    expect(await screen.findByRole('cell', { name: MANUAL.name })).toBeVisible();
    expect(backups.create).toHaveBeenCalledTimes(1);
    expect(lastNotice().message).toBe(`Backed up as ${MANUAL.name}`);
  });

  it('says why when the server refuses', async () => {
    const user = userEvent.setup();
    backups.create.mockRejectedValue(
      new ApiError('a backup is already running', { status: 409 }),
    );
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: /Back up now/ }));

    await waitFor(() =>
      expect(lastNotice()).toMatchObject({
        title: 'Could not back up',
        message: 'a backup is already running',
        color: 'red',
      }),
    );
  });
});

describe('downloading', () => {
  it('fetches the file with the session and saves it under its own name', async () => {
    const user = userEvent.setup();
    const file = new Blob(['PK']);
    backups.download.mockResolvedValue(file);
    const createObjectURL = vi.fn(() => 'blob:backup');
    const revokeObjectURL = vi.fn();
    vi.stubGlobal('URL', { ...URL, createObjectURL, revokeObjectURL });
    const saved = [];
    const click = vi
      .spyOn(HTMLAnchorElement.prototype, 'click')
      .mockImplementation(function record() {
        saved.push({ href: this.href, download: this.download });
      });
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Download ${SCHEDULED.name}` }));

    await waitFor(() => expect(saved).toHaveLength(1));
    expect(backups.download).toHaveBeenCalledWith(SCHEDULED.name);
    expect(createObjectURL).toHaveBeenCalledWith(file);
    expect(saved[0]).toEqual({ href: 'blob:backup', download: SCHEDULED.name });
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:backup');
    click.mockRestore();
  });

  it('says so when the file cannot be fetched', async () => {
    const user = userEvent.setup();
    backups.download.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Download ${SCHEDULED.name}` }));

    await waitFor(() =>
      expect(lastNotice()).toMatchObject({
        title: `Could not download ${SCHEDULED.name}`,
        message: 'Not found.',
      }),
    );
  });
});

describe('uploading', () => {
  it('posts the chosen file and lists what the server named it', async () => {
    const user = userEvent.setup();
    const uploadedEntry = {
      ...MANUAL,
      trigger: 'uploaded',
      name: MANUAL.name.replace('manual', 'uploaded'),
    };
    backups.upload.mockResolvedValue(uploadedEntry);
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    backups.list.mockResolvedValue([uploadedEntry, SCHEDULED, UNREADABLE]);
    const file = new File(['PK'], 'from-the-old-box.zip', { type: 'application/zip' });
    // Mantine's FileButton renders its input outside the button it labels.
    await user.upload(document.querySelector('input[type="file"]'), file);

    expect(await screen.findByRole('cell', { name: uploadedEntry.name })).toBeVisible();
    expect(backups.upload).toHaveBeenCalledWith(file);
    expect(lastNotice().message).toBe(`Uploaded as ${uploadedEntry.name}`);
  });

  it('does nothing when the picker is closed without a file', async () => {
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    // A change with no file is what Mantine hands on as `null`.
    fireEvent.change(document.querySelector('input[type="file"]'), {
      target: { files: [] },
    });

    expect(backups.upload).not.toHaveBeenCalled();
  });

  it('shows the reason a file is not a backup', async () => {
    const user = userEvent.setup();
    backups.upload.mockRejectedValue(
      new ApiError('not a usable backup: it is not a zip archive', { status: 400 }),
    );
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    const file = new File(['nope'], 'notes.zip', { type: 'application/zip' });
    await user.upload(document.querySelector('input[type="file"]'), file);

    await waitFor(() =>
      expect(lastNotice()).toMatchObject({
        title: 'Could not upload notes.zip',
        message: 'not a usable backup: it is not a zip archive',
      }),
    );
  });
});

describe('deleting', () => {
  it('asks first, then deletes and lists what is left', async () => {
    const user = userEvent.setup();
    backups.remove.mockResolvedValue(null);
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Delete ${SCHEDULED.name}` }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText(/there is no undo/)).toBeVisible();
    expect(backups.remove).not.toHaveBeenCalled();

    backups.list.mockResolvedValue([UNREADABLE]);
    await user.click(within(dialog).getByRole('button', { name: 'Delete' }));

    await waitFor(() =>
      expect(
        screen.queryByRole('cell', { name: SCHEDULED.name }),
      ).not.toBeInTheDocument(),
    );
    expect(backups.remove).toHaveBeenCalledWith(SCHEDULED.name);
  });

  it('says so when the delete fails', async () => {
    const user = userEvent.setup();
    backups.remove.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Delete ${SCHEDULED.name}` }));
    await user.click(
      within(await screen.findByRole('dialog')).getByRole('button', { name: 'Delete' }),
    );

    await waitFor(() =>
      expect(lastNotice()).toMatchObject({ title: `Could not delete ${SCHEDULED.name}` }),
    );
  });
});

describe('restoring', () => {
  it('warns what a restore does before doing it', async () => {
    const user = userEvent.setup();
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Restore ${SCHEDULED.name}` }));
    const dialog = await screen.findByRole('dialog');

    expect(within(dialog).getByText(/Everything in this instance/)).toHaveTextContent(
      /is replaced by this backup/,
    );
    expect(within(dialog).getByText(/is taken first/)).toBeVisible();
    expect(within(dialog).getByText(/The server\s+then restarts/)).toBeVisible();

    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    expect(backups.restore).not.toHaveBeenCalled();
  });

  it('restarts into the backup and reloads once the server is back', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    const reload = vi.fn();
    vi.stubGlobal('location', { ...window.location, reload });
    backups.restore.mockResolvedValue({
      restarting: true,
      pre_restore: 'dollet-backup-20261001-100000-pre-restore.zip',
    });
    // Still answering as the restore is accepted, then gone, then back.
    fetchVersion
      .mockResolvedValueOnce('0.9.1')
      .mockResolvedValueOnce(null)
      .mockResolvedValue('0.9.1');
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Restore ${SCHEDULED.name}` }));
    await user.click(
      within(await screen.findByRole('dialog')).getByRole('button', { name: 'Restore' }),
    );

    const status = await screen.findByRole('status');
    expect(status).toHaveTextContent(`Restoring ${SCHEDULED.name}`);
    expect(status).toHaveTextContent('dollet-backup-20261001-100000-pre-restore.zip');
    expect(backups.restore).toHaveBeenCalledWith(SCHEDULED.name);

    // Answering before it ever went away is the old process, not the restore.
    await vi.advanceTimersByTimeAsync(1000);
    expect(fetchVersion).toHaveBeenCalledTimes(1);
    expect(reload).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(1000);
    expect(reload).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(1000);
    await waitFor(() => expect(reload).toHaveBeenCalledTimes(1));
  });

  it('stops asking once the page has moved on', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    const reload = vi.fn();
    vi.stubGlobal('location', { ...window.location, reload });
    backups.restore.mockResolvedValue({
      restarting: true,
      pre_restore: 'dollet-backup-20261001-100000-pre-restore.zip',
    });
    let answer;
    fetchVersion.mockReturnValueOnce(
      new Promise((resolve) => {
        answer = resolve;
      }),
    );
    const { unmount } = renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Restore ${SCHEDULED.name}` }));
    await user.click(
      within(await screen.findByRole('dialog')).getByRole('button', { name: 'Restore' }),
    );
    await screen.findByRole('status');
    await vi.advanceTimersByTimeAsync(1000);
    expect(fetchVersion).toHaveBeenCalledTimes(1);

    unmount();
    answer(null);
    await vi.advanceTimersByTimeAsync(5000);

    expect(fetchVersion).toHaveBeenCalledTimes(1);
    expect(reload).not.toHaveBeenCalled();
  });

  it('offers a reload by hand while it waits', async () => {
    const user = userEvent.setup();
    const reload = vi.fn();
    vi.stubGlobal('location', { ...window.location, reload });
    backups.restore.mockResolvedValue({
      restarting: true,
      pre_restore: 'dollet-backup-20261001-100000-pre-restore.zip',
    });
    fetchVersion.mockResolvedValue(null);
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Restore ${SCHEDULED.name}` }));
    await user.click(
      within(await screen.findByRole('dialog')).getByRole('button', { name: 'Restore' }),
    );
    await user.click(await screen.findByRole('button', { name: 'Reload now' }));

    expect(reload).toHaveBeenCalledTimes(1);
  });

  it('keeps the list and says why when the restore is refused', async () => {
    const user = userEvent.setup();
    backups.restore.mockRejectedValue(
      new ApiError('not a usable backup: it was made by a newer build', { status: 400 }),
    );
    renderWithProviders(<BackupList />);
    await screen.findByRole('cell', { name: SCHEDULED.name });

    await user.click(screen.getByRole('button', { name: `Restore ${SCHEDULED.name}` }));
    await user.click(
      within(await screen.findByRole('dialog')).getByRole('button', { name: 'Restore' }),
    );

    await waitFor(() =>
      expect(lastNotice()).toMatchObject({
        title: `Could not restore ${SCHEDULED.name}`,
        message: 'not a usable backup: it was made by a newer build',
      }),
    );
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
    expect(screen.getByRole('cell', { name: SCHEDULED.name })).toBeVisible();
  });
});
