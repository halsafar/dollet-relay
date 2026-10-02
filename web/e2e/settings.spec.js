import { expect, test } from '@playwright/test';

import { USERS, heading, settingsSection, signIn } from './helpers.js';

test.describe('settings', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page, USERS.admin);
    await page.goto('/settings');
    await expect(heading(page, 'Settings')).toBeVisible();
  });

  test('the default stream profile is chosen by name and survives a reload', async ({
    page,
  }) => {
    const stream = await settingsSection(page, 'Streaming');
    const profile = stream.getByLabel('Default stream profile');

    // The seed stores id 3. What the operator must see is its name — an id in
    // a select is a number nobody can check against anything.
    await expect(profile).toHaveValue('proxy');

    await profile.click();
    await page.getByRole('option', { name: 'Synth Direct', exact: true }).click();
    await expect(profile).toHaveValue('Synth Direct');

    await stream.getByRole('button', { name: 'Save' }).click();
    await expect(page.getByText('Saved Streaming')).toBeVisible();

    // The round trip is the claim: a page that only updated its own state
    // looks identical until the next visit.
    await page.reload();
    const reloaded = await settingsSection(page, 'Streaming');
    await expect(reloaded.getByLabel('Default stream profile')).toHaveValue(
      'Synth Direct',
    );
  });

  test('network access offers every endpoint class, whether or not one is stored', async ({
    page,
  }) => {
    const network = await settingsSection(page, 'Network access');

    // One class is restricted in the seed and three are not. Iterating the
    // stored map would make the three that are not disappear.
    await expect(
      network.getByLabel('Playlist, guide and HDHomeRun', { exact: true }),
    ).toHaveValue('127.0.0.0/8');
    for (const label of ['Web app and API', 'Streams', 'Xtream Codes API']) {
      await expect(network.getByLabel(label, { exact: true })).toHaveValue('');
    }
  });

  test('an entry that is not a CIDR is refused inline and the draft is kept', async ({
    page,
  }) => {
    const network = await settingsSection(page, 'Network access');

    await network.getByLabel('Streams', { exact: true }).fill('192.168.1.0/33');
    await network.getByRole('button', { name: 'Save' }).click();

    // The server's own reason, on screen next to the field being fixed —
    // an entry that will not parse is skipped at request time, so saving one
    // widens access instead of narrowing it.
    await expect(network.getByText('not CIDR ranges')).toBeVisible();
    await expect(network.getByLabel('Streams', { exact: true })).toHaveValue(
      '192.168.1.0/33',
    );
  });

  test('a larger text size is kept by this browser across a reload', async ({ page }) => {
    const appearance = await settingsSection(page, 'Appearance');
    const textSize = appearance.getByRole('radiogroup', { name: 'Text size' });
    const navLink = page
      .getByRole('navigation', { name: 'Main' })
      .getByRole('link', { name: 'Logos' });

    await expect(navLink).toHaveCSS('font-size', '13.5px');

    // The label, because the radio itself is drawn zero-sized under it.
    await textSize.getByText('Large', { exact: true }).click();
    await expect(textSize.getByRole('radio', { name: 'Large' })).toBeChecked();

    await page.reload();
    await expect(textSize.getByRole('radio', { name: 'Large' })).toBeChecked();

    // The choice reaching the page, not only the control: a CSS-module size
    // multiplied by the scale Mantine now carries.
    await expect(navLink).toHaveCSS('font-size', '15.12px');
  });
});
