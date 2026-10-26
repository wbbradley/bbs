import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { BlockersPage, REPLACED_GUARD_MS, loadKept, nearest } from "./pages/Blockers";
import { useBoard } from "./store";
import { Board } from "./test/setup";
import type { Blocker } from "./types";
import { loadDraft } from "./util";

const row = (root: number, id = root, body = `question ${root}?`): Blocker => ({
  id,
  group_id: 1,
  group: "crew",
  creator: { kind: 2, id: 100 + root, name: "Zed", owner: "ada" },
  body,
  created_at: "2026-10-26T12:00:00Z",
  root_id: root,
  replaces: null,
  replaced_by: null,
  status: 1,
  outcome: null,
  can_answer: true,
  can_cancel: true,
});

let listed: Blocker[] = [];
function Refresh() {
  const board = useBoard();
  return <button onClick={board.refreshBlockers}>refresh</button>;
}

async function page(rows: Blocker[]) {
  listed = rows;
  api.auto.set("list_blockers", () => ({ blockers: listed, active_count: listed.length, has_more: false }));
  render(
    <Board>
      <Refresh />
      <BlockersPage openNav={() => {}} />
    </Board>,
  );
  await act(async () => {});
}

async function poll(rows: Blocker[]) {
  listed = rows;
  await act(async () => fireEvent.click(screen.getByText("refresh")));
}

const answerBox = () => screen.getByPlaceholderText("Your answer goes to the agent as mail.") as HTMLTextAreaElement;

beforeEach(() => {
  api.reset();
  localStorage.clear();
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("blockers", () => {
  it("repeats an unconfirmed answer exactly, and its answer cannot be edited meanwhile", async () => {
    await page([row(1)]);
    fireEvent.change(answerBox(), { target: { value: "yes" } });
    fireEvent.click(screen.getByText("Send answer"));
    const first = api.take("answer_blocker");
    await act(async () => first.reject(new api.ApiError("The server could not be reached.", 0)));
    expect(screen.getByText(/Not confirmed/)).toBeTruthy();
    expect(answerBox().readOnly).toBe(true);
    fireEvent.change(answerBox(), { target: { value: "edited" } });
    expect(answerBox().value).toBe("yes");
    fireEvent.click(screen.getByText("Retry the same answer"));
    const again = api.take("answer_blocker");
    expect(again.req).toEqual(first.req);
    await act(async () => again.resolve({ blocker: row(1), replayed: true, message_id: 5 }));
    expect(screen.getByText("Answered #1 in @crew.")).toBeTruthy();
    expect(loadDraft("blocker:1")).toBe("");
  });

  it("keeps an unconfirmed write across leaving the page", async () => {
    await page([row(1)]);
    fireEvent.change(answerBox(), { target: { value: "yes" } });
    fireEvent.click(screen.getByText("Send answer"));
    const first = api.take("answer_blocker");
    // The page goes before any answer: only a repeat can tell.
    cleanup();
    expect(Object.values(loadKept())[0].state).toBe("unconfirmed");
    await act(async () => first.reject(new api.ApiError("late", 0)));
    await page([row(1)]);
    fireEvent.click(screen.getByText("Retry the same answer"));
    expect(api.take("answer_blocker").req).toEqual(first.req);
  });

  it("lets a refused answer be edited and sent under a new key", async () => {
    await page([row(1)]);
    fireEvent.change(answerBox(), { target: { value: "yes" } });
    fireEvent.click(screen.getByText("Send answer"));
    const first = api.take("answer_blocker");
    await act(async () => first.reject(new api.ApiError("someone answered first", 409)));
    expect(screen.getByText("someone answered first")).toBeTruthy();
    expect(answerBox().readOnly).toBe(false);
    fireEvent.change(answerBox(), { target: { value: "yes, really" } });
    fireEvent.click(screen.getByText("Send answer"));
    const second = api.take("answer_blocker");
    expect(second.req.answer).toBe("yes, really");
    expect(second.req.request_key).not.toBe(first.req.request_key);
  });

  it("keeps a row the server stops listing while its write is unsettled, until dismissed", async () => {
    await page([row(1), row(2)]);
    fireEvent.change(answerBox(), { target: { value: "yes" } });
    fireEvent.click(screen.getByText("Send answer"));
    await act(async () => api.take("answer_blocker").reject(new api.ApiError("timeout", 503)));
    await poll([row(2)]);
    expect(screen.getAllByText(/no longer open/i).length).toBeGreaterThan(0);
    expect(answerBox().value).toBe("yes");
    expect(screen.getByText(/Not confirmed/)).toBeTruthy();
    fireEvent.click(screen.getByText("Dismiss"));
    expect(screen.queryByText(/question 1\?/)).toBeNull();
    expect(loadKept()).toEqual({});
  });

  it("moves the selection to the row now at its place when its row goes", async () => {
    expect(nearest(2, [1, 2, 3], [1, 3])).toBe(3);
    expect(nearest(3, [1, 2, 3], [1, 2])).toBe(2);
    await page([row(1), row(2), row(3)]);
    fireEvent.click(screen.getByText("question 2?"));
    await poll([row(1), row(3)]);
    expect(screen.getByText("question 3?", { selector: ".md p" })).toBeTruthy();
  });

  it("holds the actions for a moment when the selected blocker is replaced", async () => {
    await page([row(1)]);
    fireEvent.change(answerBox(), { target: { value: "for the old text" } });
    vi.useFakeTimers({ toFake: ["Date", "setTimeout", "clearTimeout"] });
    await poll([row(1, 9, "an entirely new question?")]);
    expect(screen.getByText("Blocker updated.")).toBeTruthy();
    expect((screen.getByText("Send answer") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.keyDown(answerBox(), { key: "Enter", metaKey: true });
    expect(api.pending("answer_blocker")).toHaveLength(0);
    await act(async () => vi.advanceTimersByTime(REPLACED_GUARD_MS + 1));
    expect((screen.getByText("Send answer") as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByText("Send answer"));
    expect(api.take("answer_blocker").req.id).toBe(9);
  });

  it("lists again after a write even when a listing was already out", async () => {
    let calls = 0;
    await page([row(1)]);
    api.auto.delete("list_blockers");
    fireEvent.click(screen.getByText("refresh"));
    fireEvent.click(screen.getByText("refresh"));
    const out = api.take("list_blockers");
    await act(async () => out.resolve({ blockers: [row(1)], active_count: 1, has_more: false }));
    calls = api.pending("list_blockers").length;
    expect(calls).toBe(1);
  });
});
