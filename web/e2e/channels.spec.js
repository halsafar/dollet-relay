import { expect, test } from '@playwright/test';

import { USERS, heading, signIn } from './helpers.js';

/**
 * Names carrying a double quote, an ampersand, angle brackets and non-ASCII.
 * Each one is a byte that some serializer on the way to this table has to
 * escape and this table has to *un*escape — a channel listed as
 * `Synth &amp; Ampersand` is the same bug as one listed as `Synth`.
 */
const AWKWARD = [
  { number: '1', name: 'Synth One' },
  { number: '2.5', name: 'Synth Two & A Half' },
  { number: '3', name: 'Synth "Quoted" Channel' },
  { number: '4', name: 'Synth <Angle> & Ampersand' },
  { number: '5', name: 'Synth Ünïcøde Ñoise' },
];

test.describe('the channel list', () => {
  test.beforeEach(async ({ page }) => {
    await signIn(page, USERS.admin);
    await page.goto('/channels');
    await expect(heading(page, 'Channels')).toBeVisible();
  });

  test('renders the seeded channels with their numbers and names unescaped', async ({
    page,
  }) => {
    const table = page.getByRole('table', { name: 'Channels' });

    for (const { number, name } of AWKWARD) {
      const row = table.getByRole('row').filter({ hasText: name });
      await expect(row, `no row for ${name}`).toHaveCount(1);
      await expect(row).toContainText(number);
    }

    // The overridden channel, whose base row says `Provider Raw Name` and
    // number 99. Neither may reach a screen.
    await expect(table).not.toContainText('Provider Raw Name');
  });

  test('names the guide a channel is mapped to, not its provider label', async ({
    page,
  }) => {
    const row = page
      .getByRole('table', { name: 'Channels' })
      .getByRole('row')
      .filter({ hasText: 'Synth One' });

    // The seed maps `Synth One` to guide channel `Synth Sports Guide` and gives
    // it the `tvg-id` `synth.sports`. Whether a channel has listings is the
    // mapping, never the label — and most mapped channels carry no label at
    // all, so a column reporting one reads as "no EPG" for a channel the TV
    // Guide fills.
    await expect(row).toContainText('Synth Sports Guide');
    await expect(row).not.toContainText('synth.sports');
  });

  /**
   * The operator's actual question — which channels Plex will show an empty
   * strip for — and the one option whose value is not a guide name but a
   * sentinel. A sentinel is exactly the sort of thing that works in jsdom and
   * not in a browser, so it is asked here too.
   */
  test('filters the lineup down to the channels with no guide at all', async ({
    page,
  }) => {
    const table = page.getByRole('table', { name: 'Channels' });
    await page.getByRole('textbox', { name: 'Filter by guide' }).click();
    await page.getByRole('option', { name: 'No guide' }).click();

    // Of the seeded seventeen, three are mapped: `Synth One`, and the two the
    // matcher mapped without a label.
    await expect(table.getByRole('row')).toHaveCount(14);
    await expect(table).not.toContainText('Synth Gap Guide');
    await expect(table).toContainText('Synth Unnumbered');
  });

  /**
   * The picker is the one control on this screen that cannot work from data
   * already in the browser: `epg_data` is the largest table there is,
   * so the options come from a search against the server. A jsdom test can
   * prove the request is made and the id is sent; only a browser proves the
   * dropdown opens, the option is clickable, and the saved mapping comes back
   * named in the column.
   */
  test('maps an unmapped channel to a guide channel found by searching', async ({
    page,
  }) => {
    const table = page.getByRole('table', { name: 'Channels' });
    const row = table.getByRole('row').filter({ hasText: 'Synth Streamless' });
    await expect(row).not.toContainText('Synth Unmapped Guide');

    await page
      .getByRole('button', { name: 'Edit Synth Streamless', exact: true })
      .click();
    const dialog = page.getByRole('dialog');
    const guide = dialog.getByLabel('Guide', { exact: true });
    await expect(guide).toHaveValue('');

    await guide.click();
    await guide.fill('Unmapped');

    // The seed publishes this one and maps nothing to it, which is exactly the
    // row an operator reaches for a picker to assign.
    await page.getByRole('option', { name: /Synth Unmapped Guide/ }).click();
    await expect(guide).toHaveValue('Synth Unmapped Guide');

    await dialog.getByRole('button', { name: 'Save' }).click();
    await expect(dialog).toBeHidden();

    // The EPG column names the guide a channel is mapped to, so the round trip
    // is visible without opening the editor again.
    await expect(row).toContainText('Synth Unmapped Guide');

    // Put it back. Every journey in this file shares one seeded server, so a
    // test that leaves a channel mapped makes the "No guide" count above
    // depend on running first — an ordering contract nothing declares and
    // `--grep` does not honour. Clearing it is also the only place a browser
    // exercises sending `epg_data_id: null`.
    await page
      .getByRole('button', { name: 'Edit Synth Streamless', exact: true })
      .click();
    await dialog.getByLabel('Clear the guide mapping').click();
    await expect(guide).toHaveValue('');
    await dialog.getByRole('button', { name: 'Save' }).click();
    await expect(dialog).toBeHidden();
    await expect(row).not.toContainText('Synth Unmapped Guide');
  });

  test('opening a channel shows its failover streams in the seeded order', async ({
    page,
  }) => {
    await page.getByRole('button', { name: 'Edit Synth One', exact: true }).click();

    const dialog = page.getByRole('dialog');
    await expect(dialog.getByText('Failover order')).toBeVisible();

    // Position 1 is played first and the rest are tried in this order, so the
    // order is the behaviour, not a presentation detail. The seed puts the
    // standard account first, the Xtream one second and the hand-added stream
    // last — three streams across two provider accounts.
    await expect(dialog.locator('[class*="failoverName"]')).toHaveText([
      'Synth One HD',
      'Synth One Backup',
      'Synth Hand Added',
    ]);
  });
});
