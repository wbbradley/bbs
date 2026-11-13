import { Kind, type Message } from "./types";

/** `name@owner` for agents with a known owner, as every BBS client shows them. */
export function qualified(name: string, owner: string | null | undefined): string {
  return owner ? `${name}@${owner}` : name;
}

export function same(a: string | null | undefined, b: string | null | undefined): boolean {
  return !!a && !!b && a.toLowerCase() === b.toLowerCase();
}

const UNITS: [number, string][] = [
  [86400, "d"],
  [3600, "h"],
  [60, "m"],
  [1, "s"],
];

/** "45s", "3m12s", "2h5m": at most two units, none smaller than `smallest` seconds. */
export function span(secs: number, smallest = 1): string {
  const units = UNITS.filter(([size]) => size >= smallest);
  let rest = Math.max(0, Math.floor(secs));
  const values = units.map(([size, unit]): [number, string] => {
    const n = Math.floor(rest / size);
    rest %= size;
    return [n, unit];
  });
  const first = values.findIndex(([n]) => n > 0);
  if (first < 0) return `0${units[units.length - 1][1]}`;
  const parts = values.slice(first, first + 2).filter(([n]) => n > 0);
  return parts.map(([n, unit]) => `${n}${unit}`).join("");
}

/** "now", "14m ago", "3d4h ago": at most two units, as the Agents list shows. */
export function ago(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "–";
  const secs = Math.max(0, Math.floor((now - Date.parse(iso)) / 1000));
  if (secs < 60) return "now";
  return `${span(secs, 60)} ago`;
}

/** "now", "in 14m", "in 3d4h": `ago` for times to come. */
export function until(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "–";
  const secs = Math.floor((Date.parse(iso) - now) / 1000);
  if (secs < 60) return "now";
  return `in ${span(secs, 60)}`;
}

// humantime's units, as the server writes a timer's `every` duration.
const DURATION_UNITS: Record<string, number> = {
  years: 31557600,
  year: 31557600,
  y: 31557600,
  months: 2630016,
  month: 2630016,
  M: 2630016,
  weeks: 604800,
  week: 604800,
  w: 604800,
  days: 86400,
  day: 86400,
  d: 86400,
  h: 3600,
  m: 60,
  s: 1,
};

/**
 * A timer's repeat as the Timers list shows it: "every 1h30m" for an
 * interval, a cron expression as written, "once" for a one-time timer.
 */
export function repeatLabel(repeat: string | null | undefined): string {
  if (!repeat) return "once";
  const every = /^every\s+(.+)$/.exec(repeat.trim());
  if (!every) return repeat;
  let secs = 0;
  for (const part of every[1].split(/\s+/)) {
    const m = /^(\d+)([a-zA-Z]+)$/.exec(part);
    const size = m ? DURATION_UNITS[m[2]] : undefined;
    if (!m || size === undefined) return repeat;
    secs += Number(m[1]) * size;
  }
  return `every ${span(secs)}`;
}

/** A time to the second in UTC: "2026-11-06 09:00:00 UTC". */
export function utcTime(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? "" : `${d.toISOString().slice(0, 19).replace("T", " ")} UTC`;
}

/** Local time: the time today, with the date on other days. */
export function clock(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  const today = new Date();
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (d.toDateString() === today.toDateString()) return time;
  const date = d.toLocaleDateString([], {
    month: "short",
    day: "numeric",
    year: d.getFullYear() === today.getFullYear() ? undefined : "numeric",
  });
  return `${date} ${time}`;
}

export function fullTime(iso: string | null | undefined): string {
  return iso ? new Date(iso).toLocaleString() : "";
}

export function dayLabel(iso: string): string {
  const d = new Date(iso);
  const today = new Date();
  const yesterday = new Date(Date.now() - 86400000);
  if (d.toDateString() === today.toDateString()) return "Today";
  if (d.toDateString() === yesterday.toDateString()) return "Yesterday";
  return d.toLocaleDateString([], { weekday: "long", month: "long", day: "numeric", year: "numeric" });
}

/** Whether a name is another installation's entity, `id:ENTITY@NAMESPACE`. */
export function isRemoteName(name: string | null | undefined): boolean {
  return !!name && name.startsWith("id:");
}

export function senderName(m: Message): string {
  return m.from_kind === Kind.Agent ? qualified(m.from_name, m.from_owner) : m.from_name;
}

export function cap99(n: number | null | undefined): string {
  if (!n) return "";
  return n > 99 ? "99+" : String(n);
}

/** A random key without `crypto.randomUUID`, which plain-HTTP pages lack. */
export function randomKey(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * This browser's storage, with memory standing in key by key for the
 * visit wherever the browser refuses a write (blocked site data, a full
 * quota): a key written to memory is read from memory until a later write
 * to the browser succeeds, and a refused removal leaves a tombstone, so
 * nothing written comes back stale. Keys the browser holds are read from
 * it, so another tab's changes show. Reading never throws.
 */
export const storage = (() => {
  // Keys memory answers for; null is a removal the browser refused.
  const memory = new Map<string, string | null>();
  const local = (): Storage | null => {
    try {
      return window.localStorage;
    } catch {
      return null;
    }
  };
  return {
    get(key: string): string | null {
      if (memory.has(key)) return memory.get(key) ?? null;
      try {
        return local()?.getItem(key) ?? null;
      } catch {
        return null;
      }
    },
    set(key: string, value: string) {
      try {
        const store = local();
        if (store) {
          store.setItem(key, value);
          memory.delete(key);
          return;
        }
      } catch {
        // Refused or full: memory keeps it for the visit.
      }
      memory.set(key, value);
    },
    remove(key: string) {
      try {
        const store = local();
        if (store) {
          store.removeItem(key);
          memory.delete(key);
          return;
        }
      } catch {
        // Refused: remember it is gone.
      }
      memory.set(key, null);
    },
    /** Every key the browser holds, for clearing old ones. */
    keys(): string[] {
      try {
        const store = local();
        return store ? Array.from({ length: store.length }, (_, i) => store.key(i)).filter((k): k is string => !!k) : [];
      } catch {
        return [];
      }
    },
  };
})();

/**
 * Drafts and kept writes belong to the signed-in human: their keys carry
 * the name, so another human signing in on this browser never sees them.
 * Without a signed-in human nothing is read or kept.
 */
let draftOwner: string | null = null;

export function setDraftOwner(human: string | null) {
  draftOwner = human ? human.toLowerCase() : null;
  // Drafts from before keys carried their human cannot be told apart by
  // whose they were: they are never read, and go.
  for (const key of storage.keys()) if (key.startsWith("bbs-draft:")) storage.remove(key);
}

/** Who owns drafts now: captured when work starts, so it ends as theirs. */
export function draftOwnerNow(): string | null {
  return draftOwner;
}

/** A key private to `owner` (the signed-in human by default), or null. */
export function ownedKey(kind: string, key: string, owner: string | null = draftOwner): string | null {
  return owner === null ? null : `bbs-v2-${kind}:${owner}:${key}`;
}

export function loadDraft(key: string, owner: string | null = draftOwner): string {
  const k = ownedKey("draft", key, owner);
  return k === null ? "" : (storage.get(k) ?? "");
}

export function saveDraft(key: string, text: string, owner: string | null = draftOwner) {
  const k = ownedKey("draft", key, owner);
  if (k === null) return;
  if (text) storage.set(k, text);
  else storage.remove(k);
}
