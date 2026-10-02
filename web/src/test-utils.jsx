import { MemoryRouter } from 'react-router-dom';
import { render } from '@testing-library/react';

import { AppearanceProvider } from './AppearanceProvider.jsx';

/**
 * Renders with the providers the app always has. Without the theme, Mantine
 * components fall back to defaults and assertions about them stop meaning
 * anything about the real UI.
 */
export function renderWithProviders(ui, { route = '/' } = {}) {
  return render(ui, {
    wrapper: ({ children }) => (
      <AppearanceProvider>
        <MemoryRouter initialEntries={[route]}>{children}</MemoryRouter>
      </AppearanceProvider>
    ),
  });
}
