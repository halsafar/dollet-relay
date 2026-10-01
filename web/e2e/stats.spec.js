import { expect, test } from '@playwright/test';

import { USERS, heading, signIn } from './helpers.js';

test('the stats page says nothing is streaming rather than failing on an idle server', async ({
  page,
}) => {
  await signIn(page, USERS.admin);
  await page.goto('/stats');

  await expect(heading(page, 'Stats')).toBeVisible();
  await expect(page.getByText('Nothing is streaming')).toBeVisible();

  // An idle server is the normal state, so an empty session list must not
  // arrive as an error — and the WebSocket this page opens must not either.
  await expect(page.getByRole('alert')).toHaveCount(0);
});
