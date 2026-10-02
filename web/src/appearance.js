import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export const STORAGE_KEY = 'dollet.appearance';

export const TEXT_SIZES = ['small', 'default', 'large'];
export const CONTRASTS = ['default', 'high'];

/**
 * How this browser draws the UI.
 *
 * Presentation, not configuration, so it lives in `localStorage` rather than in
 * the server's settings: the operator who wants larger text on a wall-mounted
 * screen should not hand it to every other browser signed in to the instance.
 */
export const useAppearance = create(
  persist(
    (set) => ({
      textSize: 'default',
      contrast: 'default',
      setTextSize: (textSize) => set({ textSize }),
      setContrast: (contrast) => set({ contrast }),
    }),
    {
      name: STORAGE_KEY,
      partialize: ({ textSize, contrast }) => ({ textSize, contrast }),
      // A stored value this build does not offer, from a hand edit or a later
      // version, falls back to the default rather than reaching the theme,
      // where an unknown text size would leave Mantine with no scale at all.
      merge: (stored, current) => ({
        ...current,
        textSize: TEXT_SIZES.includes(stored?.textSize)
          ? stored.textSize
          : current.textSize,
        contrast: CONTRASTS.includes(stored?.contrast)
          ? stored.contrast
          : current.contrast,
      }),
    },
  ),
);
