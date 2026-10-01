import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import {
  Alert,
  Anchor,
  Badge,
  Button,
  Code,
  Group,
  Select,
  Stack,
  Switch,
  Text,
} from '@mantine/core';
import { Info, TriangleAlert } from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { BaseUrlPicker, ADVERTISED_ENV } from '../components/BaseUrlPicker.jsx';
import { CopyField } from '../components/CopyField.jsx';
import { normalizeBaseUrl, useBaseUrl, useBaseUrlStore } from '../components/baseUrl.js';
import { useResource } from '../api/useResource.js';
import { useSession } from '../auth/session.js';
import {
  channelProfiles as channelProfilesApi,
  environment as environmentApi,
  hdhr as hdhrApi,
  origins as originsApi,
  outputProfiles as outputProfilesApi,
} from '../api/resources.js';
import {
  TVG_ID_SOURCES,
  DEFAULT_TVG_ID_SOURCE,
  guideUrl,
  hdhrUrl,
  playlistUrl,
  xtreamServer,
} from './connectUrls.js';
import classes from './Connect.module.css';
import { absoluteTime } from '../format.js';

/**
 * The variable that splits artwork off from every other URL.
 *
 * Not offered by the picker, unlike `ADVERTISED_ENV`: it is not a base the
 * operator chooses between here, it is the one clients are already being handed
 * for logos whatever this page shows.
 */
const ARTWORK_ENV = 'DOLLET_ARTWORK_BASE_URL';

/**
 * Where an operator finds the URLs their clients need.
 *
 * The README cannot say what *this* deployment advertises. Admin-only, because it
 * shows `DOLLET_ADVERTISED_BASE_URL` and the addresses clients have reached
 * this server on — deployment facts rather than anyone's own data.
 */
export function Connect() {
  const base = useBaseUrl();
  const setAdvertised = useBaseUrlStore((state) => state.setAdvertised);

  const loadEnvironment = useCallback(() => environmentApi.get(), []);
  const { data: environment } = useResource(loadEnvironment, null);

  const loadProfiles = useCallback(() => channelProfilesApi.list(), []);
  const { data: profiles } = useResource(loadProfiles, []);

  const loadOutputProfiles = useCallback(() => outputProfilesApi.list(), []);
  const { data: outputs } = useResource(loadOutputProfiles, []);

  const advertised = environment?.advertised_base_url ?? null;
  useEffect(() => {
    setAdvertised(advertised);
  }, [advertised, setAdvertised]);

  // Not a choice, so not in the picker: when the deployment sets it, this *is*
  // the base every client's logo URLs already carry, and showing them built
  // from the picked base instead would be showing a URL nobody receives.
  const artworkOverride = environment?.artwork_base_url ?? null;
  const artworkBase = normalizeBaseUrl(artworkOverride) ?? base;

  const profileOptions = useMemo(
    () => [
      { value: '', label: 'All channels' },
      ...profiles
        .filter((profile) => profile.name !== 'All')
        .map((profile) => ({ value: profile.name, label: profile.name })),
    ],
    [profiles],
  );

  const outputOptions = useMemo(
    () => [
      { value: '', label: 'No transcoding' },
      ...outputs.map((output) => ({
        value: String(output.id),
        label: output.name,
      })),
    ],
    [outputs],
  );

  return (
    <Page title="Connect" subtitle="The addresses Plex and your players need">
      <Stack gap="md">
        <WhichAddress advertised={advertised} artworkOverride={artworkOverride} />
        <PlexCard
          base={base}
          artworkBase={artworkBase}
          artworkOverride={artworkOverride}
          profileOptions={profileOptions}
          outputOptions={outputOptions}
        />
        <PlayersCard
          base={base}
          profileOptions={profileOptions}
          outputOptions={outputOptions}
        />
        <XtreamCard base={base} />
        <ProxyCard />
      </Stack>
    </Page>
  );
}

/**
 * Keeps a dropdown inside the card that owns it rather than at the end of the
 * document. The Plex and M3U cards ask the same two questions of different
 * clients, so their pickers share labels; without this the open list belongs to
 * neither card as far as anything walking the page is concerned.
 */
const DROPDOWN_IN_CARD = { withinPortal: false };

function Card({ title, children }) {
  return (
    <section className={classes.card}>
      <h2 className={classes.cardTitle}>{title}</h2>
      <Stack gap="sm">{children}</Stack>
    </section>
  );
}

/**
 * The load-bearing paragraph, and the evidence under it.
 *
 * Everything else on this page is a URL an operator could have written down.
 * This is the part they cannot know without being told, and the part that fails
 * as "discovery works, playback doesn't" when it is wrong.
 */
function WhichAddress({ advertised, artworkOverride }) {
  const load = useCallback(() => originsApi.list(), []);
  const { data: seen, error } = useResource(load, []);
  const chooseBase = useBaseUrlStore((state) => state.chooseBase);

  return (
    <Card title="Which address?">
      <div className={classes.picker}>
        <BaseUrlPicker />
      </div>

      <Text size="sm">
        The URLs <em>inside</em> the HDHomeRun lineup and the M3U playlist are built from
        the address the client used to fetch them — so a tuner added as{' '}
        <Code>localhost</Code> hands Plex a lineup full of channels only this machine can
        play. Unless <Code>{ADVERTISED_ENV}</Code> is set, in which case every client is
        given that base no matter how it arrived.
      </Text>

      <Text size="sm" c={advertised ? undefined : 'dimmed'}>
        {advertised ? (
          <>
            Right now it is set to <Code>{advertised}</Code>, so that is what every client
            gets.
          </>
        ) : (
          <>
            Right now it is not set, so each client gets URLs built from the address it
            reached this server on.
          </>
        )}
      </Text>

      {artworkOverride && (
        <Text size="sm">
          Artwork is the exception. <Code>{ARTWORK_ENV}</Code> is set to{' '}
          <Code>{artworkOverride}</Code>, so every logo and guide icon is built from that
          one instead — those are fetched by a browser rather than by the server that read
          the document they came in.
        </Text>
      )}

      <Stack gap={6}>
        <Group gap={6}>
          <Text size="sm" fw={500}>
            Seen from
          </Text>
          <Text size="xs" c="dimmed">
            addresses a client has actually fetched a lineup, playlist or guide on
          </Text>
        </Group>

        {error && (
          <Alert color="red" variant="light" icon={<TriangleAlert size={16} />}>
            {error.message}
          </Alert>
        )}

        {!error && seen.length === 0 && (
          <Text size="xs" c="dimmed">
            No client has fetched a lineup or playlist yet.
          </Text>
        )}

        {seen.map((origin) => (
          <div key={origin.base_url} className={classes.seenRow}>
            <Code className={classes.seenUrl}>{origin.base_url}</Code>
            <Group gap={4} wrap="nowrap">
              {origin.kinds.map((kind) => (
                <Badge key={kind} size="xs" variant="light" color="gray">
                  {kind}
                </Badge>
              ))}
            </Group>
            <Text size="xs" c="dimmed">
              {absoluteTime(origin.last_seen)}
            </Text>
            <Text size="xs" c="dimmed">
              {origin.requests} {origin.requests === 1 ? 'request' : 'requests'}
            </Text>
            <Button
              size="compact-xs"
              variant="default"
              onClick={() => chooseBase(origin.base_url)}
            >
              Use this
            </Button>
          </div>
        ))}
      </Stack>
    </Card>
  );
}

function PlexCard({ base, artworkBase, artworkOverride, profileOptions, outputOptions }) {
  const [channelProfile, setChannelProfile] = useState('');
  const [outputProfile, setOutputProfile] = useState('');

  const scope = useMemo(
    () => ({
      channelProfile: channelProfile || undefined,
      outputProfile: outputProfile ? Number(outputProfile) : undefined,
    }),
    [channelProfile, outputProfile],
  );

  return (
    <Card title="Plex — HDHomeRun tuner">
      <Text size="sm">
        Only the Plex <em>server</em> talks to this address. Clients watching Live TV get
        the video from Plex and never need to reach this one.
      </Text>
      <Text size="sm">
        Each combination below is a distinct tuner to Plex — a different{' '}
        <Code>DeviceID</Code> — so adding a second one does not replace the first.
      </Text>

      <Group grow align="flex-start">
        <Select
          label="Channel profile"
          data={profileOptions}
          value={channelProfile}
          onChange={setChannelProfile}
          allowDeselect={false}
          comboboxProps={DROPDOWN_IN_CARD}
        />
        <Select
          label="Output profile"
          description="Transcodes every stream this tuner serves."
          data={outputOptions}
          value={outputProfile}
          onChange={setOutputProfile}
          allowDeselect={false}
          comboboxProps={DROPDOWN_IN_CARD}
        />
      </Group>

      <CopyField
        label="Tuner address"
        description="Add this in Plex under Live TV & DVR as an HDHomeRun tuner."
        value={hdhrUrl(base, scope)}
      />
      <TunerIdentity scope={scope} />

      <CopyField
        label="Guide URL"
        description="XMLTV. Plex asks for this after it finds the tuner."
        value={guideUrl(base, { channelProfile: scope.channelProfile })}
      />

      <Text size="sm">
        Guide artwork is the one exception to all of that. Plex hands the{' '}
        <Code>&lt;icon&gt;</Code> URLs out of that XMLTV straight to the browser rendering
        the guide, unproxied — so they have to be reachable from the browser, not from the
        Plex server. If those are two different addresses, set <Code>{ARTWORK_ENV}</Code>{' '}
        and the logos move without the streams following them.
      </Text>

      <CopyField
        label="Guide artwork base"
        description={
          artworkOverride
            ? `From ${ARTWORK_ENV}: every logo URL in the guide and the playlist is built from this, whichever base is picked above.`
            : 'Every logo URL in the guide and the playlist is built from this — the same base as the URLs above, because nothing has split them.'
        }
        value={artworkBase}
      />
    </Card>
  );
}

/**
 * The tuner's own `FriendlyName` and `DeviceID`, asked of the server.
 *
 * Fetched from this browser's origin, not from the chosen base: the identity
 * depends only on the profiles in the path, and asking the server is what keeps
 * its slug rules from being reimplemented here and drifting.
 */
function TunerIdentity({ scope }) {
  const load = useCallback(() => hdhrApi.discover(scope), [scope]);
  const { data: tuner, error } = useResource(load, null);

  if (error) {
    return (
      <Text size="xs" c="dimmed">
        Could not read this tuner&apos;s identity: {error.message}
      </Text>
    );
  }

  if (!tuner) return null;

  return (
    <Group gap={6}>
      <Info size={13} color="var(--mantine-color-dark-2)" />
      <Text size="xs" c="dimmed">
        Plex will show this as <strong>{tuner.FriendlyName}</strong>, device id{' '}
        <Code fz={11}>{tuner.DeviceID}</Code>.
      </Text>
    </Group>
  );
}

function PlayersCard({ base, profileOptions, outputOptions }) {
  const [channelProfile, setChannelProfile] = useState('');
  const [outputProfile, setOutputProfile] = useState('');
  const [direct, setDirect] = useState(false);
  const [cachedLogos, setCachedLogos] = useState(true);
  const [tvgIdSource, setTvgIdSource] = useState(DEFAULT_TVG_ID_SOURCE);

  const options = {
    channelProfile: channelProfile || undefined,
    outputProfile: outputProfile ? Number(outputProfile) : undefined,
    direct,
    cachedLogos,
    tvgIdSource,
  };

  return (
    <Card title="M3U players — TiviMate, Jellyfin, VLC">
      <Text size="sm">
        Unlike Plex, these fetch the streams themselves, so this address has to be
        reachable from the device running the player.
      </Text>

      <Group grow align="flex-start">
        <Select
          label="Channel profile"
          data={profileOptions}
          value={channelProfile}
          onChange={setChannelProfile}
          allowDeselect={false}
          comboboxProps={DROPDOWN_IN_CARD}
        />
        <Select
          label="Output profile"
          data={outputOptions}
          value={outputProfile}
          onChange={setOutputProfile}
          allowDeselect={false}
          comboboxProps={DROPDOWN_IN_CARD}
        />
        <Select
          label="Guide id source"
          description="Which field becomes tvg-id."
          data={TVG_ID_SOURCES}
          value={tvgIdSource}
          onChange={setTvgIdSource}
          allowDeselect={false}
          comboboxProps={DROPDOWN_IN_CARD}
        />
      </Group>

      <Group gap="lg">
        <Switch
          label="Direct streams"
          description="Hand out the provider URL instead of proxying."
          checked={direct}
          onChange={(event) => setDirect(event.currentTarget.checked)}
        />
        <Switch
          label="Cached logos"
          description="Serve artwork from here rather than the provider's CDN."
          checked={cachedLogos}
          onChange={(event) => setCachedLogos(event.currentTarget.checked)}
        />
      </Group>

      <CopyField label="Playlist URL" value={playlistUrl(base, options)} />
      <CopyField
        label="Player guide URL"
        description="The same guide, with the same id source as the playlist."
        value={guideUrl(base, {
          channelProfile: options.channelProfile,
          tvgIdSource,
        })}
      />
    </Card>
  );
}

function XtreamCard({ base }) {
  const user = useSession((state) => state.user);
  // `base` is always a normalised absolute URL — the picker resolves anything
  // else to this browser's origin — so the split cannot fail here.
  const { server, port } = xtreamServer(base);
  const password = user?.custom_properties?.xc_password ?? null;

  return (
    <Card title="Xtream Codes players">
      <Text size="sm">
        These credentials travel in the URL, which is why the Xtream password is a
        separate secret rather than your login password.
      </Text>

      <Group grow align="flex-start">
        <CopyField label="Server URL" value={server} />
        <CopyField label="Port" value={port} />
      </Group>

      <CopyField label="Username" value={user?.username ?? ''} />

      {password ? (
        <CopyField label="Password" value={password} secret />
      ) : (
        <Text size="sm" c="dimmed">
          This account has no Xtream password, so no Xtream player can sign in as it. Set
          one on the{' '}
          <Anchor component={Link} to="/users">
            Users
          </Anchor>{' '}
          page.
        </Text>
      )}
    </Card>
  );
}

function ProxyCard() {
  return (
    <Card title="Behind a reverse proxy">
      <Text size="sm">
        List the proxy&apos;s own address or network in{' '}
        <Code>DOLLET_TRUSTED_PROXIES</Code>, or its <Code>X-Forwarded-Proto</Code> and{' '}
        <Code>X-Forwarded-Host</Code> are ignored and every URL above is built from
        whatever the proxy put in <Code>Host</Code>. If the proxy cannot send those
        headers, set <Code>{ADVERTISED_ENV}</Code> instead and every client is handed that
        base regardless.
      </Text>
    </Card>
  );
}
