import { Kind, type Message } from "./types";

/** `name@owner` for agents with a known owner, as every BBS client shows them. */
export function qualified(name: string, owner: string | null | undefined): string {
  return owner ? `${name}@${owner}` : name;
}

export function same(a: string | null | undefined, b: string | null | undefined): boolean {
  return !!a && !!b && a.toLowerCase() === b.toLowerCase();
}

/** "now", "14m ago", "3d4h ago": at most two units, as the Agents list shows. */
export function ago(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "–";
  const secs = Math.max(0, Math.floor((now - Date.parse(iso)) / 1000));
  if (secs < 60) return "now";
  const values: [number, string][] = [
    [Math.floor(secs / 86400), "d"],
    [Math.floor((secs % 86400) / 3600), "h"],
    [Math.floor((secs % 3600) / 60), "m"],
  ];
  const first = values.findIndex(([n]) => n > 0);
  const parts = values.slice(first, first + 2).filter(([n]) => n > 0);
  return `${parts.map(([n, unit]) => `${n}${unit}`).join("")} ago`;
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
 * This browser's storage, or memory for the visit when the browser refuses
 * it (blocked site data, some private windows): reading never throws.
 */
export const storage = (() => {
  const memory = new Map<string, string>();
  const local = (): Storage | null => {
    try {
      return window.localStorage;
    } catch {
      return null;
    }
  };
  return {
    get(key: string): string | null {
      try {
        const value = local()?.getItem(key);
        if (value !== null && value !== undefined) return value;
      } catch {
        // Fall back to this visit's memory.
      }
      return memory.get(key) ?? null;
    },
    set(key: string, value: string) {
      memory.set(key, value);
      try {
        local()?.setItem(key, value);
      } catch {
        // Memory keeps it for the visit.
      }
    },
    remove(key: string) {
      memory.delete(key);
      try {
        local()?.removeItem(key);
      } catch {
        // Nothing stored to remove.
      }
    },
  };
})();

/**
 * Drafts belong to the signed-in human: their keys carry the name, so
 * another human signing in on this browser never sees them. Without a
 * signed-in human nothing is read or kept.
 */
let draftOwner: string | null = null;

export function setDraftOwner(human: string | null) {
  draftOwner = human ? human.toLowerCase() : null;
}

/** A key private to the signed-in human, or null when nobody is. */
export function ownedKey(kind: string, key: string): string | null {
  return draftOwner === null ? null : `bbs-${kind}:${draftOwner}:${key}`;
}

export function loadDraft(key: string): string {
  const k = ownedKey("draft", key);
  return k === null ? "" : (storage.get(k) ?? "");
}

export function saveDraft(key: string, text: string) {
  const k = ownedKey("draft", key);
  if (k === null) return;
  if (text) storage.set(k, text);
  else storage.remove(k);
}
