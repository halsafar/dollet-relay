import { MantineProvider } from '@mantine/core';
import { MemoryRouter } from 'react-router-dom';
import { render } from '@testing-library/react';

import { theme } from './theme.js';

/**
 * Renders with the providers the app always has. Without the theme, Mantine
 * components fall back to defaults and assertions about them stop meaning
 * anything about the real UI.
 */
export function renderWithProviders(ui, { route = '/' } = {}) {
  return render(ui, {
    wrapper: ({ children }) => (
      <MantineProvider theme={theme} forceColorScheme="dark">
        <MemoryRouter initialEntries={[route]}>{children}</MemoryRouter>
      </MantineProvider>
    ),
  });
}
