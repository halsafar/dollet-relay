import { create } from 'zustand';
import { USER_LEVELS, auth } from '../api/resources.js';
import { tokenStore } from './tokenStore.js';

/**
 * @typedef {'loading' | 'authenticated' | 'anonymous'} SessionStatus
 */

/**
 * Who is logged in, as distinct from which tokens are held.
 *
 * Boot is optimistic: if a refresh token survived the reload the shell renders
 * immediately and `/users/me/` fills in the profile, rather than flashing the
 * login page while a round trip completes.
 */
export const useSession = create((set, get) => ({
  /** @type {SessionStatus} */
  status: tokenStore.hasTokens() ? 'loading' : 'anonymous',
  /** @type {import('../api/resources.js').User | null} */
  user: null,

  async login(username, password) {
    const tokens = await auth.login(username, password);
    await get().adopt(tokens);
  },

  /**
   * Creates the first administrator on an empty instance and signs in.
   *
   * The server answers with the same token pair as a login, so there is no
   * second sign-in step and no window where the account exists but nobody is
   * holding it.
   */
  async bootstrap(username, password) {
    const tokens = await auth.bootstrap(username, password);
    await get().adopt(tokens);
  },

  /** @param {{access: string, refresh: string}} tokens */
  async adopt(tokens) {
    tokenStore.setTokens({ access: tokens.access, refresh: tokens.refresh });
    set({ status: 'loading' });
    await get().loadUser();
  },

  logout() {
    tokenStore.clear();
    set({ status: 'anonymous', user: null });
  },

  /** Resolves the current profile, demoting to anonymous if the tokens are dead. */
  async loadUser() {
    if (!tokenStore.hasTokens()) {
      set({ status: 'anonymous', user: null });
      return;
    }
    try {
      const user = await auth.me();
      set({ status: 'authenticated', user });
    } catch (error) {
      if (error?.isAuthError) {
        tokenStore.clear();
      }
      // Whether we still hold credentials is the only thing that decides this,
      // not which error came back. Reading the error class instead would let a
      // failure that already cleared the tokens re-assert "authenticated" and
      // strand the user in a shell making unauthenticated requests.
      if (!tokenStore.hasTokens()) {
        set({ status: 'anonymous', user: null });
        return;
      }
      // A server that is down or half-built must not look like a logged-out
      // session, or the user is bounced to a login page that also cannot work.
      set({ status: 'authenticated', user: null });
    }
  },

  isAdmin: () => (get().user?.user_level ?? -1) >= USER_LEVELS.ADMIN,
}));

/**
 * Clearing tokens is how the API client reports an unrecoverable 401. Watching
 * the store keeps that one-way: no callback plumbed through the client, and no
 * way for the two to disagree about whether a session exists.
 */
tokenStore.subscribe((state) => {
  if (!state.access && !state.refresh && useSession.getState().status !== 'anonymous') {
    useSession.setState({ status: 'anonymous', user: null });
  }
});

/** Kicks off the optimistic boot described above. Called once, from `main`. */
export function restoreSession() {
  if (tokenStore.hasTokens()) void useSession.getState().loadUser();
}
