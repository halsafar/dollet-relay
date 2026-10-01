import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * Loads a resource on mount and exposes `reload` for after a mutation.
 *
 * Every list screen needs exactly this, and doing it inline means repeating
 * three pieces of state and the stale-response guard on each one.
 *
 * @template T
 * @param {() => Promise<T>} loader Must be stable — wrap it in `useCallback`.
 * @param {T} [initial]
 */
export function useResource(loader, initial = null) {
  const [data, setData] = useState(initial);
  const [error, setError] = useState(null);
  const [loading, setLoading] = useState(true);
  const generation = useRef(0);

  const reload = useCallback(async () => {
    const mine = ++generation.current;
    // Every load, not just the first. Without this the spinner appears once on
    // mount and a later refetch looks like nothing is happening.
    setLoading(true);
    try {
      const next = await loader();
      // A slower earlier request must not overwrite a newer result.
      if (mine !== generation.current) return;
      setData(next);
      setError(null);
    } catch (failure) {
      if (mine === generation.current) setError(failure);
    } finally {
      if (mine === generation.current) setLoading(false);
    }
  }, [loader]);

  // Fetching on mount necessarily means setting state from an effect. The rule
  // exists to push that into a data-fetching library; there is deliberately no
  // such dependency here, so it is suppressed once, here, rather than per page.
  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void reload();
  }, [reload]);

  return { data, error, loading, reload, setData };
}
