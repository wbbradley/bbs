import { describe, expect, it } from "vitest";
import { Control, GoalStatus, type GoalReport } from "../types";
import { extendedDeadline, goalControls, goalProgress, goalStatus, pickerValue } from "./Goal";

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

describe("editing a goal", () => {
  const due = "2026-11-12T17:00:00Z";

  it("only moves the deadline when the objective is unchanged", () => {
    expect(goalControls(goal(), "Ship it", due, true)).toEqual([[Control.GoalDeadline, due]]);
    expect(goalControls(goal({ deadline: due }), "Ship it", null, true)).toEqual([[Control.GoalDeadline, "none"]]);
    expect(goalControls(goal(), "Ship it", null, false)).toEqual([]);
  });

  it("sets a new goal, keeping the deadline, when the objective changes", () => {
    expect(goalControls(goal({ deadline: due }), "Ship it today", due, false)).toEqual([
      [Control.GoalSet, "Ship it today"],
      [Control.GoalDeadline, due],
    ]);
    expect(goalControls(goal(), "Ship it today", null, false)).toEqual([[Control.GoalSet, "Ship it today"]]);
  });

  it("always sets a goal from the New dialog", () => {
    expect(goalControls(null, "Ship it", null, false)).toEqual([[Control.GoalSet, "Ship it"]]);
  });

  it("shows a deadline in the picker in local time", () => {
    const local = new Date(2026, 10, 12, 9, 5);
    expect(pickerValue(local.toISOString())).toBe("2026-11-12T09:05");
    expect(pickerValue(null)).toBe("");
  });
});
