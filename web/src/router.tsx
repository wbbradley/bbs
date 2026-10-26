import { useSyncExternalStore, type MouseEvent, type ReactNode } from "react";
import { draftOwnerNow } from "./util";

// A small history router: the server answers every non-API path with the page.

const listeners = new Set<() => void>();

/** The address the page shows; Back and Forward return to it if refused. */
let shown = location.pathname + location.search;

// Back and Forward ask the leave guards too: the address goes back to the
// page still shown while they decide, and moves only if they all agree.
window.addEventListener("popstate", () => {
  const to = location.pathname + location.search;
  if (leaveGuards.size === 0) {
    shown = to;
    listeners.forEach((f) => f());
    return;
  }
  history.pushState(null, "", shown);
  void navigate(to);
});

function subscribe(f: () => void) {
  listeners.add(f);
  return () => listeners.delete(f);
}

/**
 * Things to settle before the page moves on, such as a changed note to
 * store; each resolves false to stay.
 */
const leaveGuards = new Set<() => boolean | Promise<boolean>>();

export function addLeaveGuard(guard: () => boolean | Promise<boolean>): () => void {
  leaveGuards.add(guard);
  return () => leaveGuards.delete(guard);
}

/** Move to `to` once every leave guard agrees; resolves whether it moved. */
export async function navigate(to: string, replace = false): Promise<boolean> {
  if (to === location.pathname + location.search) return true;
  // A guard that waits on the server may see the human sign out meanwhile;
  // then the page is no longer theirs to move.
  const owner = draftOwnerNow();
  for (const guard of [...leaveGuards]) {
    if (!(await guard())) return false;
    if (draftOwnerNow() !== owner) return false;
  }
  go(to, replace);
  return true;
}

/** Change the address now, with no guards: for this page's own state. */
export function go(to: string, replace = false) {
  if (to === location.pathname + location.search) return;
  if (replace) history.replaceState(null, "", to);
  else history.pushState(null, "", to);
  shown = to;
  listeners.forEach((f) => f());
}

export function usePath(): string {
  return useSyncExternalStore(subscribe, () => location.pathname);
}

export function useSearch(): URLSearchParams {
  const search = useSyncExternalStore(subscribe, () => location.search);
  return new URLSearchParams(search);
}

export function Link({
  to,
  className,
  children,
  title,
  onClick,
}: {
  to: string;
  className?: string;
  children: ReactNode;
  title?: string;
  onClick?: () => void;
}) {
  const click = (e: MouseEvent) => {
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
    e.preventDefault();
    onClick?.();
    navigate(to);
  };
  return (
    <a href={to} className={className} onClick={click} title={title}>
      {children}
    </a>
  );
}

export const paths = {
  agents: "/agents",
  agent: (name: string) => `/agents/${encodeURIComponent(name)}`,
  groups: "/groups",
  group: (name: string) => `/groups/${encodeURIComponent(name)}`,
  blockers: "/blockers",
  feed: "/feed",
  settings: "/settings",
};
