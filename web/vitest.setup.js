import '@testing-library/jest-dom/vitest';
import { cleanup } from '@testing-library/react';
import { afterEach, vi } from 'vitest';

afterEach(() => {
  cleanup();
  localStorage.clear();
});

// Mantine reads both of these during layout and jsdom implements neither.
window.matchMedia ??= (query) => ({
  matches: false,
  media: query,
  onchange: null,
  addListener: () => {},
  removeListener: () => {},
  addEventListener: () => {},
  removeEventListener: () => {},
  dispatchEvent: () => false,
});

window.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
};

window.scrollTo ??= vi.fn();

// Mantine's Combobox scrolls the active option into view on open.
Element.prototype.scrollIntoView ??= vi.fn();
