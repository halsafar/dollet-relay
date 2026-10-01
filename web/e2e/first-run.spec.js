import { expect, test } from '@playwright/test';

import { EMPTY_URL, PASSWORD_HASH_TIMEOUT, heading, settingsSection } from './helpers.js';

/**
 * The instance nobody has signed into yet. Without this screen a fresh install
 * has no way in at all: the sign-in form would 401 forever against a database
 * with no accounts in it.
 */
const FIRST_ADMIN = { username: 'e2e-owner', password: 'e2e-owner-password' };

// Serial and in this order: these share one server, and the account the second
// test creates is what the two after it depend on. Nothing else in the suite
// touches the empty instance.
test.describe.configure({ mode: 'serial' });

test.describe('the first run of an empty instance', () => {
  test.use({ baseURL: EMPTY_URL });

  /** The account the second test creates, once it exists. */
  const signInAsOwner = async (page) => {
    await page.goto('/login');
    await page.getByLabel('Username').fill(FIRST_ADMIN.username);
    await page.getByLabel('Password', { exact: true }).fill(FIRST_ADMIN.password);
    await page.getByRole('button', { name: 'Sign in' }).click();
    await expect(heading(page, 'Channels')).toBeVisible({
      timeout: PASSWORD_HASH_TIMEOUT,
    });
  };

  test('offers to create the first administrator rather than a sign-in form', async ({
    page,
  }) => {
    await page.goto('/');

    await expect(page.getByText('Create the first administrator')).toBeVisible();
    await expect(page.getByText('This instance has no accounts yet')).toBeVisible();
    await expect(
      page.getByRole('button', { name: 'Create administrator' }),
    ).toBeVisible();
    await expect(page.getByRole('button', { name: 'Sign in' })).toHaveCount(0);
  });

  test('creating that administrator lands in the app already signed in', async ({
    page,
  }) => {
    await page.goto('/');

    await page.getByLabel('Username').fill(FIRST_ADMIN.username);
    await page.getByLabel('Password', { exact: true }).fill(FIRST_ADMIN.password);
    await page.getByRole('button', { name: 'Create administrator' }).click();

    // No second sign-in step: the server answers the bootstrap with the same
    // token pair a login would, so there is no window where the account exists
    // and nobody holds it.
    await expect(page).toHaveURL(/\/channels$/, { timeout: PASSWORD_HASH_TIMEOUT });
    await expect(heading(page, 'Channels')).toBeVisible();
    await expect(page.getByRole('navigation', { name: 'Main' })).toContainText(
      FIRST_ADMIN.username,
    );
  });

  test('a second visit offers sign-in only, because the instance now has an owner', async ({
    page,
  }) => {
    // A new context, so nothing carries over but the server's own state.
    await page.goto('/');

    await expect(page.getByText('Sign in to continue')).toBeVisible();
    await expect(page.getByText('Create the first administrator')).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Sign in' })).toBeVisible();
  });

  test('a fresh instance offers every network endpoint rather than an empty section', async ({
    page,
  }) => {
    await signInAsOwner(page);

    await page.goto('/settings');
    await page.getByRole('button', { name: 'Network Access' }).click();

    // `network_access` is `{}` until someone restricts something — which is
    // exactly when they come looking for this control. Rendering the stored
    // map's keys would show a fresh instance nothing but the help line.
    const network = settingsSection(page, 'Network Access');
    for (const label of [
      'Web app and API',
      'Playlist, guide and HDHomeRun',
      'Streams',
      'Xtream Codes API',
    ]) {
      await expect(network.getByLabel(label, { exact: true })).toHaveValue('');
    }
  });
});
