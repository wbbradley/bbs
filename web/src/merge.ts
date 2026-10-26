// Windows of a stream merged into what the page already holds, so a
// reconnect or a new window never throws history away, and a place where
// messages may be missing is marked rather than closed up.

/** Rows kept per page; the oldest go first. */
export const KEEP = 3000;

export interface Held<T> {
  rows: T[];
  /** Ids of rows that something may be missing before. */
  gaps: Set<number>;
}

export function empty<T>(): Held<T> {
  return { rows: [], gaps: new Set() };
}

/**
 * Merge `window`, the newest rows a stream sent (at most `limit` of them),
 * into `held`, oldest first by id. Newer copies replace older ones. When a
 * full window starts past everything held, rows between may never have
 * arrived: that place is marked.
 */
export function mergeWindow<T extends { id: number }>(held: Held<T>, window: T[], limit: number): Held<T> {
  if (window.length === 0) return held;
  const byId = new Map(held.rows.map((r) => [r.id, r]));
  const newestHeld = held.rows.length > 0 ? held.rows[held.rows.length - 1].id : null;
  const oldest = Math.min(...window.map((r) => r.id));
  const gaps = new Set(held.gaps);
  if (newestHeld !== null && window.length >= limit && oldest > newestHeld) gaps.add(oldest);
  for (const r of window) byId.set(r.id, r);
  let rows = [...byId.values()].sort((a, b) => a.id - b.id);
  if (rows.length > KEEP) rows = rows.slice(rows.length - KEEP);
  const first = rows.length > 0 ? rows[0].id : 0;
  for (const g of gaps) if (g <= first) gaps.delete(g);
  return { rows, gaps };
}

/**
 * Add rows read around one message (its context) to `held`. When the
 * context ends before the rows held begin and more may lie between
 * (`complete` false), the first held row is marked.
 */
export function mergeContext<T extends { id: number }>(held: Held<T>, context: T[], complete: boolean): Held<T> {
  if (context.length === 0) return held;
  const firstHeld = held.rows.length > 0 ? held.rows[0].id : null;
  const merged = mergeWindow(held, context, Infinity);
  const newestContext = Math.max(...context.map((r) => r.id));
  if (!complete && firstHeld !== null && newestContext < firstHeld) merged.gaps.add(firstHeld);
  return merged;
}
