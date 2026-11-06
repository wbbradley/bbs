import { describe, expect, it } from "vitest";
import { buildActivity, mergeTimeline } from "./activity";
import { ago, span } from "./util";
import type { Activity, Message } from "./types";

const rec = (id: number, at: string, record: object) => ({ id, created_at: at, record: JSON.stringify(record) }) as Activity;

describe("span", () => {
  it("shows at most two units, largest first", () => {
    expect(span(0)).toBe("0s");
    expect(span(45)).toBe("45s");
    expect(span(192)).toBe("3m12s");
    expect(span(3600)).toBe("1h");
    expect(span(3600 + 5 * 60 + 9)).toBe("1h5m");
    expect(span(2 * 86400 + 4 * 3600 + 59)).toBe("2d4h");
  });

  it("leaves ago unchanged", () => {
    const now = Date.parse("2026-11-05T12:00:00Z");
    expect(ago("2026-11-05T11:59:30Z", now)).toBe("now");
    expect(ago("2026-11-05T11:46:00Z", now)).toBe("14m ago");
    expect(ago("2026-11-02T08:00:00Z", now)).toBe("3d4h ago");
  });
});

describe("turn end", () => {
  it("carries how long the turn took since its prompt", () => {
    const entries = buildActivity([
      rec(1, "2026-11-05T12:00:00Z", { event: "prompt", text: "hi" }),
      rec(2, "2026-11-05T12:03:12Z", { event: "turn_end", stop_reason: "end_turn" }),
    ]);
    expect(entries[1]).toMatchObject({ kind: "turn_end", reason: "end_turn", took: "3m12s" });
  });

  it("does not measure from a prompt before a failed turn", () => {
    const entries = buildActivity([
      rec(1, "2026-11-05T12:00:00Z", { event: "prompt", text: "hi" }),
      rec(2, "2026-11-05T12:01:00Z", { event: "turn_error", message: "boom" }),
      rec(3, "2026-11-05T12:05:00Z", { event: "turn_end", stop_reason: "end_turn" }),
    ]);
    expect(entries[2]).toMatchObject({ kind: "turn_end", took: "" });
  });

  it("keeps the duration of a cancelled turn", () => {
    const entries = buildActivity([
      rec(1, "2026-11-05T12:00:00Z", { event: "prompt", text: "hi" }),
      rec(2, "2026-11-05T12:00:30Z", { event: "cancel" }),
      rec(3, "2026-11-05T12:00:45Z", { event: "turn_end", stop_reason: "cancelled" }),
    ]);
    expect(entries[2]).toMatchObject({ kind: "turn_end", took: "45s" });
  });

  it("has no duration without a prompt", () => {
    const entries = buildActivity([rec(1, "2026-11-05T12:00:00Z", { event: "turn_end", stop_reason: "end_turn" })]);
    expect(entries[0]).toMatchObject({ kind: "turn_end", took: "" });
  });
});

describe("timeline placement", () => {
  const msg = (id: number, at: string, extra: Partial<Message> = {}): Message =>
    ({
      id,
      created_at: at,
      from_kind: 2,
      from_name: "ada",
      to_group: null,
      to_name: "Mateo",
      read_at: null,
      body: `body ${id}`,
      ...extra,
    }) as Message;
  const order = (items: ReturnType<typeof mergeTimeline>) => items.map((i) => i.key);

  it("shows mail where a prompt delivered it, not where it was sent", () => {
    const entries = buildActivity([
      rec(1, "2026-11-05T12:00:00Z", { event: "prompt", text: "first" }),
      rec(2, "2026-11-05T12:05:00Z", { event: "turn_end", stop_reason: "end_turn" }),
      rec(3, "2026-11-05T12:06:00Z", { event: "prompt", text: "mail", mail_ids: [10] }),
    ]);
    const items = mergeTimeline([msg(10, "2026-11-05T12:01:00Z", { read_at: "2026-11-05T12:01:01Z" })], entries, "Mateo");
    expect(order(items)).toEqual(["a1", "a2", "a3", "m10"]);
  });

  it("shows direct mail with no delivery record where the agent read it", () => {
    const entries = buildActivity([
      rec(1, "2026-11-05T12:00:00Z", { event: "prompt", text: "first" }),
      rec(2, "2026-11-05T12:05:00Z", { event: "turn_end", stop_reason: "end_turn" }),
    ]);
    const mail = [
      msg(10, "2026-11-05T11:59:00Z", { read_at: "2026-11-05T12:06:00Z" }),
      msg(11, "2026-11-05T11:59:30Z"),
      // The agent's own mail keeps its send time, whoever read it.
      msg(12, "2026-11-05T12:01:00Z", { from_kind: 1, from_name: "Mateo", to_name: "ada", read_at: "2026-11-05T12:07:00Z" }),
      // Group mail has no reader of its own.
      msg(13, "2026-11-05T11:58:00Z", { to_name: null, to_group: "crew", read_at: "2026-11-05T12:08:00Z" }),
    ];
    expect(order(mergeTimeline(mail, entries, "Mateo"))).toEqual(["m13", "m11", "a1", "m12", "a2", "m10"]);
    // Without an agent, send times stand.
    expect(order(mergeTimeline(mail, entries))).toEqual(["m13", "m10", "m11", "a1", "m12", "a2"]);
  });
});

describe("goal records", () => {
  it("read as one line, with stops as warnings", () => {
    const entries = buildActivity([
      rec(1, "2026-11-05T12:00:00Z", { event: "goal", status: "active", changed_by: "owner", iteration: 0, budget: 50, reason: "", evidence: "", wake_at: null }),
      rec(2, "2026-11-05T12:01:00Z", { event: "goal", status: "waiting", changed_by: "agent", iteration: 3, budget: 50, reason: "CI runs", evidence: "", wake_at: "2026-11-06T09:00:00Z" }),
      rec(3, "2026-11-05T12:02:00Z", { event: "goal", status: "stalled", changed_by: "runner", iteration: 6, budget: 50, reason: "3 turns in a row used no tools", evidence: "" }),
      rec(4, "2026-11-05T12:03:00Z", { event: "goal", status: "cleared" }),
    ]);
    expect(entries.map((e) => (e.kind === "event" ? [e.level, e.text] : e.kind))).toEqual([
      ["info", "goal active (owner) 0/50"],
      ["info", "goal waiting (agent) 3/50: until 2026-11-06T09:00:00Z — CI runs"],
      ["warning", "goal stalled (runner) 6/50: 3 turns in a row used no tools"],
      ["info", "goal cleared"],
    ]);
  });
});
