import { useSyncExternalStore, type MouseEvent, type ReactNode } from "react";
import { draftOwnerNow } from "./util";

// A small history router: the server answers every non-API path with the page.

const listeners = new Set<() => void>();
const notify = () => listeners.forEach((f) => f());

// Each history entry knows its place, so Back and Forward can be undone
// and redone exactly, never by adding entries.
const place = (): number => {
  const idx = (history.state as { idx?: unknown } | null)?.idx;
  return typeof idx === "number" ? idx : 0;
};
if (typeof (history.state as { idx?: unknown } | null)?.idx !== "number") {
  history.replaceState({ idx: 0 }, "");
}
/** The place of the entry the page shows. */
let index = place();

/**
 * Back or Forward the leave guards must agree to: first the browser is
 * sent back to the entry shown ("undoing"), then, if they agree, forward
 * by the same steps again ("redoing"), which the page then follows. A
 * move made while they decide is undone and dropped.
 */
let traversal: { phase: "undoing" | "deciding" | "redoing"; delta: number } | null = null;

window.addEventListener("popstate", () => {
  const at = place();
  if (traversal?.phase === "undoing") {
    traversal.phase = "deciding";
    const { delta } = traversal;
    void (async () => {
      const ok = await agreeToLeave();
      if (ok && traversal) {
        traversal.phase = "redoing";
        history.go(delta);
      } else {
        traversal = null;
      }
    })();
    return;
  }
  if (traversal?.phase === "redoing") {
    traversal = null;
    index = at;
    notify();
    return;
  }
  if (traversal?.phase === "deciding") {
    // Another step while the guards decide: undone, not followed.
    if (at !== index) history.go(index - at);
    return;
  }
  const delta = at - index;
  if (delta === 0 || leaveGuards.size === 0) {
    index = at;
    notify();
    return;
  }
  traversal = { phase: "undoing", delta };
  history.go(-delta);
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

/**
 * Whether every leave guard agrees, in turn. A guard that waits on the
 * server may see the human sign out meanwhile; then the page is no longer
 * theirs to move.
 */
async function agreeToLeave(): Promise<boolean> {
  const owner = draftOwnerNow();
  for (const guard of [...leaveGuards]) {
    if (!(await guard())) return false;
    if (draftOwnerNow() !== owner) return false;
  }
  return true;
}

/** Move to `to` once every leave guard agrees; resolves whether it moved. */
export async function navigate(to: string, replace = false): Promise<boolean> {
  if (to === location.pathname + location.search) return true;
  if (!(await agreeToLeave())) return false;
  go(to, replace);
  return true;
}

/** Change the address now, with no guards: for this page's own state. */
export function go(to: string, replace = false) {
  if (to === location.pathname + location.search) return;
  if (replace) history.replaceState({ idx: index }, "", to);
  else history.pushState({ idx: ++index }, "", to);
  notify();
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
