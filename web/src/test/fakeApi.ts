// A stand-in for ../api in tests: each RPC call waits on a promise the test
// settles, and each watch is a stream the test feeds.
import { useEffect, useRef, useState } from "react";
import type { WatchState } from "../api";

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

export interface Call {
  method: string;
  req: Record<string, unknown>;
  resolve: (value: unknown) => void;
  reject: (error: unknown) => void;
}

export const calls: Call[] = [];
/** Answers given at once, by method, instead of waiting for the test. */
export const auto = new Map<string, (req: Record<string, unknown>) => unknown>();

export function rpc<T>(method: string, req: object = {}): Promise<T> {
  const answer = auto.get(method);
  if (answer) {
    try {
      return Promise.resolve(answer(req as Record<string, unknown>) as T);
    } catch (e) {
      return Promise.reject(e);
    }
  }
  return new Promise<T>((resolve, reject) => {
    calls.push({ method, req: req as Record<string, unknown>, resolve: resolve as (v: unknown) => void, reject });
  });
}

/** The pending calls to `method`, oldest first. */
export function pending(method: string): Call[] {
  return calls.filter((c) => c.method === method);
}

/** Take the oldest pending call to `method`. */
export function take(method: string): Call {
  const i = calls.findIndex((c) => c.method === method);
  if (i < 0) throw new Error(`no pending ${method}; have ${calls.map((c) => c.method).join(", ")}`);
  return calls.splice(i, 1)[0];
}

export interface Stream {
  kind: string;
  req: Record<string, unknown>;
  push: (snapshot: unknown, fresh?: boolean) => void;
  setState: (state: WatchState) => void;
  open: boolean;
}

export const streams: Stream[] = [];

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
    const stream: Stream = {
      kind,
      req: JSON.parse(key),
      open: true,
      setState,
      push: (snapshot, fresh = false) => {
        setState({ state: "live" });
        handler.current(snapshot as T, fresh);
      },
    };
    streams.push(stream);
    return () => {
      stream.open = false;
    };
  }, [kind, key]);
  return state;
}

/** The newest open stream of `kind`. */
export function stream(kind: string): Stream {
  const s = [...streams].reverse().find((x) => x.open && x.kind === kind);
  if (!s) throw new Error(`no open ${kind} stream`);
  return s;
}

const signedOut = new Set<() => void>();
export function onSignedOut(f: () => void): () => void {
  signedOut.add(f);
  return () => signedOut.delete(f);
}

/** A one-shot watch is a call the test answers, named `watch:<kind>`. */
export function watchOnce<T>(kind: string, req: object): Promise<T> {
  return rpc<T>(`watch:${kind}`, req);
}

export const login = (name: string, token: string) => rpc("login", { name, token });
export const logout = () => rpc("logout");
export const serverInfo = () => rpc("server_info");

export function reset() {
  calls.length = 0;
  streams.length = 0;
  auto.clear();
}
