import { describe, expect, it } from "vitest";
import { GoalStatus, type GoalReport } from "../types";
import { extendedDeadline, goalProgress, goalStatus } from "./Goal";

const goal = (over: Partial<GoalReport> = {}): GoalReport => ({
  objective: "Ship it",
  status: GoalStatus.Active,
  set_by: "will",
  set_at: "2026-11-08T09:00:00Z",
  iteration: 3,
  budget: 0,
  reason: "",
  evidence: "",
  wake_at: null,
  changed_by: "runner",
  ...over,
});

describe("goal progress and deadlines", () => {
  it("counts turns, with the budget only when there is one", () => {
    expect(goalProgress(goal())).toBe("turn 3");
    expect(goalProgress(goal({ budget: 50 }))).toBe("turn 3 of 50");
  });

  it("names the expired status", () => {
    expect(goalStatus(goal({ status: GoalStatus.Expired }))).toBe("expired");
  });

  it("moves a deadline a day later, or a day past now once it has passed", () => {
    const now = Date.parse("2026-11-09T12:00:00Z");
    expect(extendedDeadline(goal({ deadline: "2026-11-10T18:00:00Z" }), now)).toBe("2026-11-11T18:00:00Z");
    expect(extendedDeadline(goal({ deadline: "2026-11-01T00:00:00Z" }), now)).toBe("2026-11-10T12:00:00Z");
    expect(extendedDeadline(goal(), now)).toBe("2026-11-10T12:00:00Z");
  });
});
