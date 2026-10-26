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

/**
 * This tab's id: its next event stream replaces its last one at once, so a
 * reopen never waits for the server to notice the old one closed.
 */
const TAB = Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, "0")).join("");

interface Watcher {
  onSnapshot: (snapshot: unknown, fresh: boolean) => void;
  onState: (state: WatchState) => void;
}

interface Watch {
  kind: string;
  q: unknown;
  /**
   * The request to reopen with, once the watch has been open: one that
   * carries on from what is held (activity after its newest record), whose
   * first snapshot then continues rather than starts afresh.
   */
  resume?: () => unknown | null;
  opened?: boolean;
  watchers: Set<Watcher>;
  state: WatchState;
  /**
   * The newest snapshot and the stream it came from, for a view that joins
   * a watch live on this stream; one from an earlier stream is never
   * replayed as if current.
   */
  last?: { snapshot: unknown; stream: number };
  /** Refused only for want of room on the stream: asked for again later. */
  crowded?: boolean;
}

/**
 * Every watch the page holds, over one `/api/events` stream per tab. A
 * change to the set reopens the stream with the new set once it settles;
 * each key's first snapshot after an open is fresh. A failed key ends
 * alone: refused requests stay failed, and transient failures are asked
 * for again with backoff. The session ending signs out.
 */
const hub = (() => {
  // By key: an opaque id given each distinct request as it is first watched.
  const watches = new Map<string, Watch>();
  const keys = new Map<string, string>();
  let nextKey = 0;
  // Each stream opened, so events from one already closed change nothing.
  let stream = 0;
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
    const active = [...watches.entries()].filter(([, w]) => w.state.state !== "failed" || w.crowded);
    for (const [, w] of active.slice(MAX_WATCHES)) {
      w.crowded = true;
      tell(w, { state: "failed", error: "too many live views in this tab" });
    }
    const subscribed = active.slice(0, MAX_WATCHES);
    for (const [, w] of subscribed) {
      if (!w.crowded) continue;
      w.crowded = false;
      tell(w, { state: "connecting" });
    }
    if (subscribed.length === 0) return;
    const fresh = new Set<string>();
    const subs = subscribed.map(([key, w]) => {
      const resumed = w.opened ? (w.resume?.() ?? null) : null;
      if (resumed === null) fresh.add(key);
      w.opened = true;
      return { key, kind: w.kind, q: resumed ?? w.q };
    });
    const s = JSON.stringify(subs);
    const es = new EventSource(`/api/events?tab=${TAB}&s=${encodeURIComponent(s)}`);
    source = es;
    const mine = ++stream;
    // Events still queued from a stream this tab has closed are dropped.
    const current = () => source === es && stream === mine;
    es.onmessage = (e) => {
      if (!current()) return;
      const { key, snapshot } = JSON.parse(e.data) as { key: string; snapshot: unknown };
      const w = watches.get(key);
      if (!w) return;
      delay = 1000;
      const first = fresh.delete(key);
      w.last = { snapshot, stream: mine };
      tell(w, { state: "live" });
      w.watchers.forEach((x) => x.onSnapshot(snapshot, first));
    };
    es.addEventListener("fail", (e) => {
      if (!current()) return;
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
      if (!current()) return;
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

  // Signed out: the stream closes, nothing held is shown again, and the
  // next sign-in starts afresh, without the old backoff.
  signedOut.add(() => {
    close();
    stream++;
    window.clearTimeout(retry);
    delay = 1000;
    for (const w of watches.values()) {
      w.last = undefined;
      w.opened = false;
    }
  });

  return {
    add(kind: string, q: unknown, watcher: Watcher, resume?: () => unknown | null): () => void {
      const request = `${kind}\n${JSON.stringify(q)}`;
      let key = keys.get(request);
      let w = key === undefined ? undefined : watches.get(key);
      if (key === undefined || !w) {
        key = `w${++nextKey}`;
        keys.set(request, key);
        w = { kind, q, resume, watchers: new Set(), state: { state: "connecting" } };
        watches.set(key, w);
        changed();
      }
      const k = key;
      const held = w;
      held.watchers.add(watcher);
      watcher.onState(held.state);
      // A view joining a watch live on this stream starts from what it last
      // sent; anything older waits for the stream's own first snapshot.
      if (held.last && held.last.stream === stream && held.state.state === "live") {
        watcher.onSnapshot(held.last.snapshot, true);
      }
      return () => {
        held.watchers.delete(watcher);
        if (held.watchers.size === 0 && watches.get(k) === held) {
          watches.delete(k);
          keys.delete(request);
          changed();
        }
      };
    },
    /** The key a request is watched under now, for tests. */
    keyOf(kind: string, q: unknown): string | undefined {
      return keys.get(`${kind}\n${JSON.stringify(q)}`);
    },
  };
})();

/**
 * Follow a server watch stream (`feed`, `activity` or `summary`). Each
 * snapshot goes to `onSnapshot`, marked fresh when it is the first since
 * the stream (re)opened. All of a tab's watches share one event stream.
 */
/** The key `kind` and `q` are watched under on this tab's stream, if any. */
export function watchKey(kind: string, q: unknown): string | undefined {
  return hub.keyOf(kind, q);
}

export function useWatch<T>(
  kind: string,
  req: object | null,
  onSnapshot: (snapshot: T, fresh: boolean) => void,
  /** The request to carry on with after a reopen, or null to start afresh. */
  resume?: () => object | null,
): WatchState {
  const [state, setState] = useState<WatchState>({ state: "connecting" });
  const handler = useRef(onSnapshot);
  handler.current = onSnapshot;
  const carryOn = useRef(resume);
  carryOn.current = resume;
  const key = req === null ? null : JSON.stringify(req);

  useEffect(() => {
    if (key === null) return;
    setState({ state: "connecting" });
    return hub.add(
      kind,
      JSON.parse(key),
      {
        onSnapshot: (snapshot, fresh) => handler.current(snapshot as T, fresh),
        onState: setState,
      },
      () => carryOn.current?.() ?? null,
    );
  }, [kind, key]);

  return state;
}

/**
 * The first snapshot of a watch, then the watch is closed: rows read once,
 * such as the context around one message. It rides the page's one event
 * stream for as long as it takes.
 */
export function watchOnce<T>(kind: string, req: object, timeoutMs = 15000): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    let stop = () => {};
    const done = (f: () => void) => {
      window.clearTimeout(timer);
      stop();
      f();
    };
    const timer = window.setTimeout(() => done(() => reject(new ApiError("timed out", 0))), timeoutMs);
    let settled = false;
    stop = hub.add(kind, req, {
      onSnapshot: (snapshot) => {
        if (settled) return;
        settled = true;
        // Leave the stream after this call returns, not while it delivers.
        queueMicrotask(() => done(() => resolve(snapshot as T)));
      },
      onState: (state) => {
        if (settled || state.state !== "failed") return;
        settled = true;
        queueMicrotask(() => done(() => reject(new ApiError(state.error, 400))));
      },
    });
  });
}
