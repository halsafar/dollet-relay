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

/**
 * Sources stacks two tables under two notes, and on a phone the notes take
 * most of the height. The page has to scroll for that. A layout that shared
 * the window out instead cut each note to its first lines and each table to a
 * pane too short for one row, while the footer still counted them; and the
 * decision list, one line per item, pushed the first note's prose past its
 * right edge.
 */
test('the Sources page scrolls rather than clipping its notes and tables', async ({
  page,
}) => {
  await signIn(page, USERS.admin);
  await page.goto('/sources');
  await expect(heading(page, 'Sources')).toBeVisible();

  // Soft, so that one run names every clipped part rather than the first.
  // Whole, not cut off at the bottom: its button is the last thing in it.
  const note = page
    .getByRole('alert')
    .filter({ hasText: 'Match channels to guide data' });
  const noteBox = await note.boundingBox();
  const button = await note
    .getByRole('button', { name: 'Match unmapped channels' })
    .boundingBox();
  expect.soft(button.y + button.height).toBeLessThanOrEqual(noteBox.y + noteBox.height);

  // Clamped to the note's width rather than widening it.
  const decisions = page.getByRole('alert').filter({ hasText: 'a guide decision' });
  const decisionsBox = await decisions.boundingBox();
  const item = await decisions.getByRole('listitem').first().boundingBox();
  expect
    .soft(item.x + item.width)
    .toBeLessThanOrEqual(decisionsBox.x + decisionsBox.width);

  // Every row inside the table's pane. Visibility is not the check, because a
  // row clipped by its scrolling ancestor still counts as visible.
  for (const [name, last] of [
    ['M3U accounts', 'Synth Retired'],
    ['Guide sources', 'Synth Retired Guide'],
  ]) {
    const table = page.getByRole('table', { name });
    await expect(table.getByRole('cell', { name: last, exact: true })).toBeVisible();
    const pane = await table.locator('..').boundingBox();
    const row = await table.getByRole('row').last().boundingBox();
    expect.soft(row.y + row.height).toBeLessThanOrEqual(pane.y + pane.height);
  }
});
