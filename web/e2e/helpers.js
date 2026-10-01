import { expect } from '@playwright/test';

/**
 * The seeded accounts, from `mod synthetic` in
 * `crates/dollet-server/src/api/tests.rs`. The hashes in the seed are Django's
 * format at 1,000 iterations rather than the shipped 1.2 M, so signing in once
 * per test is affordable in an unoptimised build.
 */
export const USERS = {
  admin: { username: 'synthadmin', password: 'dollet-test-admin' },
  standard: { username: 'synthstandard', password: 'dollet-test-standard' },
  streamer: { username: 'synthstreamer', password: 'dollet-test-streamer' },
};

/** `custom_properties.xc_password`, which is never the login password. */
export const XC_PASSWORDS = {
  admin: 'synth-xc-admin',
  standard: 'synth-xc-standard',
};

/** The instance with no rows in it, for the first-run journey. */
export const EMPTY_URL = process.env.E2E_EMPTY_URL;

/** The instance seeded from `fixtures/synthetic/instance.sql`. */
export const SEEDED_URL = process.env.E2E_SEEDED_URL;

/**
 * Long enough for one password hash in an unoptimised build.
 *
 * A new account is hashed at the shipped 1.2 M PBKDF2 iterations, which is
 * tens of milliseconds in the release binary and about five seconds in the
 * debug one this harness builds — past Playwright's default five-second
 * assertion timeout. The seeded accounts sidestep it by being hashed at 1,000
 * iterations; an account the test creates cannot.
 */
export const PASSWORD_HASH_TIMEOUT = 30_000;

/**
 * Signs in through the form rather than by planting a token, because the form
 * is the path every user takes and the token layout is not a contract.
 *
 * Lands on /channels, whose heading is the signal that the shell mounted —
 * true even for a user whose channel list then comes back 403.
 */
export async function signIn(page, who) {
  await page.goto('/login');
  await expect(page.getByText('Sign in to continue')).toBeVisible();
  await page.getByLabel('Username').fill(who.username);
  await page.getByLabel('Password', { exact: true }).fill(who.password);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(heading(page, 'Channels')).toBeVisible();
}

/** A screen's own title, which is an `h1` and never repeated on the page. */
export function heading(page, name) {
  return page.getByRole('heading', { level: 1, name, exact: true });
}

/**
 * One settings section. Mantine gives each accordion panel `role="region"`
 * labelled by its control, so a section is addressable by the name the server
 * gave the group — and "Save" means *that* section's button rather than one of
 * the six on the page.
 */
export function settingsSection(page, name) {
  return page.getByRole('region', { name });
}
