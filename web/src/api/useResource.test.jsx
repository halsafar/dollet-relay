import { describe, expect, it, vi } from 'vitest';
import { act, renderHook, waitFor } from '@testing-library/react';

import { useResource } from './useResource.js';
import { ApiError } from './errors.js';

describe('useResource', () => {
  it('loads on mount and exposes the result', async () => {
    const loader = vi.fn().mockResolvedValue([{ id: 1 }]);
    const { result } = renderHook(() => useResource(loader, []));

    expect(result.current.loading).toBe(true);
    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.data).toEqual([{ id: 1 }]);
    expect(result.current.error).toBeNull();
  });

  it('keeps the initial value and records the failure', async () => {
    const failure = new ApiError('Not found.', { status: 404 });
    const { result } = renderHook(() =>
      useResource(vi.fn().mockRejectedValue(failure), []),
    );

    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.error).toBe(failure);
    expect(result.current.data).toEqual([]);
  });

  it('clears a previous error on a successful reload', async () => {
    const loader = vi
      .fn()
      .mockRejectedValueOnce(new ApiError('Boom', { status: 500 }))
      .mockResolvedValueOnce(['ok']);

    const { result } = renderHook(() => useResource(loader, []));
    await waitFor(() => expect(result.current.error).toBeTruthy());

    await act(async () => {
      await result.current.reload();
    });

    expect(result.current.error).toBeNull();
    expect(result.current.data).toEqual(['ok']);
  });

  it('ignores a slow earlier response that resolves after a newer one', async () => {
    const resolvers = [];
    const loader = vi.fn(() => new Promise((resolve) => resolvers.push(resolve)));

    const { result } = renderHook(() => useResource(loader, []));
    await waitFor(() => expect(resolvers).toHaveLength(1));

    await act(async () => {
      void result.current.reload();
      await waitFor(() => expect(resolvers).toHaveLength(2));
      resolvers[1]('newer');
      resolvers[0]('older');
    });

    expect(result.current.data).toBe('newer');
  });

  it('lets a caller patch the data without a round trip', async () => {
    const { result } = renderHook(() => useResource(vi.fn().mockResolvedValue([1]), []));
    await waitFor(() => expect(result.current.loading).toBe(false));

    act(() => result.current.setData([1, 2]));

    expect(result.current.data).toEqual([1, 2]);
  });
});
