import { defineConfig, devices } from '@playwright/test';

/**
 * The browser layer. `scripts/e2e.sh` is what runs it: it builds the SPA,
 * starts the two servers these tests address and exports their URLs, so
 * running `playwright test` on its own fails at the first navigation — which
 * is the honest failure, because there is nothing to test against.
 *
 * Chromium only. These journeys are about the app being served and rendered at
 * all, not about engine differences, and a second browser doubles the runtime
 * of the slowest layer in the suite for no claim it can make alone.
 */
export default defineConfig({
  testDir: './e2e',
  // One worker, in declaration order. Both servers are shared mutable state:
  // the first-run journey creates the administrator that the rest of its file
  // signs in as, and the settings journey writes to the seeded instance.
  workers: 1,
  fullyParallel: false,
  retries: 0,
  reporter: 'list',
  use: {
    baseURL: process.env.E2E_SEEDED_URL,
    trace: 'on-first-retry',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
