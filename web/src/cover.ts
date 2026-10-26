import { useLayoutEffect, useSyncExternalStore } from "react";

// Overlays that hide the page beneath them: dialogs, the launcher, drawers
// over a timeline. Nothing beneath is read or seen while any is open.

let open = 0;
const listeners = new Set<() => void>();

function subscribe(f: () => void) {
  listeners.add(f);
  return () => listeners.delete(f);
}

/** Whether any overlay covers the page at this moment. */
export function isCovered(): boolean {
  return open > 0;
}

/** Whether any overlay covers the page now, re-rendering as that changes. */
export function useCovered(): boolean {
  return useSyncExternalStore(subscribe, () => open > 0);
}

/**
 * Count this component as an overlay while it is mounted and `active`,
 * from before anything beneath it can run its effects.
 */
export function useCover(active = true) {
  useLayoutEffect(() => {
    if (!active) return;
    open++;
    listeners.forEach((f) => f());
    return () => {
      open--;
      listeners.forEach((f) => f());
    };
  }, [active]);
}
