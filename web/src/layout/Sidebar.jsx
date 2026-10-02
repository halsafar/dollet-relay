import { useEffect, useState } from 'react';
import { NavLink, useNavigate } from 'react-router-dom';
import { ActionIcon, Badge, Group, Stack, Text, Tooltip } from '@mantine/core';
import { CircleUserRound, LogOut } from 'lucide-react';

import { DolletMark } from '../components/DolletMark.jsx';
import { NAV_ITEMS, NOTIFICATIONS_PATH } from './nav.js';
import { useSession } from '../auth/session.js';
import { useLeaveGuard } from '../unsavedChanges.js';
import {
  USER_LEVEL_LABELS,
  channels,
  fetchVersion,
  notifications,
} from '../api/resources.js';
import classes from './Sidebar.module.css';

/**
 * How often the badge asks again.
 *
 * The conditions behind it are raised by jobs that run on the hour, so a minute
 * is already far finer than the thing being watched; polling faster would only
 * make an idle tab talk to the server for nothing.
 */
const BADGE_INTERVAL_MS = 60_000;

/**
 * The mark and the name, as the link home. The sidebar leads with it, and on
 * a narrow window the top bar shows the same one beside the burger.
 */
export function Brand({ onNavigate }) {
  const guard = useLeaveGuard();

  return (
    <NavLink
      to="/channels"
      className={classes.brand}
      onClick={(event) => {
        guard('/channels')(event);
        onNavigate?.();
      }}
    >
      {/* Decorative beside the name: the mark labels itself "Dollet", and a
          link that reads "Dollet Dollet" is not an improvement. */}
      <span className={classes.mark}>
        <DolletMark size={18} aria-hidden="true" />
      </span>
      Dollet
    </NavLink>
  );
}

/**
 * `onNavigate` is called on any link, chosen or held back by the leave guard
 * alike: the drawer this renders in on a narrow window closes either way, and
 * the guard's confirm then has the screen to itself.
 */
export function Sidebar({ onNavigate }) {
  const user = useSession((state) => state.user);
  const isAdmin = useSession((state) => state.isAdmin());
  const logout = useSession((state) => state.logout);
  const navigate = useNavigate();
  const guard = useLeaveGuard();
  const [version, setVersion] = useState(null);
  const [counts, setCounts] = useState({});
  const [unacknowledged, setUnacknowledged] = useState(0);

  useEffect(() => {
    let live = true;
    const apply = (update) => {
      if (live) update();
    };

    fetchVersion().then((found) => apply(() => found && setVersion(found)));
    channels
      .count()
      .then((count) => apply(() => count !== null && setCounts({ channels: count })));

    return () => {
      live = false;
    };
  }, []);

  // The routes are admin-only, so a standard user asking would collect a 403
  // every minute for a badge they are never shown.
  useEffect(() => {
    if (!isAdmin) return undefined;

    let live = true;
    const poll = () =>
      notifications.count().then((count) => {
        if (live && count !== null) setUnacknowledged(count);
      });

    void poll();
    const timer = setInterval(poll, BADGE_INTERVAL_MS);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [isAdmin]);

  const signOut = () => {
    logout();
    navigate('/login', { replace: true });
  };

  const items = NAV_ITEMS.filter((item) => !item.adminOnly || isAdmin);

  const leave = (to) => (event) => {
    guard(to)(event);
    onNavigate?.();
  };

  return (
    <nav className={classes.sidebar} aria-label="Main">
      <Brand onNavigate={onNavigate} />

      <div className={classes.nav}>
        {items.map(({ label, to, icon: Icon, count }) => (
          <NavLink key={to} to={to} className={classes.link} onClick={leave(to)}>
            <Icon size={17} strokeWidth={1.9} className={classes.linkIcon} />
            {label}
            {count && counts[count] !== undefined && (
              <span className={classes.count}>({counts[count]})</span>
            )}
            {to === NOTIFICATIONS_PATH && unacknowledged > 0 && (
              <Badge
                size="sm"
                circle
                color="red"
                className={classes.badge}
                aria-label={`${unacknowledged} unacknowledged`}
              >
                {unacknowledged}
              </Badge>
            )}
          </NavLink>
        ))}
      </div>

      <div className={classes.foot}>
        <Group gap={10} wrap="nowrap">
          <CircleUserRound
            size={26}
            strokeWidth={1.5}
            color="var(--mantine-color-dark-2)"
          />
          <Stack gap={0} style={{ minWidth: 0, flex: 1 }}>
            <span className={classes.userName}>{user?.username ?? 'Signed in'}</span>
            {user?.user_level != null && (
              <span className={classes.userLevel}>
                {USER_LEVEL_LABELS[user.user_level] ?? `Level ${user.user_level}`}
              </span>
            )}
          </Stack>
          <Tooltip label="Sign out">
            <ActionIcon
              variant="subtle"
              color="gray"
              onClick={signOut}
              aria-label="Sign out"
            >
              <LogOut size={16} />
            </ActionIcon>
          </Tooltip>
        </Group>
        <Text component="span" className={classes.version}>
          {version ? `v${version}` : ''}
        </Text>
      </div>
    </nav>
  );
}
