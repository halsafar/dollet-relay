import {
  Activity,
  Bell,
  CalendarDays,
  Image,
  Layers,
  Plug,
  Radio,
  Settings,
  Tv,
  Users,
} from 'lucide-react';

/**
 * Named, because the sidebar hangs the unacknowledged badge off this one entry
 * and two copies of a path is how a link and its badge end up on different
 * screens.
 */
export const NOTIFICATIONS_PATH = '/notifications';

/**
 * Top to bottom is the order an operator works in: add a provider, arrange its
 * groups and their numbering, curate the channels and their logos, check the
 * guide, hand clients the connect URLs, then watch the server and administer
 * it.
 *
 * VODs, DVR, and Plugins are omitted rather than shown as dead links — a
 * deferred feature is a documented gap, never a stub that fakes success.
 *
 * `adminOnly` mirrors the server: those handlers extract `AdminUser` and answer
 * 403, so showing the link to a streamer only offers them an error.
 */
export const NAV_ITEMS = [
  { label: 'Sources', to: '/sources', icon: Radio },
  // A group could be a filter on the channel table and a checkbox in a
  // provider dialog. Here it is where the lineup's numbering is decided, which
  // is a screen.
  { label: 'Groups', to: '/groups', icon: Layers },
  { label: 'Channels', to: '/channels', icon: Tv, count: 'channels' },
  { label: 'Logos', to: '/logos', icon: Image },
  { label: 'TV Guide', to: '/guide', icon: CalendarDays },
  // Admin-only: it shows nobody else's data, but it does show
  // `advertised_base_url` and the addresses clients have reached this server
  // on, which are deployment facts.
  { label: 'Connect', to: '/connect', icon: Plug, adminOnly: true },
  { label: 'Stats', to: '/stats', icon: Activity, adminOnly: true },
  // Admin-only for the same reason the routes behind it are: a notification
  // names a provider account and the pattern that would not compile.
  { label: 'Notifications', to: NOTIFICATIONS_PATH, icon: Bell, adminOnly: true },
  { label: 'Users', to: '/users', icon: Users, adminOnly: true },
  { label: 'Settings', to: '/settings', icon: Settings, adminOnly: true },
];
