import { describe, expect, it } from "vitest";
import { buildActivity } from "./activity";
import { ago, span } from "./util";
import type { Activity } from "./types";

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

  it("has no duration without a prompt", () => {
    const entries = buildActivity([rec(1, "2026-11-05T12:00:00Z", { event: "turn_end", stop_reason: "end_turn" })]);
    expect(entries[0]).toMatchObject({ kind: "turn_end", took: "" });
  });
});
