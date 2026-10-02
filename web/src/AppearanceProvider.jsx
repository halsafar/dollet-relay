import { useMemo } from 'react';
import { MantineProvider } from '@mantine/core';

import { useAppearance } from './appearance.js';
import { buildTheme } from './theme.js';

/**
 * Mantine, themed from this browser's appearance choices. It re-themes the
 * moment a choice changes, which is why the Settings section has no Save.
 */
export function AppearanceProvider({ children }) {
  const textSize = useAppearance((state) => state.textSize);
  const contrast = useAppearance((state) => state.contrast);
  const theme = useMemo(() => buildTheme({ textSize, contrast }), [textSize, contrast]);

  return (
    <MantineProvider theme={theme} forceColorScheme="dark">
      {children}
    </MantineProvider>
  );
}
