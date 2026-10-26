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

/**
 * Follow a server watch stream (`feed`, `activity` or `summary`). Each
 * snapshot goes to `onSnapshot`. A dropped or failed stream reopens with
 * backoff from one to thirty seconds; a refused sign-in signs out instead,
 * and a refused request (bad filter, missing capability) stops.
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
    let source: EventSource | null = null;
    let timer: number | undefined;
    let delay = 1000;
    let closed = false;
    setState({ state: "connecting" });

    const open = () => {
      // Each connection's first snapshot replaces what the last one showed.
      let fresh = true;
      source = new EventSource(`/api/watch/${kind}?q=${encodeURIComponent(key)}`);
      source.onmessage = (e) => {
        delay = 1000;
        setState({ state: "live" });
        handler.current(JSON.parse(e.data) as T, fresh);
        fresh = false;
      };
      source.addEventListener("fail", (e) => {
        const { error, status } = JSON.parse((e as MessageEvent).data);
        source?.close();
        if (status === 401) {
          signedOut.forEach((f) => f());
          return;
        }
        if (status >= 500 || status === 429) retry(error);
        else setState({ state: "failed", error });
      });
      source.onerror = () => {
        source?.close();
        retry("connection lost");
      };
    };
    const retry = async (error: string) => {
      if (closed) return;
      setState({ state: "retrying", error });
      // A refused cookie looks like a dropped stream to EventSource; ask.
      try {
        await rpc("whoami");
      } catch (e) {
        if (e instanceof ApiError && e.status === 401) return;
      }
      timer = window.setTimeout(() => !closed && open(), delay);
      delay = Math.min(delay * 2, 30000);
    };
    open();
    return () => {
      closed = true;
      source?.close();
      window.clearTimeout(timer);
    };
  }, [kind, key]);

  return state;
}
