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

/**
 * What a draft means for `/btw`, as in connect: its first word, after
 * leading space, decides. `/btw <question>` asks on the side; `//btw …` is
 * mail starting with a literal `/btw`; anything else is mail as typed.
 */
export type SideDraft = { kind: "ask"; question: string } | { kind: "literal"; body: string } | { kind: "mail" };

export function sideDraft(draft: string): SideDraft {
  const text = draft.trimStart();
  const word = (prefix: string): string | null => {
    if (!text.startsWith(prefix)) return null;
    const rest = text.slice(prefix.length);
    return rest === "" || /^\s/.test(rest) ? rest : null;
  };
  if (word("//btw") !== null) return { kind: "literal", body: text.slice(1) };
  const question = word("/btw");
  return question === null ? { kind: "mail" } : { kind: "ask", question: question.trim() };
}
