import { beforeEach, describe, expect, it, vi } from 'vitest';

const STORAGE_KEY = 'dollet.appearance';

/**
 * The store reads localStorage when it is created, so every case about what a
 * reload restores needs a fresh module instance.
 */
async function loadStore() {
  vi.resetModules();
  return (await import('./appearance.js')).useAppearance;
}

function stored() {
  return JSON.parse(localStorage.getItem(STORAGE_KEY))?.state;
}

beforeEach(() => {
  localStorage.clear();
});

describe('the appearance store', () => {
  it('starts at the default text size and contrast', async () => {
    const store = await loadStore();

    expect(store.getState()).toMatchObject({ textSize: 'default', contrast: 'default' });
  });

  it('persists a choice under its own key', async () => {
    const store = await loadStore();
    store.getState().setTextSize('large');
    store.getState().setContrast('high');

    expect(stored()).toEqual({ textSize: 'large', contrast: 'high' });
  });

  it('restores the stored choices on the next load', async () => {
    const first = await loadStore();
    first.getState().setTextSize('small');
    first.getState().setContrast('high');

    const reloaded = await loadStore();

    expect(reloaded.getState()).toMatchObject({ textSize: 'small', contrast: 'high' });
  });

  it('falls back to the default for a value this build does not offer', async () => {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({ state: { textSize: 'huge', contrast: 'high' }, version: 0 }),
    );
    const store = await loadStore();

    // The valid half survives; the unknown half would have given the theme
    // no scale at all.
    expect(store.getState()).toMatchObject({ textSize: 'default', contrast: 'high' });
  });

  it('starts at the defaults rather than throwing on a corrupt entry', async () => {
    localStorage.setItem(STORAGE_KEY, '{{{');
    const store = await loadStore();

    expect(store.getState()).toMatchObject({ textSize: 'default', contrast: 'default' });
  });
});
