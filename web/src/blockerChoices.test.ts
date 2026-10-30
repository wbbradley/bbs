import { describe, expect, it } from "vitest";
import { answerSelection } from "./blockerChoices";
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
