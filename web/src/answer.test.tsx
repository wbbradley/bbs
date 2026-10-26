import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { AnswerDialog } from "./components/AnswerDialog";
import type { OwnerInput } from "./types";

const input = (request_id: string, ids: string[]): OwnerInput =>
  ({
    request_id,
    status: 1,
    is_blocking: true,
    message: null,
    questions: ids.map((id) => ({
      id,
      header: `H ${id}`,
      text: `Q ${id}`,
      required: true,
      allow_free_text: false,
      options: [
        { value: "a", label: "A", description: null },
        { value: "b", label: "B", description: null },
      ],
    })),
  }) as unknown as OwnerInput;

afterEach(() => {
  cleanup();
  api.reset();
});

describe("the questions form", () => {
  it("takes question ids that objects already have, such as constructor", () => {
    render(<AnswerDialog agent="Mateo" input={input("r1", ["constructor", "__proto__", "toString"])} onClose={() => {}} />);
    const submit = screen.getByText("Submit") as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    for (const a of screen.getAllByText("A")) fireEvent.click(a);
    expect(submit.disabled).toBe(false);
    fireEvent.click(submit);
    expect(api.take("answer_owner_input").req.answers).toEqual([
      { question_id: "constructor", value: "a" },
      { question_id: "__proto__", value: "a" },
      { question_id: "toString", value: "a" },
    ]);
  });

  it("starts empty for a new request rather than carry answers over", () => {
    const { rerender } = render(
      <AnswerDialog key="r1" agent="Mateo" input={input("r1", ["q"])} onClose={() => {}} />,
    );
    fireEvent.click(screen.getByText("A"));
    rerender(<AnswerDialog key="r2" agent="Mateo" input={input("r2", ["q"])} onClose={() => {}} />);
    expect((screen.getByText("Submit") as HTMLButtonElement).disabled).toBe(true);
  });
});
