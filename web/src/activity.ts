import type { Activity, Message } from "./types";

// A headless agent's shared activity log as timeline entries, after
// src/ui/activity.rs: thoughts, text, one line per tool call, and events.
// Tool records are paired by call id, so a call is one entry whose status
// changes from running to its result.

export type ActivityEntry =
  | { kind: "thought"; id: number; at: string; text: string }
  | { kind: "text"; id: number; at: string; text: string; mailId: number | null }
  | {
      kind: "tool";
      id: number;
      at: string;
      callId: string;
      name: string;
      status: string;
      took: string;
    }
  | { kind: "prompt"; id: number; at: string; text: string; mailIds: number[] }
  | { kind: "steer"; id: number; at: string; text: string; mailIds: number[] }
  | { kind: "mail"; id: number; at: string; from: string[]; queued: boolean; mailIds: number[] }
  | { kind: "turn_end"; id: number; at: string; reason: string }
  | { kind: "event"; id: number; at: string; text: string; level: "info" | "warning" | "error" };

type Rec = Record<string, unknown>;

const str = (r: Rec, k: string) => (typeof r[k] === "string" ? (r[k] as string) : "");
const ids = (r: Rec, k: string) => (Array.isArray(r[k]) ? (r[k] as unknown[]).filter((v) => typeof v === "number") : []) as number[];

/** A tool's name from its title, without an MCP server prefix. */
function toolName(title: string): string {
  const parts = title.split("mcp__");
  return parts[parts.length - 1] || "tool";
}

/** The mail in a prompt, without the preamble before it. */
function mailPart(prompt: string): string {
  const start = prompt.indexOf("New BBS mail (");
  if (start < 0) return prompt;
  const mail = prompt.slice(start);
  const split = mail.indexOf("\n\n");
  return split < 0 ? mail : mail.slice(split + 2);
}

function rest(r: Rec): string {
  const copy = { ...r };
  delete copy.ts;
  delete copy.event;
  return JSON.stringify(copy);
}

/** Turn boundaries end any call still running, without inventing a result. */
const RESETS = new Set(["start", "session", "prompt", "turn_end", "turn_error", "cancel", "stop"]);

export function buildActivity(records: Activity[]): ActivityEntry[] {
  const out: ActivityEntry[] = [];
  const running = new Map<string, { entry: Extract<ActivityEntry, { kind: "tool" }>; started: number }>();
  for (const a of records) {
    let r: Rec;
    try {
      const parsed = JSON.parse(a.record);
      if (!parsed || typeof parsed !== "object") continue;
      r = parsed as Rec;
    } catch {
      out.push({ kind: "event", id: a.id, at: a.created_at, text: a.record, level: "info" });
      continue;
    }
    const event = str(r, "event");
    const base = { id: a.id, at: a.created_at };
    if (RESETS.has(event)) {
      // Calls that never reported a result leave the timeline, as in connect.
      for (const { entry } of running.values()) {
        const i = out.indexOf(entry);
        if (i >= 0) out.splice(i, 1);
      }
      running.clear();
    }
    switch (event) {
      case "update":
        break;
      case "start":
        out.push({
          ...base,
          kind: "event",
          level: "info",
          text: `started ${str(r, "agent")} (${str(r, "harness")}) in ${str(r, "cwd")}`,
        });
        break;
      case "session":
        out.push({
          ...base,
          kind: "event",
          level: "info",
          text: `session ${str(r, "id")}${r.resumed === true ? " (resumed)" : ""}`,
        });
        break;
      case "mail":
        out.push({
          ...base,
          kind: "mail",
          from: Array.isArray(r.from) ? (r.from as unknown[]).map(String) : [],
          queued: r.queued === true,
          mailIds: ids(r, "mail_ids"),
        });
        break;
      case "prompt":
        out.push({ ...base, kind: "prompt", text: mailPart(str(r, "text")), mailIds: ids(r, "mail_ids") });
        break;
      case "steer":
        out.push({ ...base, kind: "steer", text: mailPart(str(r, "text")), mailIds: ids(r, "mail_ids") });
        break;
      case "thought":
        if (str(r, "text").trim()) out.push({ ...base, kind: "thought", text: str(r, "text") });
        break;
      case "text":
        if (str(r, "text").trim())
          out.push({
            ...base,
            kind: "text",
            text: str(r, "text"),
            mailId: typeof r.mail_id === "number" ? r.mail_id : null,
          });
        break;
      case "tool": {
        const callId = str(r, "id");
        const status = str(r, "status");
        const title = str(r, "title");
        const known = running.get(callId);
        if (known && title) known.entry.name = toolName(title);
        // Partial updates carry output or a title, but no status.
        if (!status) break;
        const ts = Date.parse(str(r, "ts"));
        if (status === "pending" || status === "in_progress") {
          if (!known) {
            const entry = { ...base, kind: "tool" as const, callId, name: toolName(title), status: "running", took: "" };
            running.set(callId, { entry, started: ts });
            out.push(entry);
          }
          break;
        }
        running.delete(callId);
        const took =
          known && !Number.isNaN(known.started) && !Number.isNaN(ts) ? `${((ts - known.started) / 1000).toFixed(1)}s` : "";
        if (known) {
          // The finished call takes the place of its running line, a new object
          // so memoized rows redraw.
          const i = out.indexOf(known.entry);
          const done = { ...known.entry, status, took };
          if (i >= 0) out[i] = done;
          else out.push(done);
        } else {
          out.push({ ...base, kind: "tool", callId, name: toolName(title), status, took });
        }
        break;
      }
      case "permission":
        out.push({ ...base, kind: "event", level: "warning", text: `permission ${rest(r)}` });
        break;
      case "turn_end":
        out.push({ ...base, kind: "turn_end", reason: str(r, "stop_reason") });
        break;
      case "turn_error":
      case "error":
        out.push({ ...base, kind: "event", level: "error", text: `${event}: ${str(r, "message")}` });
        break;
      case "warning":
        out.push({ ...base, kind: "event", level: "warning", text: `warning: ${str(r, "message")}` });
        break;
      case "cancel":
        out.push({ ...base, kind: "event", level: "warning", text: "cancel" });
        break;
      case "stop":
        out.push({
          ...base,
          kind: "event",
          level: typeof r.error === "string" ? "error" : "info",
          text: typeof r.error === "string" ? `stopped: ${r.error}` : "stopped",
        });
        break;
      case "unprocessed":
        out.push({ ...base, kind: "event", level: "warning", text: `unprocessed mail:\n${str(r, "text")}` });
        break;
      default:
        out.push({ ...base, kind: "event", level: "info", text: `${event} ${rest(r)}` });
    }
  }
  return out;
}

export type TimelineItem =
  | { type: "message"; key: string; message: Message; at: string }
  | { type: "activity"; key: string; entry: ActivityEntry; at: string };

/**
 * Messages and activity in one order. Each thing shows once: mail a prompt
 * or steer delivered shows where it entered the agent's context, in place of
 * that echo; receipts and text records naming loaded mail give way to it.
 */
export function mergeTimeline(messages: Message[], entries: ActivityEntry[]): TimelineItem[] {
  const loaded = new Set(messages.map((m) => m.id));
  // Where each piece of mail first entered the agent's context.
  const delivered = new Map<number, { at: string; order: number }>();
  for (const e of entries) {
    if (e.kind !== "prompt" && e.kind !== "steer") continue;
    for (const id of e.mailIds) if (!delivered.has(id)) delivered.set(id, { at: e.at, order: e.id + 0.5 });
  }
  const keyed: { item: TimelineItem; at: number; order: number }[] = [];
  for (const m of messages) {
    const spot = delivered.get(m.id);
    const at = spot?.at ?? m.created_at;
    keyed.push({
      item: { type: "message", key: `m${m.id}`, message: m, at },
      at: Date.parse(at),
      order: spot?.order ?? -Infinity,
    });
  }
  for (const e of entries) {
    const echoes =
      e.kind === "text" ? (e.mailId !== null ? [e.mailId] : []) : "mailIds" in e ? e.mailIds : [];
    if (echoes.length > 0 && echoes.every((id) => loaded.has(id))) {
      // A prompt still marks where its turn began.
      if (e.kind !== "prompt") continue;
      keyed.push({
        item: { type: "activity", key: `a${e.id}`, entry: { ...e, text: "" }, at: e.at },
        at: Date.parse(e.at),
        order: e.id,
      });
      continue;
    }
    keyed.push({ item: { type: "activity", key: `a${e.id}`, entry: e, at: e.at }, at: Date.parse(e.at), order: e.id });
  }
  keyed.sort((a, b) => a.at - b.at || a.order - b.order);
  return keyed.map((k) => k.item);
}
