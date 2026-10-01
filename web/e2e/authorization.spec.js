import { expect, test } from '@playwright/test';

import { NAV_ITEMS } from '../src/layout/nav.js';
import { SEEDED_URL, USERS, XC_PASSWORDS, heading, signIn } from './helpers.js';

test.describe('a user below admin', () => {
  test('sees no admin-only entries in the sidebar', async ({ page }) => {
    await signIn(page, USERS.streamer);
    const sidebar = page.getByRole('navigation', { name: 'Main' });

    for (const { label, adminOnly } of NAV_ITEMS) {
      const link = sidebar.getByRole('link', { name: label, exact: true });
      // The server answers these 403, so showing the link would only offer
      // the streamer an error.
      await expect(link, label).toHaveCount(adminOnly ? 0 : 1);
    }
  });

  test('is refused the settings form on a direct visit to /settings', async ({
    page,
  }) => {
    await signIn(page, USERS.streamer);
    await page.goto('/settings');

    // The shell renders — they are signed in — and the screen says why it is
    // empty rather than showing a form whose every save would 403.
    await expect(heading(page, 'Settings')).toBeVisible();
    await expect(page.getByText('not permitted for this account').first()).toBeVisible();
    await expect(page.getByLabel('Default stream profile')).toHaveCount(0);
  });

  test('has their channel-profile restriction applied to the catalogue they fetch', async ({
    page,
  }) => {
    await signIn(page, USERS.standard);

    // The SPA has no screen for a user's own channel-profile restriction —
    // every /api/channels route is admin-only — so the browser can only
    // observe it where their player would: the Xtream catalogue, which is
    // narrowed to `Living Room`.
    const catalogue = async (user, password) => {
      const response = await page.request.get(
        `${SEEDED_URL}/player_api.php?username=${user}&password=${password}&action=get_live_streams`,
      );
      expect(response.status()).toBe(200);
      return (await response.json()).map((entry) => entry.name);
    };

    expect(await catalogue(USERS.standard.username, XC_PASSWORDS.standard)).toEqual([
      'Synth One',
      'Synth Two & A Half',
      'Synth "Quoted" Channel',
      'Synth Unnumbered',
    ]);
    expect(
      (await catalogue(USERS.admin.username, XC_PASSWORDS.admin)).length,
    ).toBeGreaterThan(4);
  });
});
