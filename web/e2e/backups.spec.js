/**
 * Backups, through the built SPA and a real server.
 *
 * Restore is not driven here: it restarts the server, and this harness owns
 * that process and does not restart it, so a restore would leave every later
 * journey talking to nothing. The server side of a restore, through to the
 * boot that applies it, is covered by `crates/dollet-server/src/api/tests/backups.rs`.
 */
import { readFile } from 'node:fs/promises';

import { expect, test } from '@playwright/test';

import { USERS, heading, settingsSection, signIn } from './helpers.js';

test('a backup taken by hand is listed and downloads as a zip', async ({ page }) => {
  await signIn(page, USERS.admin);
  await page.goto('/settings');
  await expect(heading(page, 'Settings')).toBeVisible();

  const backups = await settingsSection(page, 'Backups');
  await backups.getByRole('button', { name: 'Back up now' }).click();

  const notice = page.getByText(/^Backed up as dollet-backup-\d{8}-\d{6}-manual\.zip$/);
  await expect(notice).toBeVisible();
  const name = (await notice.textContent()).replace('Backed up as ', '');

  const row = backups.getByRole('row').filter({ hasText: name });
  await expect(row).toContainText('By hand');

  const [download] = await Promise.all([
    page.waitForEvent('download'),
    row.getByRole('button', { name: `Download ${name}` }).click(),
  ]);
  expect(download.suggestedFilename()).toBe(name);

  // The bytes the browser saved, not the request: a download that is really an
  // error page or the SPA's index.html still saves under the right name.
  const saved = await readFile(await download.path());
  expect(saved.subarray(0, 2).toString('latin1')).toBe('PK');
});
