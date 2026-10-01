import { expect, test } from '@playwright/test';

import { NAV_ITEMS } from '../src/layout/nav.js';
import { USERS, heading, signIn } from './helpers.js';

/**
 * A hard refresh on a client route. The fallback must serve `index.html` as
 * `text/html` for an extensionless path like `/settings`, or Chromium saves
 * the page to disk — and no unit test asks a built server for a URL.
 *
 * The routes come from the sidebar the client renders, so a screen added there
 * is covered the day it lands. Each one's heading is its nav label, which is
 * what makes "the app is still on screen" a single assertion.
 */
test.describe('a hard refresh', () => {
  for (const { label, to } of NAV_ITEMS) {
    test(`on ${to} renders the app rather than downloading it`, async ({ page }) => {
      const downloads = [];
      page.on('download', (download) => downloads.push(download.suggestedFilename()));

      await signIn(page, USERS.admin);
      await page.goto(to);
      await expect(heading(page, label)).toBeVisible();

      // A response the browser treats as a download aborts the navigation, so
      // this is where a wrong content type throws. Caught, because the useful
      // failure message is the one below and not `net::ERR_ABORTED`.
      const reloaded = await page
        .reload({ waitUntil: 'domcontentloaded' })
        .catch(() => null);

      expect(downloads, `${to} was downloaded instead of rendered`).toEqual([]);
      expect(reloaded?.status(), `${to} did not answer a reload`).toBe(200);
      expect(await page.evaluate(() => document.contentType)).toBe('text/html');
      await expect(heading(page, label)).toBeVisible();
    });
  }

  test('on / renders the app rather than downloading it', async ({ page }) => {
    const downloads = [];
    page.on('download', (download) => downloads.push(download.suggestedFilename()));

    await signIn(page, USERS.admin);
    await page.goto('/');

    const reloaded = await page
      .reload({ waitUntil: 'domcontentloaded' })
      .catch(() => null);

    expect(downloads, '/ was downloaded instead of rendered').toEqual([]);
    expect(reloaded?.status(), '/ did not answer a reload').toBe(200);
    expect(await page.evaluate(() => document.contentType)).toBe('text/html');
    await expect(heading(page, 'Channels')).toBeVisible();
  });

  test('on /login while signed out renders the sign-in form rather than downloading it', async ({
    page,
  }) => {
    const downloads = [];
    page.on('download', (download) => downloads.push(download.suggestedFilename()));

    await page.goto('/login');
    await expect(page.getByText('Sign in to continue')).toBeVisible();

    const reloaded = await page
      .reload({ waitUntil: 'domcontentloaded' })
      .catch(() => null);

    expect(downloads, '/login was downloaded instead of rendered').toEqual([]);
    expect(reloaded?.status(), '/login did not answer a reload').toBe(200);
    expect(await page.evaluate(() => document.contentType)).toBe('text/html');
    await expect(page.getByText('Sign in to continue')).toBeVisible();
  });
});
