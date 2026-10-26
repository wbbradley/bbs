// Windows of a stream merged into what the page already holds, so a
// reconnect or a new window never throws history away, and a place where
// messages may be missing is marked rather than closed up.

/** Rows kept per page; the oldest go first, except those kept on purpose. */
export const KEEP = 3000;

export interface Held<T> {
  rows: T[];
  /** Ids of rows that something may be missing before. */
  gaps: Set<number>;
  /**
   * Rows read on purpose, such as a found message and its context: never
   * trimmed for the cap, however old.
   */
  kept: Set<number>;
}

export function empty<T>(): Held<T> {
  return { rows: [], gaps: new Set(), kept: new Set() };
}

/**
 * The newest rows within the cap, plus every kept row; where trimming
 * opened a hole after a kept row, it is marked.
 */
function trim<T extends { id: number }>(rows: T[], gaps: Set<number>, kept: Set<number>): Held<T> {
  const over = rows.length - KEEP;
  if (over > 0) {
    const out: T[] = [];
    let dropped = 0;
    let trimmedSince = false;
    for (const r of rows) {
      if (dropped < over && !kept.has(r.id)) {
        dropped++;
        trimmedSince = true;
        continue;
      }
      // The first row after rows dropped behind a kept one.
      if (trimmedSince && out.length > 0) gaps.add(r.id);
      trimmedSince = false;
      out.push(r);
    }
    rows = out;
  }
  const first = rows.length > 0 ? rows[0].id : 0;
  for (const g of gaps) if (g <= first) gaps.delete(g);
  return { rows, gaps, kept };
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
  return trim(
    [...byId.values()].sort((a, b) => a.id - b.id),
    gaps,
    new Set(held.kept),
  );
}

/**
 * Add rows read around one message (its context) to `held`, kept from the
 * cap. When the context ends before the rows held begin and more may lie
 * between (`complete` false), the first held row after it is marked.
 */
export function mergeContext<T extends { id: number }>(held: Held<T>, context: T[], complete: boolean): Held<T> {
  if (context.length === 0) return held;
  const newestContext = Math.max(...context.map((r) => r.id));
  // The live rows begin after all of the context only when none overlap it.
  const live = held.rows.find((r) => !held.kept.has(r.id));
  const after = live && live.id > newestContext ? live : undefined;
  const kept = new Set(held.kept);
  for (const r of context) kept.add(r.id);
  const byId = new Map(held.rows.map((r) => [r.id, r]));
  for (const r of context) byId.set(r.id, r);
  const gaps = new Set(held.gaps);
  if (!complete && after) gaps.add(after.id);
  return trim(
    [...byId.values()].sort((a, b) => a.id - b.id),
    gaps,
    kept,
  );
}

/**
 * Let go of the rows kept on purpose, when the message they were read for
 * is no longer sought: they are then trimmed as any others.
 */
export function release<T extends { id: number }>(held: Held<T>): Held<T> {
  if (held.kept.size === 0) return held;
  return trim([...held.rows], new Set(held.gaps), new Set());
}

/** `held` without the row `id`: a row shown early that proved not to belong. */
export function without<T extends { id: number }>(held: Held<T>, id: number): Held<T> {
  const kept = new Set(held.kept);
  kept.delete(id);
  return { rows: held.rows.filter((r) => r.id !== id), gaps: new Set(held.gaps), kept };
}
