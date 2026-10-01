import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Alert,
  Badge,
  Button,
  Group,
  Loader,
  Select,
  Text,
  TextInput,
  Tooltip,
  UnstyledButton,
} from '@mantine/core';
import { notifyDone, notifyError, notifyQuiet } from '../notify.js';
import {
  CalendarDays,
  ChevronLeft,
  ChevronRight,
  Check,
  Search,
  TriangleAlert,
  X,
} from 'lucide-react';

import { Page } from '../layout/AppLayout.jsx';
import { LogoImage } from '../components/LogoImage.jsx';
import { useResource } from '../api/useResource.js';
import { channelProfiles as profilesApi, guide as guideApi } from '../api/resources.js';
import {
  ROW_HEIGHT,
  hourMarks,
  isOnAir,
  nowOffset,
  placeProgram,
  toPx,
  visibleRows,
} from './guideLayout.js';
import classes from './Guide.module.css';

const CHANNEL_WIDTH = 210;
const WINDOW_HOURS = 24;
/** How far the arrows move the window. Matches what fits on a wide screen. */
const STEP_HOURS = 3;

/** Rounded down to the hour so the axis labels land on whole hours. */
function startOfHour(date) {
  const copy = new Date(date);
  copy.setMinutes(0, 0, 0);
  return copy;
}

const formatTime = (value) =>
  new Date(value).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });

export function Guide() {
  // The window opens an hour back, so whatever is on now is already on screen
  // with its beginning visible rather than clipped at the left edge.
  const [windowStart, setWindowStart] = useState(() =>
    startOfHour(new Date(Date.now() - 3_600_000)),
  );
  const [profile, setProfile] = useState(null);
  const [search, setSearch] = useState('');
  const [now, setNow] = useState(() => Date.now());
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportHeight, setViewportHeight] = useState(600);
  const scroller = useRef(null);

  const windowEnd = useMemo(
    () => windowStart.getTime() + WINDOW_HOURS * 3_600_000,
    [windowStart],
  );

  const load = useCallback(
    () =>
      guideApi.grid({
        from: windowStart,
        to: new Date(windowEnd),
        channelProfile: profile ? Number(profile) : undefined,
      }),
    [windowStart, windowEnd, profile],
  );
  const { data, loading, error } = useResource(load, { channels: [] });

  const loadProfiles = useCallback(() => profilesApi.list(), []);
  const { data: profiles } = useResource(loadProfiles, []);

  // The now-line would otherwise be wherever it was when the page loaded.
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(timer);
  }, []);

  const channels = useMemo(() => {
    const needle = search.trim().toLowerCase();
    if (!needle) return data.channels;
    return data.channels.filter((channel) => channel.name.toLowerCase().includes(needle));
  }, [data.channels, search]);

  const { start, end, offsetY, totalHeight } = visibleRows(
    scrollTop,
    viewportHeight,
    channels.length,
  );

  const laneWidth = toPx(windowEnd - windowStart.getTime());
  const marks = useMemo(
    () => hourMarks(windowStart.getTime(), windowEnd),
    [windowStart, windowEnd],
  );
  const nowLeft = nowOffset(now, windowStart.getTime(), windowEnd);

  const move = (hours) =>
    setWindowStart((current) => new Date(current.getTime() + hours * 3_600_000));

  const jumpToNow = () => {
    setWindowStart(startOfHour(new Date(Date.now() - 3_600_000)));
    scroller.current?.scrollTo?.({ left: 0 });
  };

  const withoutGuide = data.channels.filter(
    (channel) => channel.programs.length === 0,
  ).length;
  const suggested = data.channels.filter((channel) => channel.epg_suggestion).length;

  const profileOptions = useMemo(
    () => profiles.map((entry) => ({ value: String(entry.id), label: entry.name })),
    [profiles],
  );

  return (
    <Page
      title="TV Guide"
      actions={
        <Group gap={6}>
          <Text size="xs" c="dimmed">
            {windowStart.toLocaleDateString([], {
              weekday: 'long',
              month: 'short',
              day: 'numeric',
            })}
          </Text>
          <Tooltip label={`Back ${STEP_HOURS} hours`}>
            <Button
              size="xs"
              variant="default"
              aria-label="Earlier"
              onClick={() => move(-STEP_HOURS)}
            >
              <ChevronLeft size={14} />
            </Button>
          </Tooltip>
          <Button size="xs" variant="default" onClick={jumpToNow}>
            Now
          </Button>
          <Tooltip label={`Forward ${STEP_HOURS} hours`}>
            <Button
              size="xs"
              variant="default"
              aria-label="Later"
              onClick={() => move(STEP_HOURS)}
            >
              <ChevronRight size={14} />
            </Button>
          </Tooltip>
        </Group>
      }
    >
      <div className={classes.toolbar}>
        <TextInput
          size="xs"
          value={search}
          onChange={(event) => setSearch(event.currentTarget.value)}
          placeholder="Search channels"
          aria-label="Search channels"
          leftSection={<Search size={13} />}
          w={200}
        />
        <Select
          size="xs"
          data={profileOptions}
          value={profile}
          onChange={setProfile}
          placeholder="All profiles"
          aria-label="Filter by channel profile"
          clearable
          w={170}
          comboboxProps={{ withinPortal: true }}
        />
        {loading && <Loader size={14} color="gray" />}
        <span className={classes.count}>
          {channels.length} {channels.length === 1 ? 'channel' : 'channels'}
          {withoutGuide > 0 && ` · ${withoutGuide} without guide data`}
        </span>
      </div>

      {error && (
        <Alert color="red" variant="light" mb="sm" icon={<TriangleAlert size={16} />}>
          {error.message}
        </Alert>
      )}

      {suggested > 0 && (
        <Alert color="yellow" variant="light" mb="sm" p="xs" fz="xs">
          {suggested} {suggested === 1 ? 'channel has' : 'channels have'} a likely guide
          match the matcher would not decide on its own. Each one is offered on its row
          below.
        </Alert>
      )}

      {!loading && data.channels.length === 0 && !error && (
        <div className={classes.empty}>
          <CalendarDays size={28} strokeWidth={1.5} color="var(--mantine-color-dark-3)" />
          <Text fw={600} c="gray.3">
            No channels to show
          </Text>
          <Text size="sm" c="dimmed" maw={420}>
            The guide lists the channels in your lineup. Add some on the Channels page,
            then attach an EPG source to give them programme data.
          </Text>
        </div>
      )}

      {data.channels.length > 0 && (
        <div
          className={classes.grid}
          ref={scroller}
          onScroll={(event) => {
            setScrollTop(event.currentTarget.scrollTop);
            setViewportHeight(event.currentTarget.clientHeight);
          }}
        >
          <div className={classes.axis} style={{ width: CHANNEL_WIDTH + laneWidth }}>
            <div className={classes.axisCorner} style={{ width: CHANNEL_WIDTH }} />
            <div className={classes.axisLane} style={{ width: laneWidth }}>
              {marks.map((mark) => (
                <div
                  key={mark.time}
                  className={classes.hourMark}
                  style={{ left: mark.left }}
                >
                  {formatTime(mark.time)}
                </div>
              ))}
            </div>
          </div>

          <div
            className={classes.rows}
            style={{ height: totalHeight, width: CHANNEL_WIDTH + laneWidth }}
          >
            {nowLeft !== null && (
              <div
                className={classes.nowLine}
                style={{ left: CHANNEL_WIDTH + nowLeft }}
                aria-hidden
              />
            )}

            {channels.slice(start, end).map((channel, index) => (
              <GuideRow
                key={channel.id}
                channel={channel}
                top={offsetY + index * ROW_HEIGHT}
                laneWidth={laneWidth}
                windowStart={windowStart.getTime()}
                windowEnd={windowEnd}
                now={now}
              />
            ))}
          </div>
        </div>
      )}
    </Page>
  );
}

function GuideRow({ channel, top, laneWidth, windowStart, windowEnd, now }) {
  const [suggestion, setSuggestion] = useState(channel.epg_suggestion ?? null);
  const [resolving, setResolving] = useState(false);

  const placed = useMemo(
    () =>
      channel.programs
        .map((program) => ({
          program,
          box: placeProgram(program, windowStart, windowEnd),
        }))
        .filter((entry) => entry.box !== null),
    [channel.programs, windowStart, windowEnd],
  );

  const resolve = async (accept) => {
    setResolving(true);
    try {
      if (accept) await guideApi.acceptSuggestion(channel.id);
      else await guideApi.dismissSuggestion(channel.id);
      setSuggestion(null);
      (accept ? notifyDone : notifyQuiet)(
        accept
          ? `${channel.name} now uses ${suggestion.name ?? 'the suggested guide'}. It fills in at the next refresh.`
          : `Suggestion dismissed for ${channel.name}`,
      );
    } catch (failure) {
      notifyError('Could not apply the suggestion', failure);
    } finally {
      setResolving(false);
    }
  };

  return (
    <div
      className={classes.row}
      style={{ top, height: ROW_HEIGHT, width: CHANNEL_WIDTH + laneWidth }}
    >
      <div className={classes.channel} style={{ width: CHANNEL_WIDTH }}>
        <LogoImage src={channel.logo_url} alt="" size={26} />
        <div className={classes.channelText}>
          <span className={classes.channelName}>{channel.name}</span>
          {channel.channel_number !== null && channel.channel_number !== undefined && (
            <span className={classes.channelNumber}>{channel.channel_number}</span>
          )}
        </div>
      </div>

      <div className={classes.lane} style={{ width: laneWidth }}>
        {placed.length === 0 ? (
          // The row exists precisely so this can be said. A missing row would
          // read as the channel itself being gone.
          <div className={classes.noGuide}>
            {suggestion ? (
              <span className={classes.suggestion}>
                <Badge size="xs" variant="light" color="yellow">
                  likely {suggestion.name ?? `guide ${suggestion.epg_data_id}`}
                </Badge>
                <Text component="span" size="xs" c="dimmed">
                  {Math.round(suggestion.score)}% match — not confident enough to apply on
                  its own.
                </Text>
                <Tooltip label="Use this guide for the channel">
                  <UnstyledButton
                    aria-label={`Use ${suggestion.name ?? 'the suggested guide'} for ${channel.name}`}
                    disabled={resolving}
                    onClick={() => resolve(true)}
                    style={{ display: 'flex', color: 'var(--mantine-color-accent-5)' }}
                  >
                    <Check size={15} />
                  </UnstyledButton>
                </Tooltip>
                <Tooltip label="Dismiss the suggestion">
                  <UnstyledButton
                    aria-label={`Dismiss the guide suggestion for ${channel.name}`}
                    disabled={resolving}
                    onClick={() => resolve(false)}
                    style={{ display: 'flex', color: 'var(--mantine-color-dark-2)' }}
                  >
                    <X size={15} />
                  </UnstyledButton>
                </Tooltip>
              </span>
            ) : (
              <span>
                No guide data for this channel
                {channel.epg_data_id === null && ' — no EPG source is matched to it'}
              </span>
            )}
          </div>
        ) : (
          placed.map(({ program, box }) => (
            <Tooltip
              key={program.id}
              label={program.description || program.title}
              multiline
              w={320}
              openDelay={500}
            >
              {/* A div, not a button: nothing happens when a programme is
                  activated, so a button would be a focus stop a keyboard user
                  has to tab through and a control a screen reader announces.
                  The tooltip works on any element. */}
              <div
                className={classes.program}
                // A stable hook for the tests, which otherwise have to match a
                // hashed CSS-module class and would find the inner title span
                // first — `programTitle` is also a class starting `program`.
                data-program={program.id}
                style={{ left: box.left, width: box.width }}
                data-on-air={isOnAir(program, now) || undefined}
                data-clipped-start={box.clippedStart || undefined}
                data-clipped-end={box.clippedEnd || undefined}
              >
                <span className={classes.programTitle}>{program.title}</span>
                <span className={classes.programTime}>
                  {formatTime(program.start_time)} – {formatTime(program.end_time)}
                  {program.is_new && ' · New'}
                </span>
                {program.description && (
                  <span className={classes.programDescription}>
                    {program.description}
                  </span>
                )}
              </div>
            </Tooltip>
          ))
        )}
      </div>
    </div>
  );
}
