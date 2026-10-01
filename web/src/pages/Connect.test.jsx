import { beforeEach, describe, expect, it, vi } from 'vitest';
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { Connect } from './Connect.jsx';
import {
  channelProfiles,
  environment,
  hdhr,
  origins,
  outputProfiles,
} from '../api/resources.js';
import { BASE_URL_SOURCES, useBaseUrlStore } from '../components/baseUrl.js';
import { useSession } from '../auth/session.js';
import { renderWithProviders } from '../test-utils.jsx';
import { ApiError } from '../api/errors.js';

vi.mock('../api/resources.js', () => ({
  channelProfiles: { list: vi.fn() },
  environment: { get: vi.fn() },
  hdhr: { discover: vi.fn() },
  origins: { list: vi.fn() },
  outputProfiles: { list: vi.fn() },
  USER_LEVELS: { STREAMER: 0, STANDARD: 1, ADMIN: 10 },
}));

/** jsdom's origin, which is what "This browser" resolves to. */
const ORIGIN = 'http://localhost:3000';

/**
 * One card's own controls. The Plex and M3U cards both offer a channel profile
 * and an output profile, as they should — they are different questions about
 * different clients — so every query has to say which card it means.
 */
function card(title) {
  return within(screen.getByRole('heading', { name: title }).closest('section'));
}

const PLEX = 'Plex — HDHomeRun tuner';
const PLAYERS = 'M3U players — TiviMate, Jellyfin, VLC';

const SEEN = [
  {
    base_url: 'http://tv.lan:9191',
    kinds: ['hdhr', 'm3u'],
    first_seen: '2026-09-14T10:00:00Z',
    last_seen: '2026-09-15T09:30:00Z',
    requests: 12,
  },
];

beforeEach(() => {
  useBaseUrlStore.setState({
    source: BASE_URL_SOURCES.browser,
    custom: '',
    advertised: null,
  });
  useSession.setState({ status: 'authenticated', user: ADMIN });

  environment.get.mockResolvedValue({
    version: '0.3.1',
    advertised_base_url: null,
    artwork_base_url: null,
  });
  channelProfiles.list.mockResolvedValue([
    { id: 1, name: 'All' },
    { id: 2, name: 'Living Room' },
    { id: 3, name: 'Kids (2)' },
  ]);
  outputProfiles.list.mockResolvedValue([{ id: 5, name: 'Transcode 720p' }]);
  origins.list.mockResolvedValue([]);
  hdhr.discover.mockResolvedValue({
    FriendlyName: 'Dollet HDHomeRun',
    DeviceID: '12345678',
    TunerCount: 2,
  });
});

const ADMIN = {
  id: 1,
  username: 'synthadmin',
  user_level: 10,
  custom_properties: { xc_password: 'synth-xc-admin' },
};

describe('which address', () => {
  it('builds every URL from this browser by default', async () => {
    renderWithProviders(<Connect />);

    expect(await screen.findByLabelText('Tuner address')).toHaveValue(`${ORIGIN}/hdhr/`);
    expect(screen.getByLabelText('Guide URL')).toHaveValue(`${ORIGIN}/output/epg`);
    expect(screen.getByLabelText('Playlist URL')).toHaveValue(`${ORIGIN}/output/m3u`);
  });

  it('says the advertised base is unset, which is the common case', async () => {
    renderWithProviders(<Connect />);

    expect(
      await screen.findByText(/Right now it is not set/, { exact: false }),
    ).toBeInTheDocument();
  });

  it('names the advertised base when the deployment has one', async () => {
    environment.get.mockResolvedValue({
      version: '0.3.1',
      advertised_base_url: 'https://tv.example',
      artwork_base_url: null,
    });
    renderWithProviders(<Connect />);

    expect(
      await screen.findByText(/Right now it is set to/, { exact: false }),
    ).toBeInTheDocument();
    // And the picker grows its third source from the same answer.
    expect(await screen.findByRole('radio', { name: 'Advertised' })).toBeInTheDocument();
  });

  it('says so when no client has been anywhere yet', async () => {
    renderWithProviders(<Connect />);

    expect(
      await screen.findByText('No client has fetched a lineup or playlist yet.'),
    ).toBeInTheDocument();
  });

  it('lists the addresses clients have used, with what they asked for', async () => {
    origins.list.mockResolvedValue(SEEN);
    renderWithProviders(<Connect />);

    expect(await screen.findByText('http://tv.lan:9191')).toBeInTheDocument();
    expect(screen.getByText('hdhr')).toBeInTheDocument();
    expect(screen.getByText('m3u')).toBeInTheDocument();
    expect(screen.getByText('12 requests')).toBeInTheDocument();
  });

  it('counts a single request in the singular and prints a timestamp verbatim when it will not parse', async () => {
    origins.list.mockResolvedValue([{ ...SEEN[0], requests: 1, last_seen: 'whenever' }]);
    renderWithProviders(<Connect />);

    expect(await screen.findByText('1 request')).toBeInTheDocument();
    // A date the browser cannot read is shown as it arrived rather than as
    // "Invalid Date", which says nothing about what the server sent.
    expect(screen.getByText('whenever')).toBeInTheDocument();
  });

  it('rebuilds every URL from a seen address when that one is chosen', async () => {
    const user = userEvent.setup();
    origins.list.mockResolvedValue(SEEN);
    renderWithProviders(<Connect />);

    await user.click(await screen.findByRole('button', { name: 'Use this' }));

    expect(screen.getByLabelText('Tuner address')).toHaveValue(
      'http://tv.lan:9191/hdhr/',
    );
    // As a custom base, so it survives a reload and the picker shows what
    // happened rather than silently rewriting the URLs.
    expect(useBaseUrlStore.getState().source).toBe(BASE_URL_SOURCES.custom);
    expect(screen.getByRole('radio', { name: 'Custom' })).toBeChecked();
  });

  it('surfaces a failure to list them rather than showing an empty list', async () => {
    origins.list.mockRejectedValue(new ApiError('Forbidden.', { status: 403 }));
    renderWithProviders(<Connect />);

    expect(await screen.findByRole('alert')).toHaveTextContent('Forbidden.');
  });
});

describe('the Plex card', () => {
  it('scopes the tuner and the guide to a channel profile', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Connect />);
    await screen.findByLabelText('Tuner address');

    await user.click(card(PLEX).getByRole('textbox', { name: 'Channel profile' }));
    await user.click(
      await card(PLEX).findByRole('option', { name: 'Kids (2)', hidden: true }),
    );

    expect(screen.getByLabelText('Tuner address')).toHaveValue(
      `${ORIGIN}/hdhr/Kids%20%282%29/`,
    );
    expect(screen.getByLabelText('Guide URL')).toHaveValue(
      `${ORIGIN}/output/epg/Kids%20%282%29`,
    );
  });

  it('adds an output profile to the tuner prefix but not to the guide', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Connect />);
    await screen.findByLabelText('Tuner address');

    await user.click(card(PLEX).getByRole('textbox', { name: 'Output profile' }));
    await user.click(
      await card(PLEX).findByRole('option', { name: 'Transcode 720p', hidden: true }),
    );

    expect(screen.getByLabelText('Tuner address')).toHaveValue(
      `${ORIGIN}/hdhr/output_profile/5/`,
    );
    // The guide is the same guide whichever way the video is encoded.
    expect(screen.getByLabelText('Guide URL')).toHaveValue(`${ORIGIN}/output/epg`);
  });

  it('says why guide artwork is a different address from the tuner', async () => {
    renderWithProviders(<Connect />);

    expect(
      await card(PLEX).findByText(/straight to the browser rendering the guide/),
    ).toBeInTheDocument();
    expect(card(PLEX).getByText('DOLLET_ARTWORK_BASE_URL')).toBeInTheDocument();
  });

  it('builds artwork from the picked base until the deployment splits them', async () => {
    renderWithProviders(<Connect />);

    // Nothing has split them, so artwork is wherever everything else is.
    expect(await screen.findByLabelText('Guide artwork base')).toHaveValue(ORIGIN);
  });

  it('shows the configured artwork base instead, because that is what clients get', async () => {
    environment.get.mockResolvedValue({
      version: '0.3.1',
      advertised_base_url: null,
      artwork_base_url: 'https://tv.example.com/',
    });
    renderWithProviders(<Connect />);

    // The trailing slash goes, the same way the server drops it before building
    // `…/api/channels/logos/<id>/cache/`.
    expect(await screen.findByLabelText('Guide artwork base')).toHaveValue(
      'https://tv.example.com',
    );
    // And it wins over the picked base rather than being an option beside it:
    // this is the URL every client is already being handed.
    expect(screen.getByLabelText('Tuner address')).toHaveValue(`${ORIGIN}/hdhr/`);
    expect(
      screen.getByText(/so every logo and guide icon is built from that one instead/),
    ).toBeInTheDocument();
  });

  it('says the identity could not be read rather than showing nothing', async () => {
    hdhr.discover.mockRejectedValue(new ApiError('Forbidden.', { status: 403 }));
    renderWithProviders(<Connect />);

    expect(
      await screen.findByText(/Could not read this tuner's identity/),
    ).toBeInTheDocument();
  });

  it('asks the server for the tuner identity rather than reimplementing it', async () => {
    renderWithProviders(<Connect />);

    expect(await screen.findByText('Dollet HDHomeRun')).toBeInTheDocument();
    expect(screen.getByText('12345678')).toBeInTheDocument();
    expect(hdhr.discover).toHaveBeenCalledWith({
      channelProfile: undefined,
      outputProfile: undefined,
    });
  });
});

describe('the M3U card', () => {
  it('turns each toggle into the query parameter the server reads', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Connect />);
    await screen.findByLabelText('Playlist URL');

    // Mantine folds a switch's description into its accessible name, so these
    // match on the label alone.
    await user.click(screen.getByRole('switch', { name: /^Direct streams/ }));
    await user.click(screen.getByRole('switch', { name: /^Cached logos/ }));

    expect(screen.getByLabelText('Playlist URL')).toHaveValue(
      `${ORIGIN}/output/m3u?direct=true&cachedlogos=false`,
    );
  });

  it('carries the guide id source into both the playlist and its guide', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Connect />);
    await screen.findByLabelText('Playlist URL');

    await user.click(card(PLAYERS).getByRole('textbox', { name: /^Guide id source/ }));
    await user.click(
      await card(PLAYERS).findByRole('option', {
        name: 'Gracenote station id',
        hidden: true,
      }),
    );

    expect(screen.getByLabelText('Playlist URL')).toHaveValue(
      `${ORIGIN}/output/m3u?tvg_id_source=gracenote`,
    );
    expect(screen.getByLabelText('Player guide URL')).toHaveValue(
      `${ORIGIN}/output/epg?tvg_id_source=gracenote`,
    );
  });
});

describe('the M3U card, scoped', () => {
  it('puts a channel profile in the path and an output profile in the query', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Connect />);
    await screen.findByLabelText('Playlist URL');

    await user.click(card(PLAYERS).getByRole('textbox', { name: 'Channel profile' }));
    await user.click(
      await card(PLAYERS).findByRole('option', { name: 'Living Room', hidden: true }),
    );
    await user.click(card(PLAYERS).getByRole('textbox', { name: 'Output profile' }));
    await user.click(
      await card(PLAYERS).findByRole('option', {
        name: 'Transcode 720p',
        hidden: true,
      }),
    );

    expect(screen.getByLabelText('Playlist URL')).toHaveValue(
      `${ORIGIN}/output/m3u/Living%20Room?output_profile=5`,
    );
    // The guide follows the profile but never the output profile: the listings
    // are the same whichever way the video is encoded.
    expect(screen.getByLabelText('Player guide URL')).toHaveValue(
      `${ORIGIN}/output/epg/Living%20Room`,
    );
  });
});

describe('the Xtream card', () => {
  it('splits the base and masks the password until it is revealed', async () => {
    const user = userEvent.setup();
    renderWithProviders(<Connect />);

    const password = await screen.findByLabelText('Password');
    expect(screen.getByLabelText('Server URL')).toHaveValue('http://localhost');
    expect(screen.getByLabelText('Port')).toHaveValue('3000');
    expect(screen.getByLabelText('Username')).toHaveValue('synthadmin');

    // Masked by the input's own type, so "copy" still copies the secret.
    expect(password).toHaveAttribute('type', 'password');
    expect(password).toHaveValue('synth-xc-admin');

    await user.click(screen.getByRole('button', { name: 'Reveal Password' }));
    expect(screen.getByLabelText('Password')).toHaveAttribute('type', 'text');
  });

  it('says the account has no Xtream password rather than offering a blank one', async () => {
    useSession.setState({
      status: 'authenticated',
      user: { ...ADMIN, custom_properties: {} },
    });
    renderWithProviders(<Connect />);

    expect(
      await screen.findByText(/This account has no Xtream password/),
    ).toBeInTheDocument();
    expect(screen.queryByLabelText('Password')).not.toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Users' })).toHaveAttribute('href', '/users');
  });
});

describe('copying', () => {
  it('copies the URL the field shows', async () => {
    // Read back through `user-event`'s own clipboard stub rather than a mock:
    // Mantine's right section ignores pointer events by default, and a mock
    // that is never called cannot tell that apart from a button that copied
    // the wrong string.
    const user = userEvent.setup();
    renderWithProviders(<Connect />);
    await screen.findByLabelText('Tuner address');

    await user.click(screen.getByRole('button', { name: 'Copy Tuner address' }));

    await waitFor(async () =>
      expect(await navigator.clipboard.readText()).toBe(`${ORIGIN}/hdhr/`),
    );
  });
});
