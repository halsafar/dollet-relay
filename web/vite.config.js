import { defineConfig } from 'vite';
import { configDefaults } from 'vitest/config';
import react from '@vitejs/plugin-react';

// Every path the Rust server owns is proxied so the dev server behaves like the
// production single-origin deployment: same-origin cookies, same relative URLs,
// and no CORS layer that only exists in development.
const BACKEND = 'http://127.0.0.1:9191';
const PROXIED = ['/api', '/output', '/hdhr', '/proxy'];

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      ...Object.fromEntries(
        PROXIED.map((path) => [path, { target: BACKEND, changeOrigin: true }]),
      ),
      '/ws': { target: BACKEND, ws: true, changeOrigin: true },
    },
  },
  build: {
    // The bundle is embedded in the binary by rust-embed, so size is resident
    // memory in a process whose whole point is a small footprint.
    chunkSizeWarningLimit: 900,
  },
  test: {
    environment: 'jsdom',
    // `e2e/` is Playwright's, and its specs match vitest's default pattern —
    // without this they are collected here and fail on the first import of
    // `@playwright/test`.
    exclude: [...configDefaults.exclude, 'e2e/**'],
    globals: true,
    setupFiles: ['./vitest.setup.js'],
    css: false,
    coverage: {
      provider: 'v8',
      reporter: ['text', 'json-summary', 'lcov'],
      reportsDirectory: './coverage',
      include: ['src/**/*.{js,jsx}'],
      exclude: ['src/main.jsx', 'src/test-utils.jsx', 'src/**/*.test.{js,jsx}'],
      // The ratchet: coverage may never decrease. Raise these when it rises,
      // never lower them to make a build pass.
      thresholds: {
        statements: 97,
        branches: 92,
        functions: 97,
        lines: 98,
      },
    },
  },
});
