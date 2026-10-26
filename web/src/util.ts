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

const DRAFT_PREFIX = "bbs-draft:";

export function loadDraft(key: string): string {
  try {
    return localStorage.getItem(DRAFT_PREFIX + key) ?? "";
  } catch {
    return "";
  }
}

export function saveDraft(key: string, text: string) {
  try {
    if (text) localStorage.setItem(DRAFT_PREFIX + key, text);
    else localStorage.removeItem(DRAFT_PREFIX + key);
  } catch {
    // Storage may be unavailable (private windows); drafts then last the visit.
  }
}
