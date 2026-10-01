import { beforeEach, describe, expect, it, vi } from 'vitest';
import { act, renderHook, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { BaseUrlPicker } from './BaseUrlPicker.jsx';
import {
  BASE_URL_SOURCES,
  STORAGE_KEY,
  normalizeBaseUrl,
  useBaseUrl,
  useBaseUrlStore,
} from './baseUrl.js';
import { renderWithProviders } from '../test-utils.jsx';

/**
 * jsdom serves the app from here, so this is what "This browser" resolves to
 * and what every assertion below compares against.
 */
const ORIGIN = 'http://localhost:3000';

/**
 * The store is module state shared by the Connect page, the Channels header and
 * the channel editor — which is the point of it — so each test starts it over.
 */
function reset() {
  useBaseUrlStore.setState({
    source: BASE_URL_SOURCES.browser,
    custom: '',
    advertised: null,
  });
}

beforeEach(reset);

describe('normalizeBaseUrl', () => {
  it('strips a trailing slash, so nothing builds a URL with two', () => {
    expect(normalizeBaseUrl('http://dollet-relay:9191/')).toBe(
      'http://dollet-relay:9191',
    );
    expect(normalizeBaseUrl('  http://dollet-relay:9191  ')).toBe(
      'http://dollet-relay:9191',
    );
  });

  it('normalises case and a redundant default port', () => {
    // Otherwise `HTTP://Host:80` and `http://host` read as two addresses in a
    // list whose whole job is telling addresses apart.
    expect(normalizeBaseUrl('HTTP://Host:80/')).toBe('http://host');
  });

  it('refuses anything that is not an absolute http(s) URL', () => {
    for (const bad of ['', '   ', 'dollet-relay:9191', '/hdhr', 'ftp://host']) {
      expect(normalizeBaseUrl(bad)).toBeNull();
    }
  });

  it('treats a missing value as no answer rather than as the string "null"', () => {
    expect(normalizeBaseUrl(null)).toBeNull();
    expect(normalizeBaseUrl(undefined)).toBeNull();
  });
});

describe('useBaseUrl', () => {
  it('is this browser by default', () => {
    const { result } = renderHook(() => useBaseUrl());
    expect(result.current).toBe(ORIGIN);
  });

  it('follows the advertised base when that source is chosen', () => {
    act(() => {
      useBaseUrlStore.setState({
        advertised: 'https://tv.example/',
        source: BASE_URL_SOURCES.advertised,
      });
    });
    const { result } = renderHook(() => useBaseUrl());
    expect(result.current).toBe('https://tv.example');
  });

  it('falls back to this browser when the advertised source has nothing behind it', () => {
    // Only reachable if the deployment drops `DOLLET_ADVERTISED_BASE_URL` while
    // a page is open, but the alternative is every URL reading "null/hdhr/".
    act(() => {
      useBaseUrlStore.setState({
        source: BASE_URL_SOURCES.advertised,
        advertised: null,
      });
    });
    const { result } = renderHook(() => useBaseUrl());
    expect(result.current).toBe(ORIGIN);
  });

  it('falls back to this browser while a custom base is half-typed', () => {
    // The page keeps showing working URLs; the picker reports the problem
    // inline, where it was made.
    act(() => {
      useBaseUrlStore.setState({
        source: BASE_URL_SOURCES.custom,
        custom: 'http://',
      });
    });
    const { result } = renderHook(() => useBaseUrl());
    expect(result.current).toBe(ORIGIN);
  });
});

describe('the picker', () => {
  it('offers the advertised source only when the deployment has one', async () => {
    const user = userEvent.setup();
    renderWithProviders(<BaseUrlPicker />);

    expect(screen.getByRole('radio', { name: 'This browser' })).toBeChecked();
    expect(screen.queryByRole('radio', { name: 'Advertised' })).not.toBeInTheDocument();

    act(() => useBaseUrlStore.getState().setAdvertised('https://tv.example'));
    await user.click(screen.getByRole('radio', { name: 'Advertised' }));

    expect(useBaseUrlStore.getState().source).toBe(BASE_URL_SOURCES.advertised);
    expect(screen.getByText(/https:\/\/tv\.example/)).toBeInTheDocument();
  });

  it('remembers a custom base in localStorage, because it is this operator’s view', async () => {
    const user = userEvent.setup();
    renderWithProviders(<BaseUrlPicker />);

    await user.click(screen.getByRole('radio', { name: 'Custom' }));
    await user.type(screen.getByLabelText('Custom base URL'), 'http://dollet-relay:9191');

    expect(localStorage.getItem(STORAGE_KEY)).toBe('http://dollet-relay:9191');
  });

  it('restores a remembered custom base as the selection, not just as text', async () => {
    localStorage.setItem(STORAGE_KEY, 'http://dollet-relay:9191');
    // Storage is read once, when the store is created, so this is the one
    // assertion that needs the module evaluated again.
    vi.resetModules();
    const fresh = await import('./baseUrl.js');

    expect(fresh.useBaseUrlStore.getState().source).toBe('custom');
    expect(fresh.resolveBaseUrl(fresh.useBaseUrlStore.getState(), ORIGIN)).toBe(
      'http://dollet-relay:9191',
    );
  });

  it('survives a browser that refuses storage, at the cost of a retype', async () => {
    // Private-mode Safari and a locked-down profile both throw here. Losing
    // the remembered address is acceptable; a page that will not render is not.
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('storage is disabled');
    });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('storage is disabled');
    });

    vi.resetModules();
    const fresh = await import('./baseUrl.js');
    expect(fresh.useBaseUrlStore.getState().custom).toBe('');

    expect(() =>
      fresh.useBaseUrlStore.getState().setCustom('http://dollet-relay:9191'),
    ).not.toThrow();
    expect(fresh.useBaseUrlStore.getState().custom).toBe('http://dollet-relay:9191');

    vi.restoreAllMocks();
  });

  it('forgets a custom base that is cleared', async () => {
    const user = userEvent.setup();
    localStorage.setItem(STORAGE_KEY, 'http://dollet-relay:9191');
    useBaseUrlStore.setState({
      source: BASE_URL_SOURCES.custom,
      custom: 'http://dollet-relay:9191',
    });
    renderWithProviders(<BaseUrlPicker />);

    await user.clear(screen.getByLabelText('Custom base URL'));

    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('shows an inline error for a base that will not parse', async () => {
    const user = userEvent.setup();
    renderWithProviders(<BaseUrlPicker />);

    await user.click(screen.getByRole('radio', { name: 'Custom' }));
    await user.type(screen.getByLabelText('Custom base URL'), 'dollet-relay:9191');

    expect(
      screen.getByText('Enter an absolute http:// or https:// URL.'),
    ).toBeInTheDocument();
  });

  it('says nothing about an empty field, which is not yet a mistake', async () => {
    const user = userEvent.setup();
    renderWithProviders(<BaseUrlPicker />);

    await user.click(screen.getByRole('radio', { name: 'Custom' }));

    expect(
      screen.queryByText('Enter an absolute http:// or https:// URL.'),
    ).not.toBeInTheDocument();
  });
});
