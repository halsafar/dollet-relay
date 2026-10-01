import { expect, test } from '@playwright/test';

import { SEEDED_URL, USERS, heading, signIn } from './helpers.js';

/**
 * The one screen whose whole subject is an address, checked against the address
 * the browser actually used.
 *
 * jsdom cannot make this claim: `window.location.origin` there is a constant of
 * the test environment, and the question this page exists to answer is what a
 * real client reaching a real server sees. The second half is the same
 * question from the server's side — a lineup fetched over HTTP has to appear in
 * the page's own "seen from" list.
 */
/**
 * By role, not by label: every URL field carries a copy button whose
 * `aria-label` names the same field, and `getByLabel` matches both.
 */
function urlField(page, name) {
  return page.getByRole('textbox', { name });
}

test('the Connect page hands out the address the client is using', async ({ page }) => {
  await signIn(page, USERS.admin);
  await page.goto('/connect');
  await expect(heading(page, 'Connect')).toBeVisible();

  // The default: whatever this browser reached the server on.
  await expect(urlField(page, 'Tuner address')).toHaveValue(`${SEEDED_URL}/hdhr/`);
  await expect(urlField(page, 'Playlist URL')).toHaveValue(`${SEEDED_URL}/output/m3u`);

  // A container name is the case the picker exists for: nothing the server can
  // derive, and the address every other container on that network uses.
  await page.getByLabel('Base address').getByText('Custom', { exact: true }).click();
  await urlField(page, 'Custom base URL').fill('http://dollet-relay:9191');

  await expect(urlField(page, 'Tuner address')).toHaveValue(
    'http://dollet-relay:9191/hdhr/',
  );

  // A profile name with a space in it, which is the encoding the server's own
  // lineup uses and the one `encodeURIComponent` alone would get wrong for a
  // name carrying brackets.
  const plex = page
    .getByRole('heading', { name: 'Plex — HDHomeRun tuner' })
    .locator('..');
  await plex.getByRole('textbox', { name: 'Channel profile' }).click();
  await page.getByRole('option', { name: 'Living Room' }).click();

  await expect(urlField(page, 'Tuner address')).toHaveValue(
    'http://dollet-relay:9191/hdhr/Living%20Room/',
  );
});

test('an address a client has fetched a lineup on appears in Seen from', async ({
  page,
}) => {
  await signIn(page, USERS.admin);

  // Plex's own request, made the way Plex makes it: no credential, no browser.
  const lineup = await page.request.get(`${SEEDED_URL}/hdhr/lineup.json`);
  expect(lineup.status()).toBe(200);

  await page.goto('/connect');
  await expect(heading(page, 'Connect')).toBeVisible();

  const row = page.locator('code').filter({ hasText: SEEDED_URL });
  await expect(row).toHaveCount(1);
  // Which output it was asked for, so an operator can tell a Plex tuner from a
  // player fetching a playlist.
  await expect(row.locator('..')).toContainText('hdhr');
});
