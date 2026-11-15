import { describe, expect, it } from "vitest";
import { Control, GoalStatus, type GoalReport } from "../types";
import { extendedDeadline, goalControls, goalProgress, goalStatus, pickerValue, waitLeft } from "./Goal";

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

describe("a waiting goal", () => {
  const now = Date.parse("2026-11-10T12:00:00Z");
  const waiting = (wake_at: string | null) => goal({ status: GoalStatus.Waiting, wake_at });

  it("says how much longer it waits, rounded up to the minute", () => {
    expect(waitLeft(waiting("2026-11-10T12:11:30Z"), now)).toMatch(/^12m left, until /);
    expect(waitLeft(waiting("2026-11-10T14:05:00Z"), now)).toMatch(/^2h5m left, until /);
    expect(waitLeft(waiting("2026-11-10T12:00:01Z"), now)).toMatch(/^1m left, until /);
  });

  it("says it is waking once the time has come", () => {
    expect(waitLeft(waiting("2026-11-10T12:00:00Z"), now)).toBe("waking");
    expect(waitLeft(waiting("2026-11-10T11:00:00Z"), now)).toBe("waking");
  });

  it("says nothing without a wake time, or when not waiting", () => {
    expect(waitLeft(waiting(null), now)).toBeNull();
    expect(waitLeft(goal({ status: GoalStatus.Active, wake_at: "2026-11-10T13:00:00Z" }), now)).toBeNull();
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
