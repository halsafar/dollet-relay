import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';

import {
  MANIFEST_PATH,
  collectRoutes,
  serialize,
} from '../../scripts/route-manifest.mjs';

/**
 * The manifest is what the Rust side resolves against its router, so a stale
 * one is worse than none: it would assert that a path this client no longer
 * calls still exists, and say nothing about the one it calls now.
 *
 * This is also the only test in the suite that does not assert a path literal
 * against itself. Every entry here came from running the client.
 */
describe('the route manifest', () => {
  it('matches what the client actually calls', async () => {
    const generated = serialize(await collectRoutes());
    const committed = readFileSync(MANIFEST_PATH, 'utf8');

    expect(committed).toBe(generated);
  });

  it('marks only the genuinely public endpoints as needing no token', async () => {
    const routes = await collectRoutes();
    const open = routes.filter((route) => route.auth === 'none').map((r) => r.path);

    // Sign-in cannot require a token, refresh authenticates with the refresh
    // token in its body, and the version label is read before the shell knows
    // who is looking. Anything else appearing here is an endpoint that stopped
    // sending credentials.
    expect(open.sort()).toEqual([
      // A fresh install has nobody to authenticate as, so the setup check and
      // the first-administrator creation cannot require a token.
      '/api/accounts/initialize-superuser/',
      '/api/accounts/initialize-superuser/',
      '/api/accounts/token/',
      '/api/accounts/token/refresh/',
      '/api/core/version/',
      // The tuner's own identity, which Plex reads with nothing but a URL. The
      // Connect page asks the same question the same way rather than
      // reimplementing the server's device-id rules in the browser.
      '/hdhr/discover.json',
    ]);
  });

  it('covers the socket, which is not a fetch call', async () => {
    const routes = await collectRoutes();
    const socket = routes.find((route) => route.name === 'ws.connect');

    expect(socket).toEqual({
      name: 'ws.connect',
      method: 'GET',
      path: '/ws',
      auth: 'subprotocol',
    });
  });

  it('names the client function behind every route', async () => {
    const routes = await collectRoutes();

    // So a server-side failure reads "users.remove expects DELETE
    // /api/accounts/users/1/" rather than just a path.
    expect(routes.every((route) => route.name.length > 0)).toBe(true);
    expect(routes.length).toBeGreaterThan(50);
  });
});
