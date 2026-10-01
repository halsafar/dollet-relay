import { expect, test } from '@playwright/test';

import { USERS, signIn } from './helpers.js';

test('signing out returns to the sign-in form and a protected route redirects', async ({
  page,
}) => {
  await signIn(page, USERS.admin);

  await page.getByRole('button', { name: 'Sign out' }).click();
  await expect(page).toHaveURL(/\/login$/);
  await expect(page.getByText('Sign in to continue')).toBeVisible();

  // The tokens are gone rather than merely unused: a protected route asked for
  // directly has to bounce, or a stale shell sits there making unauthenticated
  // requests.
  await page.goto('/settings');
  await expect(page).toHaveURL(/\/login$/);
  await expect(page.getByText('Sign in to continue')).toBeVisible();
});
