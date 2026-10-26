import { useEffect, useRef, useState } from "react";
import type { Principal, ServerInfo } from "./types";

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

/** Listeners told when the server no longer accepts this browser's sign-in. */
const signedOut = new Set<() => void>();
export function onSignedOut(f: () => void): () => void {
  signedOut.add(f);
  return () => signedOut.delete(f);
}

async function post<T>(path: string, body: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      method: "POST",
      headers: { "content-type": "application/json", "x-bbs-web": "1" },
      body: JSON.stringify(body ?? {}),
      credentials: "same-origin",
    });
  } catch {
    throw new ApiError("The server could not be reached.", 0);
  }
  if (res.status === 204) return undefined as T;
  const json = await res.json().catch(() => ({ error: res.statusText }));
  if (!res.ok) {
    if (res.status === 401 && path !== "/api/login") signedOut.forEach((f) => f());
    throw new ApiError(json.error ?? res.statusText, res.status);
  }
  return json as T;
}

/** A unary RPC by its snake_case name, with a proto-shaped JSON request. */
export function rpc<T = unknown>(method: string, req: object = {}): Promise<T> {
  return post<T>(`/api/rpc/${method}`, req);
}

export function login(name: string, token: string): Promise<Principal> {
  return post<Principal>("/api/login", { name, token });
}

export function logout(): Promise<void> {
  return post<void>("/api/logout", {});
}

export async function serverInfo(): Promise<ServerInfo | null> {
  try {
    const res = await fetch("/api/info");
    return res.ok ? ((await res.json()) as ServerInfo) : null;
  } catch {
    return null;
  }
}

export type WatchState =
  | { state: "connecting" }
  | { state: "live" }
  | { state: "retrying"; error: string }
  | { state: "failed"; error: string };

/** Watches one tab may hold open on its one event stream. */
export const MAX_WATCHES = 8;
/** How long the hub waits for the set of watches to settle before reopening. */
const SETTLE_MS = 100;

/** A short, stable key for a watch: its kind and request, hashed. */
export function watchKey(kind: string, q: string): string {
  // FNV-1a, twice over with different seeds, as 16 hex digits.
  const hash = (seed: number) => {
    let h = seed >>> 0;
    for (let i = 0; i < q.length; i++) {
      h ^= q.charCodeAt(i);
      h = Math.imul(h, 0x01000193) >>> 0;
    }
    return h.toString(16).padStart(8, "0");
  };
  return `${kind}:${hash(0x811c9dc5)}${hash(0x01000193)}`;
}

interface Watcher {
  onSnapshot: (snapshot: unknown, fresh: boolean) => void;
  onState: (state: WatchState) => void;
}

interface Watch {
  kind: string;
  q: unknown;
  watchers: Set<Watcher>;
  state: WatchState;
  /** The newest snapshot, for a view that joins a watch already live. */
  last?: unknown;
}

/**
 * Every watch the page holds, over one `/api/events` stream per tab. A
 * change to the set reopens the stream with the new set once it settles;
 * each key's first snapshot after an open is fresh. A failed key ends
 * alone: refused requests stay failed, and transient failures are asked
 * for again with backoff. The session ending signs out.
 */
const hub = (() => {
  const watches = new Map<string, Watch>();
  let source: EventSource | null = null;
  let settle: number | undefined;
  let retry: number | undefined;
  let delay = 1000;

  const tell = (w: Watch, state: WatchState) => {
    w.state = state;
    w.watchers.forEach((x) => x.onState(state));
  };

  const close = () => {
    source?.close();
    source = null;
  };

  const open = () => {
    close();
    window.clearTimeout(retry);
    const active = [...watches.entries()].filter(([, w]) => w.state.state !== "failed");
    for (const [, w] of active.slice(MAX_WATCHES)) tell(w, { state: "failed", error: "too many live views in this tab" });
    const subscribed = active.slice(0, MAX_WATCHES);
    if (subscribed.length === 0) return;
    const fresh = new Set(subscribed.map(([key]) => key));
    const s = JSON.stringify(subscribed.map(([key, w]) => ({ key, kind: w.kind, q: w.q })));
    const es = new EventSource(`/api/events?s=${encodeURIComponent(s)}`);
    source = es;
    es.onmessage = (e) => {
      const { key, snapshot } = JSON.parse(e.data) as { key: string; snapshot: unknown };
      const w = watches.get(key);
      if (!w) return;
      delay = 1000;
      const first = fresh.delete(key);
      w.last = snapshot;
      tell(w, { state: "live" });
      w.watchers.forEach((x) => x.onSnapshot(snapshot, first));
    };
    es.addEventListener("fail", (e) => {
      const { key, error, status } = JSON.parse((e as MessageEvent).data) as {
        key: string | null;
        error: string;
        status: number;
      };
      if (key === null) {
        close();
        if (status === 401) signedOut.forEach((f) => f());
        else again(error);
        return;
      }
      const w = watches.get(key);
      if (!w) return;
      if (status >= 500 || status === 429) {
        tell(w, { state: "retrying", error });
        later();
      } else {
        tell(w, { state: "failed", error });
      }
    });
    es.onerror = () => {
      if (source !== es) return;
      close();
      again("connection lost");
    };
  };

  // The stream dropped: every watch waits, and the stream is asked for again.
  const again = async (error: string) => {
    for (const w of watches.values()) if (w.state.state !== "failed") tell(w, { state: "retrying", error });
    // A refused cookie looks like a dropped stream to EventSource; ask.
    try {
      await rpc("whoami");
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return;
    }
    later();
  };

  const later = () => {
    window.clearTimeout(retry);
    retry = window.setTimeout(open, delay);
    delay = Math.min(delay * 2, 30000);
  };

  const changed = () => {
    window.clearTimeout(settle);
    settle = window.setTimeout(open, SETTLE_MS);
  };

  return {
    add(kind: string, q: unknown, watcher: Watcher): () => void {
      const key = watchKey(kind, JSON.stringify(q));
      let w = watches.get(key);
      if (!w) {
        w = { kind, q, watchers: new Set(), state: { state: "connecting" } };
        watches.set(key, w);
        changed();
      }
      w.watchers.add(watcher);
      watcher.onState(w.state);
      // A view joining a live watch starts from what it last sent.
      if (w.last !== undefined) watcher.onSnapshot(w.last, true);
      return () => {
        w!.watchers.delete(watcher);
        if (w!.watchers.size === 0) {
          watches.delete(key);
          changed();
        }
      };
    },
  };
})();

/**
 * Follow a server watch stream (`feed`, `activity` or `summary`). Each
 * snapshot goes to `onSnapshot`, marked fresh when it is the first since
 * the stream (re)opened. All of a tab's watches share one event stream.
 */
export function useWatch<T>(
  kind: string,
  req: object | null,
  onSnapshot: (snapshot: T, fresh: boolean) => void,
): WatchState {
  const [state, setState] = useState<WatchState>({ state: "connecting" });
  const handler = useRef(onSnapshot);
  handler.current = onSnapshot;
  const key = req === null ? null : JSON.stringify(req);

  useEffect(() => {
    if (key === null) return;
    setState({ state: "connecting" });
    return hub.add(kind, JSON.parse(key), {
      onSnapshot: (snapshot, fresh) => handler.current(snapshot as T, fresh),
      onState: setState,
    });
  }, [kind, key]);

  return state;
}

/**
 * The first snapshot of a watch, then the watch is closed: rows read once,
 * such as the context around one message. It uses its own short event
 * stream, so the page's live one is not reopened for it.
 */
export function watchOnce<T>(kind: string, req: object, timeoutMs = 15000): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const key = "once";
    const s = JSON.stringify([{ key, kind, q: req }]);
    const source = new EventSource(`/api/events?s=${encodeURIComponent(s)}`);
    const done = (f: () => void) => {
      window.clearTimeout(timer);
      source.close();
      f();
    };
    const timer = window.setTimeout(() => done(() => reject(new ApiError("timed out", 0))), timeoutMs);
    source.onmessage = (e) => done(() => resolve((JSON.parse(e.data) as { snapshot: T }).snapshot));
    source.addEventListener("fail", (e) => {
      const { error, status } = JSON.parse((e as MessageEvent).data);
      done(() => {
        if (status === 401) signedOut.forEach((f) => f());
        reject(new ApiError(error, status));
      });
    });
    source.onerror = () => done(() => reject(new ApiError("connection lost", 0)));
  });
}
