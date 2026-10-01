import { useState } from 'react';
import { ImageOff } from 'lucide-react';

import classes from './LogoImage.module.css';

/**
 * Provider-supplied artwork, rendered defensively.
 *
 * These URLs point at hosts nobody here controls, and a page of them is dozens
 * of requests to dozens of origins. Three things matter:
 *
 * - `onError` per image, so one dead host leaves one placeholder rather than a
 *   hole, and never blocks the rest of the grid.
 * - `referrerPolicy`, so a provider's server is not told which instance is
 *   asking or which page it was on.
 * - `loading="lazy"`, so a table of hundreds does not open hundreds of
 *   connections before the user has scrolled.
 *
 * @param {{ src: string | null, alt?: string, size?: number }} props
 */
export function LogoImage({ src, alt = '', size = 28 }) {
  // The failing URL, not a boolean: a row recycled onto a different logo would
  // otherwise inherit the previous one's failure and show a placeholder over
  // artwork that loads perfectly well.
  const [failedSrc, setFailedSrc] = useState(null);
  const failed = failedSrc !== null && failedSrc === src;

  if (!src || failed) {
    return (
      <span
        className={classes.placeholder}
        style={{ height: size, width: size * 2 }}
        title={src ? 'Artwork could not be loaded' : 'No artwork'}
        role="img"
        aria-label={src ? `${alt} (artwork unavailable)` : 'No artwork'}
      >
        <ImageOff size={Math.min(size - 8, 16)} />
      </span>
    );
  }

  return (
    <img
      className={classes.image}
      style={{ maxHeight: size }}
      src={src}
      alt={alt}
      loading="lazy"
      referrerPolicy="no-referrer"
      onError={() => setFailedSrc(src)}
    />
  );
}
