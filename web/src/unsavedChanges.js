import { create } from 'zustand';

/**
 * Whether a form on screen holds edits that have not been saved, and where
 * the user asked to go while it did.
 *
 * The router here is `<Routes>` rather than a data router, so there is no
 * `useBlocker`; links that leave a form ask this store instead. The form that
 * owns the edits publishes `dirty`, a link that finds it set parks its
 * destination in `pending`, and the form's own confirm takes it from there.
 */
export const useUnsavedChanges = create((set) => ({
  dirty: false,
  /** @type {string | null} */
  pending: null,
  setDirty: (dirty) => set({ dirty }),
  requestLeave: (pending) => set({ pending }),
  settle: () => set({ pending: null }),
}));

/**
 * The click handler for a link that would leave the current form. The click
 * goes through when nothing is unsaved, and when a modifier means it opens a
 * new tab, which leaves the form where it is.
 */
export function useLeaveGuard() {
  const dirty = useUnsavedChanges((state) => state.dirty);
  const requestLeave = useUnsavedChanges((state) => state.requestLeave);

  return (to) => (event) => {
    if (!dirty) return;
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    requestLeave(to);
  };
}
