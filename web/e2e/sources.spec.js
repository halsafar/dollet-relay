import { expect, test } from '@playwright/test';

import { USERS, heading, signIn } from './helpers.js';

test.describe('matching channels to guide data', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page, USERS.admin);
    await page.goto('/sources');
    await expect(heading(page, 'Sources')).toBeVisible();
  });

  /**
   * The counts are the whole answer. The matcher's third outcome — a candidate
   * it scored into the band it refuses to call — is invisible unless the run
   * reports it, and a channel left unmatched for that reason looks exactly like
   * one with no candidate at all.
   *
   * The seeded lineup has three channels whose names score into that band
   * against the seeded guide and none that scores above it, so this asserts a
   * run that assigned nothing and still had something to say.
   */
  test('reports what it matched and what it left for a human', async ({ page }) => {
    await page.getByRole('button', { name: 'Match unmapped channels' }).click();

    await expect(page.getByText('Matched 0 channels, 3 need a decision')).toBeVisible();

    // Reloaded, not left stale: the alert above the button is the list this run
    // just added to. The seed starts with one such row.
    await expect(page.getByText('4 channels need a guide decision')).toBeVisible();
  });
});
