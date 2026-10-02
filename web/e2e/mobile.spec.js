import { expect, test } from '@playwright/test';

import { USERS, heading, signIn } from './helpers.js';

/**
 * A phone-sized window. Docked, the sidebar would take most of it, so it is
 * put away behind a burger, and the Settings page lays its section list
 * across the top instead of beside the form. Both are stylesheet decisions
 * at one breakpoint, which only a browser can be asked about.
 */
test.use({ viewport: { width: 390, height: 844 } });

test('the navigation is behind a burger and the Settings sections run across the top', async ({
  page,
}) => {
  await signIn(page, USERS.admin);

  await expect(page.getByRole('navigation', { name: 'Main' })).toBeHidden();

  await page.getByRole('button', { name: 'Open navigation' }).click();
  const drawer = page.getByRole('dialog');
  await expect(drawer.getByRole('navigation', { name: 'Main' })).toBeVisible();
  await drawer.getByRole('link', { name: 'Settings' }).click();

  await expect(drawer).toBeHidden();
  await expect(heading(page, 'Settings')).toBeVisible();
  await expect(page).toHaveURL(/\/settings\/stream$/);

  // The section list is above the form, not beside it.
  const sections = page.getByRole('navigation', { name: 'Settings sections' });
  await expect(sections).toBeVisible();
  const list = await sections.boundingBox();
  const form = await page.getByRole('region', { name: 'Streaming' }).boundingBox();
  expect(form.y).toBeGreaterThanOrEqual(list.y + list.height);
});
