import { describe, expect, it } from "vitest";
import { answerSelection, goalDetails, goalLimit } from "./blockerChoices";
import { BlockerChoiceMode, type BlockerChoices } from "./types";

const choices: BlockerChoices = {
  mode: BlockerChoiceMode.SelectMany,
  options: ["a", "b", "c"].map((id) => ({ id, label: id, detail: null })),
  min: 2,
  max: 2,
};

describe("blocker answer selections", () => {
  it("allows note-only answers and canonicalizes valid selections", () => {
    expect(answerSelection(choices, [], "  note\n")).toEqual([]);
    expect(answerSelection(null, [], "note")).toEqual([]);
    expect(answerSelection(choices, ["c", "a"], "")).toEqual(["a", "c"]);
    expect(answerSelection({ ...choices, mode: BlockerChoiceMode.SelectOne, min: null, max: null }, ["b"], "")).toEqual(["b"]);
  });

  it("does not let a note bypass invalid or incomplete selections", () => {
    for (const ids of [["a"], ["a", "b", "c"], ["a", "a"], ["a", "unknown"]]) {
      expect(answerSelection(choices, ids, "a note")).toBeNull();
    }
    expect(answerSelection(choices, [], " \n ")).toBeNull();
    expect(answerSelection(null, ["a"], "note")).toBeNull();
    expect(answerSelection({ ...choices, mode: 99 }, ["a", "b"], "note")).toBeNull();
  });
});

describe("goal proposals", () => {
  const tail = (limit: string, fence: string, objective: string) =>
    `\n\n**Proposed goal**${limit}. Approving sets it, replacing any current goal and its deadline:\n\n${fence}\n${objective}\n${fence}`;

  it("drops the goal the server appended, leaving the agent's details", () => {
    const goal = { objective: "Ship\n\n**Proposed goal**\n```", duration_s: 8 * 3600 };
    const body = "Why" + tail(", with a time limit of 8h from approval", "````", goal.objective);
    expect(goalDetails(body, goal)).toBe("Why");
  });

  it("keeps a decoy heading in the agent's details visible", () => {
    const goal = { objective: "Real", duration_s: null };
    const decoy = tail("", "```", "Fake");
    expect(goalDetails("Why" + decoy + tail("", "```", "Real"), goal)).toBe("Why" + decoy);
  });

  it("returns a body without the appended goal whole", () => {
    expect(goalDetails("Why", { objective: "Ship" })).toBe("Why");
  });

  it("states a time limit exactly", () => {
    expect(goalLimit(8 * 3600)).toBe("8h");
    expect(goalLimit(86400 + 2 * 3600 + 30 * 60 + 5)).toBe("1d 2h 30m 5s");
  });
});
