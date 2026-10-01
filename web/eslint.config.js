import js from '@eslint/js';
import globals from 'globals';
import reactHooks from 'eslint-plugin-react-hooks';
import reactRefresh from 'eslint-plugin-react-refresh';
import prettier from 'eslint-config-prettier';

export default [
  {
    ignores: [
      'dist/**',
      'coverage/**',
      'node_modules/**',
      // Playwright's artefacts from a failed run, not source.
      'playwright-report/**',
      'test-results/**',
    ],
  },
  js.configs.recommended,
  reactHooks.configs.flat['recommended-latest'],
  {
    files: ['**/*.{js,jsx}'],
    languageOptions: {
      ecmaVersion: 2024,
      sourceType: 'module',
      globals: { ...globals.browser, ...globals.es2024 },
      parserOptions: { ecmaFeatures: { jsx: true } },
    },
    plugins: { 'react-refresh': reactRefresh },
    rules: {
      'react-refresh/only-export-components': ['warn', { allowConstantExport: true }],
      'no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
      eqeqeq: ['error', 'smart'],
      'no-console': ['warn', { allow: ['warn', 'error'] }],
    },
  },
  {
    files: ['**/*.test.{js,jsx}', 'vitest.setup.js'],
    languageOptions: { globals: { ...globals.vitest, ...globals.node } },
  },
  {
    files: ['vite.config.js', 'eslint.config.js', 'scripts/**/*.mjs'],
    languageOptions: { globals: globals.node },
  },
  {
    // The browser end-to-end layer runs in Node and reaches into the page
    // through `page.evaluate`, so both sets of globals are legitimate here.
    files: ['e2e/**/*.js', 'playwright.config.js'],
    languageOptions: { globals: { ...globals.browser, ...globals.node } },
  },
  prettier,
];
