import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { BlockersPage } from "./pages/Blockers";
import { useBoard } from "./store";
import { Board, INFO } from "./test/setup";
import { BLOCKER_CHOICES } from "./blockerChoices";
import { BlockerChoiceMode, BlockerStatus, type Blocker } from "./types";

const row = (id: number, status: number = BlockerStatus.Open): Blocker => ({
  id, root_id: id, group_id: 1, group: "crew", creator: { id: 10, kind: 1, name: "Mateo", owner: "ada" },
  body: `Question ${id}?`, created_at: "2026-10-31T12:00:00Z", replaces: null, replaced_by: null,
  status, can_answer: true, can_cancel: true,
  outcome: status === BlockerStatus.Open ? null : {
    status, actor: { id: 1, kind: 2, name: "ada", owner: null },
    created_at: "2026-11-01T12:00:00Z", answer: status === BlockerStatus.Answered ? "Saved answer" : null,
    message_id: null, selected: [],
  },
});
const listing = (blockers: Blocker[], has_more = false, next_after_root_id: number | null = null) => ({
  blockers, active_count: 2, has_more, next_after_root_id,
});

function RefreshOpen() {
  const board = useBoard();
  return <button onClick={board.refreshBlockers}>Refresh open queue</button>;
}

async function page(rows: Blocker[] = [row(1), row(2)], strict = false) {
  api.auto.set("list_blockers", (req) => {
    if (!req.include_closed) return listing(rows);
    return new Promise((resolve, reject) => api.calls.push({ method: "list_blockers", req, resolve, reject }));
  });
  const content = <Board info={{ ...INFO, capabilities: [...INFO.capabilities, BLOCKER_CHOICES] }}><RefreshOpen /><BlockersPage openNav={() => {}} /></Board>;
  const view = render(strict ? <StrictMode>{content}</StrictMode> : content);
  await act(async () => {});
  return view;
}

const closed = () => within(screen.getByRole("region", { name: "Closed blockers" }));
const open = () => fireEvent.click(screen.getByRole("button", { name: "Open" }));
const showClosed = () => fireEvent.click(screen.getByRole("button", { name: "Closed" }));
const answer = () => screen.getByRole("textbox") as HTMLTextAreaElement;

beforeEach(() => { api.reset(); localStorage.clear(); });
afterEach(cleanup);

describe("closed blocker mode", () => {
  it("loads only on demand, displays saved outcomes read-only, and leaves open counts alone", async () => {
    await page();
    expect(api.pending("list_blockers")).toHaveLength(0);
    showClosed();
    expect(closed().getByRole("status").textContent).toContain("Loading");
    const request = api.take("list_blockers");
    expect(request.req).toEqual({ mine: true, include_closed: true, limit: 50 });
    await act(async () => request.resolve(listing([row(1), row(9, BlockerStatus.Answered), row(10, BlockerStatus.Cancelled)])));
    expect(closed().queryByText("Question 1?")).toBeNull();
    expect(closed().getByText("Saved answer")).toBeTruthy();
    expect(closed().getByText("ada")).toBeTruthy();
    expect(closed().queryByRole("textbox")).toBeNull();
    expect(closed().queryByRole("button", { name: "Send answer" })).toBeNull();
    expect(closed().queryByRole("button", { name: "Cancel blocker" })).toBeNull();
    fireEvent.click(closed().getByRole("button", { name: /#10 · Cancelled/ }));
    expect(closed().getByRole("region", { name: "Blocker outcome" }).textContent).toContain("Cancelled by ada");
    expect(closed().queryByText("Saved answer")).toBeNull();
    expect(screen.getByText("2 open")).toBeTruthy();
    expect(api.pending("answer_blocker")).toHaveLength(0);
    expect(api.pending("cancel_blocker")).toHaveLength(0);
  });

  it("keeps the selected open draft, selection, and cursor across mode switches without stealing focus", async () => {
    const choice = { ...row(2), choices: { mode: BlockerChoiceMode.SelectMany, options: [{ id: "go", label: "Proceed", detail: null }, { id: "wait", label: "Wait", detail: null }], min: 1, max: 2 } };
    await page([row(1), choice]);
    fireEvent.click(screen.getByText("Question 2?", { selector: ".preview" }));
    const input = answer();
    fireEvent.change(input, { target: { value: "Keep my draft" } });
    fireEvent.click(screen.getByRole("checkbox", { name: "Proceed" }));
    input.setSelectionRange(2, 6);
    showClosed();
    await act(async () => api.take("list_blockers").resolve(listing([row(9, BlockerStatus.Answered)])));
    const historyRefresh = closed().getByRole("button", { name: "Refresh" });
    historyRefresh.focus();
    await act(async () => fireEvent.click(screen.getByText("Refresh open queue")));
    expect(document.activeElement).toBe(historyRefresh);
    open();
    expect(answer()).toBe(input);
    expect(answer().value).toBe("Keep my draft");
    expect(answer().selectionStart).toBe(2);
    expect(answer().selectionEnd).toBe(6);
    expect((screen.getByRole("checkbox", { name: "Proceed" }) as HTMLInputElement).checked).toBe(true);
    expect(document.activeElement).toBe(input);
    expect(api.pending("answer_blocker")).toHaveLength(0);
  });

  it("retains an unconfirmed answer and its exact retry when the request fails while browsing Closed", async () => {
    await page([row(1)]);
    fireEvent.change(answer(), { target: { value: "Exact draft" } });
    fireEvent.click(screen.getByRole("button", { name: "Send answer" }));
    const pending = api.take("answer_blocker");
    showClosed();
    await act(async () => api.take("list_blockers").resolve(listing([])));
    await act(async () => pending.reject(new api.ApiError("uncertain", 503)));
    expect(closed().getByText("No closed blockers.")).toBeTruthy();
    open();
    expect(answer().readOnly).toBe(true);
    expect(answer().value).toBe("Exact draft");
    fireEvent.click(screen.getByRole("button", { name: "Retry the same answer" }));
    expect(api.take("answer_blocker").req).toEqual(pending.req);
  });

  it("paginates past open-only results and preserves loaded rows and the cursor after failure", async () => {
    await page(); showClosed();
    await act(async () => api.take("list_blockers").resolve(listing([row(1)], true, 50)));
    expect(closed().getByText(/No closed blockers in these results/)).toBeTruthy();
    fireEvent.click(closed().getByRole("button", { name: "Load more" }));
    let request = api.take("list_blockers");
    expect(request.req).toEqual({ mine: true, include_closed: true, limit: 50, after_root_id: 50 });
    await act(async () => request.resolve(listing([row(80, BlockerStatus.Cancelled)], true, 100)));
    fireEvent.click(closed().getByRole("button", { name: "Load more" }));
    request = api.take("list_blockers");
    await act(async () => request.reject(new Error("offline")));
    expect(closed().getByRole("alert").textContent).toContain("Showing the last successful results");
    expect(closed().getByText("Question 80?", { selector: ".preview" })).toBeTruthy();
    fireEvent.click(closed().getByRole("button", { name: "Load more" }));
    const retry = api.take("list_blockers");
    expect(retry.req).toEqual(request.req);
    await act(async () => retry.resolve(listing([row(120, BlockerStatus.Answered)])));
    expect(closed().getByText("Question 80?", { selector: ".preview" })).toBeTruthy();
    expect(closed().getByText("Question 120?", { selector: ".preview" })).toBeTruthy();
    expect(closed().queryByRole("button", { name: "Load more" })).toBeNull();
    expect(screen.getByText("2 open")).toBeTruthy();
  });

  it("reports a load error rather than an empty history and retries a refresh without stale rows", async () => {
    await page(); showClosed();
    await act(async () => api.take("list_blockers").reject(new Error("unavailable")));
    expect(closed().queryByText("No closed blockers.")).toBeNull();
    expect(closed().getByRole("alert").textContent).toContain("unavailable");
    fireEvent.click(closed().getByRole("button", { name: "Retry" }));
    await act(async () => api.take("list_blockers").resolve(listing([row(9, BlockerStatus.Answered)])));
    fireEvent.click(closed().getByRole("button", { name: "Refresh" }));
    await act(async () => api.take("list_blockers").resolve(listing([row(9), row(10, BlockerStatus.Cancelled)])));
    expect(closed().queryByText("Question 9?", { selector: ".preview" })).toBeNull();
    expect(closed().getByText("Question 10?", { selector: ".preview" })).toBeTruthy();
  });

  it("ignores replies from a closed panel and StrictMode's abandoned initial request", async () => {
    await page(); showClosed();
    const old = api.take("list_blockers");
    open(); showClosed();
    await act(async () => api.take("list_blockers").resolve(listing([row(22, BlockerStatus.Answered)])));
    await act(async () => old.resolve(listing([row(11, BlockerStatus.Cancelled)])));
    expect(closed().queryByText("Question 11?", { selector: ".preview" })).toBeNull();
    cleanup(); api.reset();
    await page([row(1)], true); showClosed();
    const abandoned = api.take("list_blockers");
    const current = api.take("list_blockers");
    await act(async () => current.resolve(listing([row(22, BlockerStatus.Answered)])));
    await act(async () => abandoned.resolve(listing([row(11, BlockerStatus.Cancelled)])));
    expect(closed().getByText("Question 22?", { selector: ".preview" })).toBeTruthy();
    expect(closed().queryByText("Question 11?", { selector: ".preview" })).toBeNull();
  });

  it("stops on a non-advancing history cursor instead of dropping results or requesting forever", async () => {
    await page(); showClosed();
    await act(async () => api.take("list_blockers").resolve(listing([row(9, BlockerStatus.Answered)], true, 50)));
    fireEvent.click(closed().getByRole("button", { name: "Load more" }));
    await act(async () => api.take("list_blockers").resolve(listing([], true, 50)));
    expect(closed().getByRole("alert").textContent).toContain("cursor that did not advance");
    expect(closed().getByText("Question 9?", { selector: ".preview" })).toBeTruthy();
    expect(api.pending("list_blockers")).toHaveLength(0);
  });
});
