/**
 * Emits every route this client can call, derived by calling it.
 *
 * The problem this solves: `endpoints.test.js` asserts a path literal against
 * the same literal in `resources.js`, so it passes green against a wrong path.
 * A hand-written list of routes would recreate that exactly — two copies of
 * the same claim, agreeing with each other and with nothing else.
 *
 * So nothing here is transcribed. Every resource function is invoked for real
 * against a stubbed `fetch`, and the manifest is whatever URLs the client
 * actually produced. Rename a path in `resources.js` and the manifest changes
 * on the next run; the server-side test that reads it then fails, which is the
 * point.
 *
 * Run `npm run routes` to regenerate, `npm run routes:check` to verify.
 */

import { writeFileSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
export const MANIFEST_PATH = resolve(HERE, '..', 'route-manifest.json');

/**
 * Exports that are pure helpers rather than endpoint callers.
 *
 * Listed rather than inferred: a function that produces no request because it
 * is a helper and one that produces none because it is broken look identical
 * from here, and the second must not pass silently.
 */
const PURE = new Set(['asSettingGroups', 'USER_LEVELS', 'USER_LEVEL_LABELS']);

/**
 * Arguments for functions the defaults cannot serve.
 *
 * The default `[1, {}, {}]` covers an id and a payload, which is nearly every
 * one. These are the exceptions, and they carry no path information — the
 * client still builds the URL.
 */
const ARGUMENTS = {
  'guide.grid': [{}],
  // The unscoped tuner. The default `1` would destructure to the same URL, but
  // only by accident — a number has no `channelProfile`.
  'hdhr.discover': [{}],
  'auth.login': ['user', 'secret'],
  'logos.create': ['name', 'http://example.invalid/logo.png'],
  'channelGroups.create': ['group'],
  'channelProfiles.setMembership': [1, [2], true],
  'm3uAccounts.updateProfile': [1, 2, {}],
  'm3uAccounts.removeProfile': [1, 2],
  'm3uAccounts.removeFilter': [1, 2],
  'stats.stopClient': ['00000000-0000-0000-0000-000000000000', 'client-1'],
  'stats.stopChannel': ['00000000-0000-0000-0000-000000000000'],
  'stats.nextSource': ['00000000-0000-0000-0000-000000000000'],
  'stats.changeSource': ['00000000-0000-0000-0000-000000000000', 1],
  'jobs.cancel': ['m3u_refresh:1'],
  // A name in the server's own format, which is how the authorization matrix
  // recognises the segment as a backup rather than as a literal path.
  'backups.download': ['dollet-backup-20261001-000000-manual.zip'],
  'backups.restore': ['dollet-backup-20261001-000000-manual.zip'],
  'backups.remove': ['dollet-backup-20261001-000000-manual.zip'],
  'backups.upload': [new Blob(['PK'], { type: 'application/zip' })],
  'settings.update': ['proxy_settings', {}],
  'systemEvents.list': [50],
};

/** Every call the client made, in the order it made them. */
function recorder() {
  const calls = [];
  const fetchImpl = async (url, init = {}) => {
    calls.push({
      method: init.method ?? 'GET',
      url: String(url),
      // The client only attaches this when a token exists and the caller did
      // not opt out, so it is the client's own answer to "does this need auth".
      auth: init.headers?.Authorization ? 'bearer' : 'none',
    });
    return {
      status: 200,
      ok: true,
      headers: { get: () => 'application/json' },
      text: async () => '{}',
    };
  };
  return { calls, fetchImpl };
}

/**
 * Computed once per process.
 *
 * `client.js` binds `globalThis.fetch` when it is first evaluated, and ESM
 * evaluates it once, so a second run would record into the first run's
 * recorder and see nothing. The answer is a property of the source rather than
 * of when it is asked, so caching it is not merely a speed-up.
 *
 * @type {Promise<object[]> | null}
 */
let cached = null;

/** @returns {Promise<{name: string, method: string, path: string,
 *            auth: 'bearer'|'subprotocol'|'none'}[]>} */
export function collectRoutes() {
  cached ??= collectRoutesOnce();
  return cached;
}

async function collectRoutesOnce() {
  const { calls, fetchImpl } = recorder();
  const originalFetch = globalThis.fetch;

  // Before the import, not after: `createApiClient` binds `globalThis.fetch`
  // once, as a default argument, and the clients are built when `client.js` is
  // first evaluated. Swapping it afterwards records nothing.
  globalThis.fetch = fetchImpl;

  const resources = await import('../src/api/resources.js');
  const { tokenStore } = await import('../src/auth/tokenStore.js');

  // A token must be present, or every route would report `auth: false` and the
  // manifest would claim the whole API is public.
  tokenStore.setTokens({ access: 'manifest-token', refresh: 'manifest-refresh' });

  const routes = [];
  const unused = [];

  /**
   * Reached only when a request 401s, so no resource function exercises it.
   * Invoked directly rather than written down: a rename still moves the
   * manifest, and this is the route that breaks sign-in when it drifts.
   */
  const { api } = await import('../src/api/client.js');

  try {
    const before = calls.length;
    await api.refreshAccessToken().catch(() => {});
    for (const call of calls.slice(before)) {
      const [path] = call.url.split('?');
      routes.push({
        name: 'client.refreshAccessToken',
        method: call.method,
        path,
        auth: call.auth,
      });
    }

    for (const [exportName, value] of Object.entries(resources)) {
      if (PURE.has(exportName)) continue;

      const members =
        typeof value === 'function'
          ? [[exportName, value]]
          : value && typeof value === 'object'
            ? Object.entries(value).filter(([, member]) => typeof member === 'function')
            : [];

      for (const [memberName, fn] of members) {
        const name =
          typeof value === 'function' ? exportName : `${exportName}.${memberName}`;
        const before = calls.length;

        try {
          await fn(...(ARGUMENTS[name] ?? [1, {}, {}]));
        } catch {
          // A resource that throws still recorded whatever it sent first.
        }

        const produced = calls.slice(before);
        if (produced.length === 0) {
          unused.push(name);
          continue;
        }
        for (const call of produced) {
          const [path] = call.url.split('?');
          routes.push({ name, method: call.method, path, auth: call.auth });
        }
      }
    }
    routes.push(await websocketRoute());
  } finally {
    globalThis.fetch = originalFetch;
    tokenStore.clear();
  }

  if (unused.length > 0) {
    throw new Error(
      `these resource functions issued no request, so the manifest cannot ` +
        `describe them — add them to PURE if that is deliberate: ${unused.join(', ')}`,
    );
  }

  // Sorted so a regenerated manifest diffs only where the routes changed.
  routes.sort((a, b) => a.path.localeCompare(b.path) || a.method.localeCompare(b.method));
  return routes;
}

/**
 * The stats socket, which is not a `fetch` call and so appears nowhere above.
 *
 * Derived the same way: the client is asked to connect against a fake
 * `WebSocket`, and the URL it builds is the answer. Its token travels in the
 * subprotocol rather than a header, because a browser cannot set one on an
 * upgrade — hence a third value for `auth` rather than a boolean.
 */
async function websocketRoute() {
  const { createWsClient, AUTH_PROTOCOL } = await import('../src/ws/client.js');

  let observed = null;
  class Recorder {
    constructor(url, protocols) {
      observed = { url, protocols };
    }
    close() {}
  }

  const client = createWsClient({
    WebSocketImpl: Recorder,
    getToken: () => 'manifest-token',
  });
  client.connect();
  client.close();

  if (!observed) throw new Error('the WebSocket client opened no connection');
  if (!observed.protocols?.includes(AUTH_PROTOCOL)) {
    throw new Error(`the WebSocket client did not offer ${AUTH_PROTOCOL}`);
  }

  return {
    name: 'ws.connect',
    // An upgrade is a GET, so it resolves against the router like any other.
    method: 'GET',
    path: new URL(observed.url).pathname,
    auth: 'subprotocol',
  };
}

export function serialize(routes) {
  return `${JSON.stringify(
    {
      $comment:
        'Generated by web/scripts/route-manifest.mjs — do not edit. Every entry ' +
        'is a URL the SPA actually produced when its resource layer was called. ' +
        '`auth` is how the client authenticates the call: `bearer` for an ' +
        'Authorization header, `subprotocol` for the WebSocket upgrade, ' +
        '`none` for the two genuinely public endpoints. Paths are concrete, ' +
        'with 1 standing in for an id, so they resolve against a router ' +
        'directly.',
      routes,
    },
    null,
    2,
  )}\n`;
}

async function main() {
  const routes = await collectRoutes();
  const contents = serialize(routes);

  if (process.argv.includes('--check')) {
    const existing = readFileSync(MANIFEST_PATH, 'utf8');
    if (existing !== contents) {
      console.error(
        'route-manifest.json is stale. Run `npm run routes` and commit the result.',
      );
      process.exit(1);
    }
    console.log(`route manifest is current (${routes.length} routes)`);
    return;
  }

  writeFileSync(MANIFEST_PATH, contents);
  console.log(`wrote ${routes.length} routes to route-manifest.json`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  await main();
}
