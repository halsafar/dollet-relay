import { beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Route, Routes } from 'react-router-dom';

import { SETTINGS_ROUTE, Settings } from './Settings.jsx';
import {
  backups,
  outputProfiles,
  settings as settingsApi,
  streamProfiles,
  userAgents,
} from '../api/resources.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';
import { useAppearance } from '../appearance.js';
import { useUnsavedChanges } from '../unsavedChanges.js';

vi.mock('../api/resources.js', () => ({
  settings: { list: vi.fn(), update: vi.fn() },
  userAgents: { list: vi.fn() },
  streamProfiles: { list: vi.fn() },
  outputProfiles: { list: vi.fn() },
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

const USER_AGENTS = [
  { id: 1, name: 'Kodi/21.0' },
  { id: 2, name: 'VLC/3.0.20' },
];

const STREAM_PROFILES = [
  { id: 1, name: 'ffmpeg' },
  { id: 3, name: 'proxy' },
  { id: 4, name: 'redirect' },
];

const OUTPUT_PROFILES = [{ id: 2, name: 'AC3 audio' }];

const GROUPS = [
  // First, where the server's own order puts it nowhere near: the page orders
  // the sections itself.
  {
    key: 'backup_settings',
    name: 'Backups',
    value: { interval_hours: 24, keep: 7 },
  },
  {
    key: 'proxy_settings',
    name: 'Proxy Settings',
    value: { ring_seconds: 15, buffering_timeout: 5 },
  },
  {
    key: 'system_settings',
    name: 'System Settings',
    value: { preferred_region: null, max_system_events: 100 },
  },
  {
    key: 'epg_settings',
    name: 'EPG Settings',
    value: { epg_auto_match_on_refresh: true, epg_match_ignore_prefixes: ['US:'] },
  },
  { key: 'network_access', name: 'Network Access', value: {} },
];

beforeEach(() => {
  vi.clearAllMocks();
  // The stores outlive a test, and the setup file clears only their storage.
  useAppearance.setState(useAppearance.getInitialState());
  useUnsavedChanges.setState(useUnsavedChanges.getInitialState());
  settingsApi.list.mockResolvedValue(GROUPS);
  userAgents.list.mockResolvedValue(USER_AGENTS);
  streamProfiles.list.mockResolvedValue(STREAM_PROFILES);
  outputProfiles.list.mockResolvedValue(OUTPUT_PROFILES);
  backups.list.mockResolvedValue([]);
});

/**
 * The page on the route it owns, so the section segment reaches it. With no
 * section named it lands on the first, which for `GROUPS` is Proxy.
 */
function renderSettings(route = '/settings') {
  return renderWithProviders(
    <Routes>
      <Route path={SETTINGS_ROUTE} element={<Settings />} />
    </Routes>,
    { route },
  );
}

/**
 * The section on screen: a `<section>` named by its heading, so "Save" means
 * that section's button. One is shown at a time, so this also asserts which.
 */
function section(name) {
  return within(screen.getByRole('region', { name }));
}

/** Opens a section from the list beside the form. */
async function open(user, name) {
  await user.click(screen.getByRole('link', { name }));
  return within(await screen.findByRole('region', { name }));
}

describe('Settings page', () => {
  it('lists a section per group in its own order, with appearance last', async () => {
    renderSettings();
    await screen.findByRole('region', { name: 'Proxy' });

    const listed = within(screen.getByRole('navigation', { name: 'Settings sections' }))
      .getAllByRole('link')
      .map((link) => link.textContent);

    // The server put Backups first; the page does not follow it.
    expect(listed).toEqual([
      'Proxy',
      'Guide matching',
      'System',
      'Backups',
      'Network access',
      'Appearance',
    ]);
  });

  it('shows one section at a time, chosen from the list', async () => {
    const user = userEvent.setup();
    renderSettings();
    await screen.findByRole('region', { name: 'Proxy' });

    expect(
      screen.queryByRole('switch', { name: /Auto-match on refresh/ }),
    ).not.toBeInTheDocument();

    const guide = await open(user, 'Guide matching');
    expect(guide.getByRole('switch', { name: /Auto-match on refresh/ })).toBeVisible();
    expect(screen.queryByRole('region', { name: 'Proxy' })).not.toBeInTheDocument();
  });

  it('opens the section the URL names, and the first for one it does not know', async () => {
    const { unmount } = renderSettings('/settings/system');
    expect(await screen.findByRole('region', { name: 'System' })).toBeVisible();
    expect(screen.queryByRole('region', { name: 'Proxy' })).not.toBeInTheDocument();
    unmount();

    renderSettings('/settings/no-such-section');
    expect(await screen.findByRole('region', { name: 'Proxy' })).toBeVisible();
  });

  it('lists the backups under the schedule that writes them', async () => {
    const user = userEvent.setup();
    settingsApi.update.mockResolvedValue({
      key: 'backup_settings',
      name: 'Backups',
      value: { interval_hours: 6, keep: 7 },
    });
    renderSettings('/settings/backup');

    const backupsSection = within(await screen.findByRole('region', { name: 'Backups' }));
    expect(await backupsSection.findByText('No backups yet.')).toBeVisible();
    expect(backupsSection.getByRole('button', { name: /Back up now/ })).toBeVisible();

    const interval = backupsSection.getByRole('textbox', { name: /Backup interval/ });
    await user.clear(interval);
    await user.type(interval, '6');
    await user.click(backupsSection.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('backup_settings', {
        interval_hours: 6,
      }),
    );
  });

  it('labels a known field from its metadata rather than its key', async () => {
    renderSettings();
    await screen.findByRole('region', { name: 'Proxy' });

    expect(screen.getByText('Ring retention')).toBeInTheDocument();
    expect(screen.queryByText('Ring seconds')).not.toBeInTheDocument();
  });

  it('surfaces a load failure', async () => {
    settingsApi.list.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderSettings();

    expect(await screen.findByText('Not found.')).toBeInTheDocument();
  });

  it('says so when the server returns nothing', async () => {
    settingsApi.list.mockResolvedValue([]);
    renderSettings();

    expect(
      await screen.findByText('The server returned no settings.'),
    ).toBeInTheDocument();
  });

  it('sends only the changed fields, because the server merges a partial', async () => {
    const user = userEvent.setup();
    settingsApi.update.mockResolvedValue({
      key: 'proxy_settings',
      name: 'Proxy Settings',
      value: { ring_seconds: 30, buffering_timeout: 5 },
    });
    renderSettings();
    await screen.findByText('Ring retention');

    const input = screen.getByRole('textbox', { name: /Ring retention/ });
    await user.clear(input);
    await user.type(input, '30');
    await user.click(section('Proxy').getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('proxy_settings', {
        ring_seconds: 30,
      }),
    );
  });

  it('offers Save only once something has changed', async () => {
    const user = userEvent.setup();
    renderSettings();
    await screen.findByText('Ring retention');

    expect(
      section('Proxy').queryByRole('button', { name: 'Save' }),
    ).not.toBeInTheDocument();

    const input = screen.getByRole('textbox', { name: /Ring retention/ });
    await user.clear(input);
    await user.type(input, '30');

    expect(section('Proxy').getByText('Unsaved changes')).toBeVisible();
    expect(section('Proxy').getByRole('button', { name: 'Save' })).toBeEnabled();
  });

  it('reverts a draft back to the loaded values', async () => {
    const user = userEvent.setup();
    renderSettings();
    await screen.findByText('Ring retention');

    const input = screen.getByRole('textbox', { name: /Ring retention/ });
    await user.clear(input);
    await user.type(input, '30');
    await user.click(section('Proxy').getByRole('button', { name: 'Revert' }));

    expect(input).toHaveValue('15');
    expect(
      section('Proxy').queryByRole('button', { name: 'Save' }),
    ).not.toBeInTheDocument();
    expect(settingsApi.update).not.toHaveBeenCalled();
  });

  it('leaves the draft editable when a save is rejected', async () => {
    const user = userEvent.setup();
    settingsApi.update.mockRejectedValue(new ApiError('Bad value', { status: 400 }));
    renderSettings();
    await screen.findByText('Ring retention');

    const input = screen.getByRole('textbox', { name: /Ring retention/ });
    await user.clear(input);
    await user.type(input, '30');
    const save = section('Proxy').getByRole('button', { name: 'Save' });
    await user.click(save);

    await waitFor(() => expect(save).toBeEnabled());
    expect(input).toHaveValue('30');
  });

  it('renders a switch for a boolean and a tag list for an array', async () => {
    const user = userEvent.setup();
    renderSettings();
    await screen.findByRole('region', { name: 'Proxy' });

    const guide = await open(user, 'Guide matching');
    expect(guide.getByRole('switch', { name: /Auto-match on refresh/ })).toBeChecked();
    expect(guide.getByText('US:')).toBeInTheDocument();
  });

  it('says a group with no entries has none, rather than showing nothing', async () => {
    // Not `network_access`, which renders its endpoint classes whether or not
    // the stored map has them — an empty map is the state it is meant to fix.
    settingsApi.list.mockResolvedValue([
      { key: 'system_settings', name: 'System Settings', value: {} },
    ]);
    renderSettings();

    expect(
      await screen.findByText('Nothing configured in this section.'),
    ).toBeInTheDocument();
  });

  it('renders one input per entry in an endpoint-to-CIDR map', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue([
      {
        key: 'network_access',
        name: 'Network Access',
        value: { UI: '10.0.0.0/8', STREAMS: '' },
      },
    ]);
    settingsApi.update.mockResolvedValue({
      key: 'network_access',
      name: 'Network Access',
      value: { UI: '10.0.0.0/8', STREAMS: '192.168.1.0/24' },
    });
    renderSettings();

    expect(await screen.findByRole('textbox', { name: 'Web app and API' })).toHaveValue(
      '10.0.0.0/8',
    );

    await user.type(screen.getByRole('textbox', { name: 'Streams' }), '192.168.1.0/24');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    // The WHOLE map, not just the changed entry. The backend replaces this
    // group rather than merging it, and a missing endpoint key means "allow
    // everyone" — so a partial PATCH here would open the UI to the internet.
    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('network_access', {
        UI: '10.0.0.0/8',
        STREAMS: '192.168.1.0/24',
      }),
    );
  });

  it('still sends only the changed fields for a group the server merges', async () => {
    const user = userEvent.setup();
    settingsApi.update.mockResolvedValue(GROUPS[0]);
    renderSettings();
    await screen.findByText('Ring retention');

    const input = screen.getByRole('textbox', { name: /Ring retention/ });
    await user.clear(input);
    await user.type(input, '30');
    await user.click(section('Proxy').getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('proxy_settings', {
        ring_seconds: 30,
      }),
    );
  });

  it('sends null, not zero, when a reference is set back to nothing', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue([
      {
        key: 'stream_settings',
        name: 'Stream Settings',
        value: { default_stream_profile: 3 },
      },
    ]);
    settingsApi.update.mockResolvedValue({
      key: 'stream_settings',
      name: 'Stream Settings',
      value: { default_stream_profile: null },
    });
    renderSettings();

    await user.click(
      await screen.findByRole('textbox', { name: /Default stream profile/ }),
    );
    await user.click(
      await screen.findByRole('option', { name: 'Not set', hidden: true }),
    );
    await user.click(screen.getByRole('button', { name: 'Save' }));

    // Coercing an emptied field to 0 would silently select profile 0.
    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('stream_settings', {
        default_stream_profile: null,
      }),
    );
  });

  it('sends null, not an empty string, when a nullable string is cleared', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue([
      {
        key: 'system_settings',
        name: 'System Settings',
        value: { preferred_region: 'us' },
      },
    ]);
    settingsApi.update.mockResolvedValue({
      key: 'system_settings',
      name: 'System Settings',
      value: { preferred_region: null },
    });
    renderSettings();

    await user.clear(await screen.findByRole('textbox', { name: /Preferred region/ }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    // The server holds an `Option<String>`; '' would fail to deserialize.
    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('system_settings', {
        preferred_region: null,
      }),
    );
  });

  it('keeps an empty string for a field not declared nullable', async () => {
    // Every string setting this build ships is declared nullable, so no real
    // payload reaches this path today. The rule it pins still holds: the form
    // does not invent `null`, because a field the server types as `String`
    // fails to deserialize one and the section stops being editable. A field
    // undeclared in `FIELD_META` is the case, so it is the case tested.
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue([
      {
        key: 'stream_settings',
        name: 'Stream Settings',
        value: { undeclared_text: 'kept' },
      },
    ]);
    settingsApi.update.mockResolvedValue({
      key: 'stream_settings',
      name: 'Stream Settings',
      value: { undeclared_text: '' },
    });
    renderSettings();

    await user.clear(await screen.findByRole('textbox', { name: /Undeclared text/i }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('stream_settings', {
        undeclared_text: '',
      }),
    );
  });

  it('shows a nested object read-only rather than as [object Object]', async () => {
    settingsApi.list.mockResolvedValue([
      {
        key: 'system_settings',
        name: 'System Settings',
        value: { nested: { a: 1 } },
      },
    ]);
    renderSettings();

    expect(await screen.findByText('Not editable here.')).toBeInTheDocument();
    expect(screen.queryByDisplayValue('[object Object]')).not.toBeInTheDocument();
  });

  it('falls back to a derived label for a field it has never seen', async () => {
    settingsApi.list.mockResolvedValue([
      { key: 'proxy_settings', name: 'Proxy Settings', value: { brand_new_knob: 'x' } },
    ]);
    renderSettings();

    expect(await screen.findByText('Brand new knob')).toBeInTheDocument();
  });
});

describe('foreign-key settings', () => {
  const streamGroup = (value) => [
    { key: 'stream_settings', name: 'Stream Settings', value },
  ];

  it('renders each foreign key as a select over its own list', async () => {
    settingsApi.list.mockResolvedValue(
      streamGroup({
        default_user_agent: 1,
        default_stream_profile: 3,
        hdhr_output_profile_id: 2,
      }),
    );
    renderSettings();

    // The names, not 1/3/2. Nobody knows which row `3` is.
    expect(
      await screen.findByRole('textbox', { name: /Default user agent/ }),
    ).toHaveValue('Kodi/21.0');
    expect(screen.getByRole('textbox', { name: /Default stream profile/ })).toHaveValue(
      'proxy',
    );
    expect(screen.getByRole('textbox', { name: /HDHR output profile/ })).toHaveValue(
      'AC3 audio',
    );

    // Once for the page, not once per field that points at a list.
    expect(streamProfiles.list).toHaveBeenCalledTimes(1);
    expect(userAgents.list).toHaveBeenCalledTimes(1);
    expect(outputProfiles.list).toHaveBeenCalledTimes(1);
  });

  it('shows an unset reference as Not set, and offers that as an option', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue(streamGroup({ hdhr_output_profile_id: null }));
    renderSettings();

    const input = await screen.findByRole('textbox', { name: /HDHR output profile/ });
    expect(input).toHaveValue('');
    expect(input).toHaveAttribute('placeholder', 'Not set');

    // The empty option, so "no transcoding" is a thing you can pick rather
    // than only a thing you can clear.
    await user.click(input);
    expect(
      await screen.findByRole('option', { name: 'Not set', hidden: true }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole('option', { name: 'AC3 audio', hidden: true }),
    ).toBeInTheDocument();
  });

  it('sends the numeric id the server stores, not the label', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue(streamGroup({ default_user_agent: null }));
    settingsApi.update.mockResolvedValue({
      key: 'stream_settings',
      name: 'Stream Settings',
      value: { default_user_agent: 2 },
    });
    renderSettings();

    await user.click(await screen.findByRole('textbox', { name: /Default user agent/ }));
    await user.click(
      await screen.findByRole('option', { name: 'VLC/3.0.20', hidden: true }),
    );
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('stream_settings', {
        default_user_agent: 2,
      }),
    );
  });

  it('disambiguates with the row id only when two entries share a name', async () => {
    streamProfiles.list.mockResolvedValue([
      { id: 1, name: 'ffmpeg' },
      { id: 5, name: 'ffmpeg' },
      { id: 3, name: 'proxy' },
    ]);
    settingsApi.list.mockResolvedValue(streamGroup({ default_stream_profile: 3 }));
    const user = userEvent.setup();
    renderSettings();

    await user.click(
      await screen.findByRole('textbox', { name: /Default stream profile/ }),
    );

    expect(
      await screen.findByRole('option', { name: 'ffmpeg (#5)', hidden: true }),
    ).toBeInTheDocument();
    // The unambiguous one keeps its bare name.
    expect(
      screen.getByRole('option', { name: 'proxy', hidden: true }),
    ).toBeInTheDocument();
  });

  it('waits for the list rather than offering an empty select', async () => {
    // Never resolves: the page has its settings but not its options yet.
    userAgents.list.mockReturnValue(new Promise(() => {}));
    settingsApi.list.mockResolvedValue(streamGroup({ default_user_agent: 1 }));
    renderSettings();

    const input = await screen.findByRole('textbox', { name: /Default user agent/ });
    expect(input).toBeDisabled();
    expect(input).toHaveAttribute('placeholder', 'Loading…');
  });

  it('falls back to the row id box when the list will not load', async () => {
    const user = userEvent.setup();
    userAgents.list.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    settingsApi.list.mockResolvedValue(streamGroup({ default_user_agent: null }));
    settingsApi.update.mockResolvedValue({
      key: 'stream_settings',
      name: 'Stream Settings',
      value: { default_user_agent: 3 },
    });
    renderSettings();

    expect(
      await screen.findByText(/Could not list User agents: Not found\./),
    ).toBeInTheDocument();

    // A dead list must not take the setting with it.
    const input = await screen.findByRole('textbox', { name: /Default user agent/ });
    await user.type(input, '3');
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('stream_settings', {
        default_user_agent: 3,
      }),
    );
  });

  it('still sends null, not zero, from the fallback box', async () => {
    const user = userEvent.setup();
    streamProfiles.list.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    settingsApi.list.mockResolvedValue(streamGroup({ default_stream_profile: 3 }));
    settingsApi.update.mockResolvedValue({
      key: 'stream_settings',
      name: 'Stream Settings',
      value: { default_stream_profile: null },
    });
    renderSettings();

    await user.clear(
      await screen.findByRole('textbox', { name: /Default stream profile/ }),
    );
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('stream_settings', {
        default_stream_profile: null,
      }),
    );
  });
});

describe('the stream identity key', () => {
  const hashGroup = (value) => [
    { key: 'stream_settings', name: 'Stream Settings', value: { m3u_hash_key: value } },
  ];

  it('offers exactly the tokens the server parses', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue(hashGroup('url'));
    renderSettings();

    await user.click(await screen.findByRole('textbox', { name: /Stream identity key/ }));

    const offered = screen
      .getAllByRole('option', { hidden: true })
      .map((option) => option.textContent);
    expect(offered).toEqual(['group', 'm3u_id', 'name', 'tvg_id', 'url']);
  });

  it('round-trips a selection to the comma-joined string', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue(hashGroup('url'));
    settingsApi.update.mockResolvedValue({
      key: 'stream_settings',
      name: 'Stream Settings',
      value: { m3u_hash_key: 'name,url' },
    });
    renderSettings();

    await user.click(await screen.findByRole('textbox', { name: /Stream identity key/ }));
    await user.click(await screen.findByRole('option', { name: 'name', hidden: true }));
    await user.click(screen.getByRole('button', { name: 'Save' }));

    // Canonical order, which is what the backend sorts the hash object into.
    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('stream_settings', {
        m3u_hash_key: 'name,url',
      }),
    );
  });

  it('refuses to save an empty selection rather than sending ""', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue(hashGroup('url'));
    renderSettings();

    // Deselecting the last token. An empty key hashes every stream in an
    // account alike, and the refresh that discovers it refuses to write.
    await user.click(await screen.findByRole('textbox', { name: /Stream identity key/ }));
    await user.click(await screen.findByRole('option', { name: 'url', hidden: true }));

    expect(await screen.findByText(/Pick at least one field/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();

    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(settingsApi.update).not.toHaveBeenCalled();
  });
});

describe('network access', () => {
  const networkGroup = (value) => [
    { key: 'network_access', name: 'Network Access', value },
  ];

  it('renders every endpoint class even when the stored map is empty', async () => {
    settingsApi.list.mockResolvedValue(networkGroup({}));
    renderSettings();

    // The state a fresh or imported instance is in.
    for (const label of [
      'Web app and API',
      'Playlist, guide and HDHomeRun',
      'Streams',
      'Xtream Codes API',
    ]) {
      expect(await screen.findByRole('textbox', { name: label })).toHaveValue('');
    }
    expect(screen.getByRole('textbox', { name: 'Web app and API' })).toHaveAttribute(
      'placeholder',
      'Open to everyone',
    );
    expect(screen.getByText(/locks you out of this page/)).toBeInTheDocument();
  });

  it('shows a stored list in its own row', async () => {
    settingsApi.list.mockResolvedValue(networkGroup({ UI: '10.0.0.0/8' }));
    renderSettings();

    expect(await screen.findByRole('textbox', { name: 'Web app and API' })).toHaveValue(
      '10.0.0.0/8',
    );
    expect(screen.getByRole('textbox', { name: 'Streams' })).toHaveValue('');
  });

  it('sends null for an emptied endpoint and leaves the others intact', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue(
      networkGroup({ UI: '10.0.0.0/8', STREAMS: '192.168.1.0/24' }),
    );
    settingsApi.update.mockResolvedValue({
      key: 'network_access',
      name: 'Network Access',
      value: { STREAMS: '192.168.1.0/24' },
    });
    renderSettings();

    await user.clear(await screen.findByRole('textbox', { name: 'Web app and API' }));

    // Typed into and thought better of. This one was never stored, so there is
    // no key to remove and nothing to say about it.
    const guide = screen.getByRole('textbox', { name: 'Playlist, guide and HDHomeRun' });
    await user.type(guide, '10.0.0.0/8');
    await user.clear(guide);

    await user.click(screen.getByRole('button', { name: 'Save' }));

    // `null` removes the key; `''` would be stored as a restriction with no
    // usable entry, and the other endpoints must survive the round trip.
    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('network_access', {
        UI: null,
        STREAMS: '192.168.1.0/24',
      }),
    );
  });

  it('keeps an endpoint class it has never heard of', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue(networkGroup({ FUTURE: '10.0.0.0/8' }));
    settingsApi.update.mockResolvedValue({
      key: 'network_access',
      name: 'Network Access',
      value: { FUTURE: '10.0.0.0/8', UI: '10.0.0.0/8' },
    });
    renderSettings();

    expect(await screen.findByRole('textbox', { name: 'FUTURE' })).toHaveValue(
      '10.0.0.0/8',
    );

    await user.type(
      screen.getByRole('textbox', { name: 'Web app and API' }),
      '10.0.0.0/8',
    );
    await user.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(settingsApi.update).toHaveBeenCalledWith('network_access', {
        FUTURE: '10.0.0.0/8',
        UI: '10.0.0.0/8',
      }),
    );
  });

  it('shows the entries the server rejected and keeps the draft', async () => {
    const user = userEvent.setup();
    settingsApi.list.mockResolvedValue(networkGroup({}));
    settingsApi.update.mockRejectedValue(
      new ApiError('invalid CIDRs — UI: 10.0.0.0/33', { status: 400 }),
    );
    renderSettings();

    await user.type(
      await screen.findByRole('textbox', { name: 'Web app and API' }),
      '10.0.0.0/33',
    );
    await user.click(screen.getByRole('button', { name: 'Save' }));

    expect(
      await screen.findByText('invalid CIDRs — UI: 10.0.0.0/33'),
    ).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Web app and API' })).toHaveValue(
      '10.0.0.0/33',
    );
  });
});

describe('leaving a section with unsaved changes', () => {
  async function editRingRetention(user) {
    const input = await screen.findByRole('textbox', { name: /Ring retention/ });
    await user.clear(input);
    await user.type(input, '30');
    return input;
  }

  it('asks first, and keeps the draft when the answer is no', async () => {
    const user = userEvent.setup();
    renderSettings();
    await editRingRetention(user);

    await user.click(screen.getByRole('link', { name: 'System' }));

    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByText('Discard unsaved changes?')).toBeVisible();
    await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));

    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(screen.getByRole('region', { name: 'Proxy' })).toBeVisible();
    expect(screen.getByRole('textbox', { name: /Ring retention/ })).toHaveValue('30');
  });

  it('discards the draft and moves on when the answer is yes', async () => {
    const user = userEvent.setup();
    renderSettings();
    await editRingRetention(user);

    await user.click(screen.getByRole('link', { name: 'System' }));
    await user.click(
      within(await screen.findByRole('dialog')).getByRole('button', { name: 'Discard' }),
    );

    expect(await screen.findByRole('region', { name: 'System' })).toBeVisible();
    expect(screen.queryByRole('region', { name: 'Proxy' })).not.toBeInTheDocument();

    // Nothing is dirty any more, so the way back is not asked about, and the
    // draft is gone rather than waiting there.
    await user.click(screen.getByRole('link', { name: 'Proxy' }));
    expect(await screen.findByRole('textbox', { name: /Ring retention/ })).toHaveValue(
      '15',
    );
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('warns the browser before a reload while something is unsaved', async () => {
    const user = userEvent.setup();
    renderSettings();
    await editRingRetention(user);

    const unload = () => {
      const event = new Event('beforeunload', { cancelable: true });
      window.dispatchEvent(event);
      return event.defaultPrevented;
    };
    expect(unload()).toBe(true);

    await user.click(section('Proxy').getByRole('button', { name: 'Revert' }));
    expect(unload()).toBe(false);
  });
});

describe('appearance', () => {
  /** What the rendered MantineProvider actually put on the page. */
  const rootVariable = (name) =>
    getComputedStyle(document.documentElement).getPropertyValue(name);

  const choice = (group, option) =>
    within(section('Appearance').getByRole('radiogroup', { name: group })).getByRole(
      'radio',
      { name: option },
    );

  it('applies a text size the moment it is chosen, with nothing to save', async () => {
    const user = userEvent.setup();
    renderSettings('/settings/appearance');
    await screen.findByRole('region', { name: 'Appearance' });

    expect(choice('Text size', 'Default')).toBeChecked();
    await user.click(choice('Text size', 'Large'));

    expect(choice('Text size', 'Large')).toBeChecked();
    expect(useAppearance.getState().textSize).toBe('large');
    expect(rootVariable('--mantine-scale')).toBe('1.12');
    expect(
      section('Appearance').queryByRole('button', { name: 'Save' }),
    ).not.toBeInTheDocument();
    expect(settingsApi.update).not.toHaveBeenCalled();
  });

  it('brightens the text tones when high contrast is chosen', async () => {
    const user = userEvent.setup();
    renderSettings('/settings/appearance');
    await screen.findByRole('region', { name: 'Appearance' });

    const before = rootVariable('--mantine-color-dark-0');
    await user.click(choice('Contrast', 'High'));

    expect(useAppearance.getState().contrast).toBe('high');
    expect(rootVariable('--mantine-color-dark-0')).not.toBe(before);
    expect(rootVariable('--mantine-color-dark-0')).toBe('#f4f6f9');
  });

  it('says the choice belongs to this browser', async () => {
    renderSettings('/settings/appearance');
    await screen.findByRole('region', { name: 'Appearance' });

    expect(section('Appearance').getByText(/Stored in this browser only/)).toBeVisible();
  });

  it('offers appearance even when the server settings will not load', async () => {
    settingsApi.list.mockRejectedValue(new ApiError('Not found.', { status: 404 }));
    renderSettings();

    await screen.findByText('Not found.');
    expect(
      section('Appearance').getByRole('radiogroup', { name: 'Contrast' }),
    ).toBeVisible();
  });
});
