import { useEffect, useRef, useState } from "react";
import { useWatch, watchOnce, type WatchState } from "./api";
import { empty, mergeContext, mergeWindow, type Held } from "./merge";
import type { Message } from "./types";

/** Rows read on each side of a message opened from search. */
export const CONTEXT = 50;

/**
 * Messages the launcher found and is opening: the page shows each at once
 * and reads only its context, rather than read it again first.
 */
const found = new Map<number, Message>();

export function seedFound(m: Message) {
  found.set(m.id, m);
}

/**
 * Where a message opened from search stands: being read, in view, or not
 * found under this page's filter.
 */
export type Seek = "none" | "loading" | "shown" | "missing" | "failed";

/**
 * A page's live messages under one feed request: every window merged into
 * what is held, with places messages may be missing marked. A message
 * `seek` names that the live window does not hold is read with its context
 * from the server and merged in too.
 */
export function useMessages(
  req: Record<string, unknown> | null,
  backlog: number,
  seek: number | null,
): { held: Held<Message>; loaded: boolean; state: WatchState; seek: Seek } {
  const [held, setHeld] = useState<Held<Message>>(empty);
  const [loaded, setLoaded] = useState(false);
  const [seekState, setSeekState] = useState<Seek>("none");
  const key = req === null ? null : JSON.stringify(req);
  const shownKey = useRef(key);
  if (shownKey.current !== key) {
    // Another filter is another thread.
    shownKey.current = key;
    setHeld(empty());
    setLoaded(false);
    setSeekState("none");
  }
  const watchReq = req === null ? null : { ...req, backlog };
  const state = useWatch<{ messages: Message[] }>("feed", watchReq, (snap) => {
    setHeld((h) => mergeWindow(h, snap.messages, backlog));
    setLoaded(true);
  });

  // Each filter or sought message is a generation; a late answer for an
  // earlier one is dropped.
  const generation = useRef(0);
  const asked = useRef<string | null>(null);
  const request = useRef(req);
  request.current = req;
  const ask = seek === null ? null : `${key}#${seek}`;
  useEffect(() => {
    generation.current++;
    asked.current = null;
    if (ask === null) setSeekState("none");
    // Gone or changed: whatever is still being read is no longer wanted.
    return () => {
      generation.current++;
    };
  }, [ask]);

  const holds = seek !== null && held.rows.some((m) => m.id === seek);
  useEffect(() => {
    if (seek === null || ask === null) return;
    if (holds) {
      setSeekState("shown");
      return;
    }
    if (!loaded || request.current === null || asked.current === ask) return;
    asked.current = ask;
    const mine = generation.current;
    const base = request.current;
    setSeekState("loading");
    const read = (extra: Record<string, unknown>) =>
      watchOnce<{ messages: Message[] }>("feed", { ...base, backlog: CONTEXT, ...extra });
    (async () => {
      const seed = found.get(seek);
      found.delete(seek);
      if (seed) {
        // Shown at once; what came before and after are read together.
        setHeld((h) => mergeContext(h, [seed], false));
        const [before, after] = await Promise.all([read({ until: seed.created_at }), read({ after_id: seek })]);
        if (generation.current !== mine) return;
        const complete = after.messages.length < CONTEXT;
        setHeld((h) => mergeContext(h, [...before.messages, ...after.messages], complete));
        return;
      }
      // Only its id: the hit and what follows, then what came before it.
      const after = await read({ after_id: seek - 1 });
      const hit = after.messages.find((m) => m.id === seek);
      if (!hit) {
        if (generation.current === mine) setSeekState("missing");
        return;
      }
      const before = await read({ until: hit.created_at });
      if (generation.current !== mine) return;
      const complete = after.messages.length < CONTEXT;
      setHeld((h) => mergeContext(h, [...before.messages, ...after.messages], complete));
    })().catch(() => {
      if (generation.current === mine) setSeekState("failed");
    });
  }, [seek, ask, holds, loaded]);

  return { held, loaded, state, seek: seekState };
}
