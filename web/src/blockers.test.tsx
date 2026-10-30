import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { Dialog } from "./components/Dialog";
import { BlockersPage, REPLACED_GUARD_MS, loadKept, nearest } from "./pages/Blockers";
import { useBoard } from "./store";
import { Board, INFO } from "./test/setup";
import { BlockerChoiceMode, type Blocker, type ServerInfo } from "./types";
import { BLOCKER_CHOICES } from "./blockerChoices";
import { loadDraft, saveDraft, setDraftOwner } from "./util";

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

async function page(rows: Blocker[], info: ServerInfo | null = INFO) {
  listed = rows;
  api.auto.set("list_blockers", () => ({ blockers: listed, active_count: listed.length, has_more: false }));
  render(
    <Board info={info}>
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
  it("focuses each selected answer, including a second click on the same row", async () => {
    await page([row(1), row(2)]);
    expect(document.activeElement).toBe(answerBox());
    fireEvent.change(answerBox(), { target: { value: "first draft" } });

    fireEvent.click(screen.getByText("question 2?", { selector: ".preview" }));
    expect(document.activeElement).toBe(answerBox());
    fireEvent.change(answerBox(), { target: { value: "second draft" } });
    answerBox().setSelectionRange(2, 6);
    screen.getByText("refresh").focus();
    fireEvent.click(screen.getByText("question 2?", { selector: ".preview" }));
    expect(document.activeElement).toBe(answerBox());
    expect(answerBox().selectionStart).toBe(2);
    expect(answerBox().selectionEnd).toBe(6);

    fireEvent.click(screen.getByText("question 1?", { selector: ".preview" }));
    expect(document.activeElement).toBe(answerBox());
    expect(answerBox().value).toBe("first draft");
    expect(api.pending("answer_blocker")).toHaveLength(0);
  });

  it("does not take focus back on a refresh or a replacement of the selected root", async () => {
    await page([row(1), row(2)]);
    const refresh = screen.getByText("refresh");
    refresh.focus();
    await poll([row(1), row(2)]);
    expect(document.activeElement).toBe(refresh);
    await poll([row(1, 9, "replacement question?"), row(2)]);
    expect(document.activeElement).toBe(refresh);
    expect(screen.getByText("Blocker updated.")).toBeTruthy();
  });

  it("focuses the next answer when the selected blocker leaves the queue", async () => {
    await page([row(1), row(2), row(3)]);
    fireEvent.click(screen.getByText("question 2?", { selector: ".preview" }));
    screen.getByText("refresh").focus();
    await poll([row(1), row(3)]);
    expect(screen.getByText("question 3?", { selector: ".md p" })).toBeTruthy();
    expect(document.activeElement).toBe(answerBox());
  });

  it("does not focus a selected read-only answer", async () => {
    setDraftOwner("ada");
    saveDraft("blocker:2", "saved answer");
    await page([row(1), { ...row(2), can_answer: false }]);
    const refresh = screen.getByText("refresh");
    refresh.focus();
    fireEvent.click(screen.getByText("question 2?", { selector: ".preview" }));
    expect(answerBox().readOnly).toBe(true);
    expect(document.activeElement).toBe(refresh);
    fireEvent.click(screen.getByText("question 2?", { selector: ".preview" }));
    expect(document.activeElement).toBe(refresh);
  });

  it("keeps a dialog's focus when the selected blocker changes beneath it", async () => {
    await page([row(1), row(2)]);
    render(
      <Dialog title="Another action" onClose={() => {}}>
        <input autoFocus placeholder="Dialog input" />
      </Dialog>,
    );
    const input = screen.getByPlaceholderText("Dialog input");
    expect(document.activeElement).toBe(input);
    await poll([row(2)]);
    expect(document.activeElement).toBe(input);
  });

  it("sends with Enter, preserves multiline text, and holds keys while a write is unsettled", async () => {
    await page([row(1)]);
    fireEvent.change(answerBox(), { target: { value: "\nfirst\nsecond" } });
    for (const modifier of [{ shiftKey: true }, { altKey: true }, { isComposing: true }, { keyCode: 229 }]) {
      expect(fireEvent.keyDown(answerBox(), { key: "Enter", ...modifier })).toBe(true);
      expect(api.pending("answer_blocker")).toHaveLength(0);
    }
    expect(fireEvent.keyDown(answerBox(), { key: "Enter" })).toBe(false);
    const first = api.take("answer_blocker");
    expect(first.req.answer).toBe("\nfirst\nsecond");
    expect(answerBox().readOnly).toBe(true);
    fireEvent.keyDown(answerBox(), { key: "Enter" });
    expect(api.pending("answer_blocker")).toHaveLength(0);
    await act(async () => first.reject(new api.ApiError("unconfirmed", 503)));
    fireEvent.keyDown(answerBox(), { key: "Enter" });
    expect(api.pending("answer_blocker")).toHaveLength(0);
    expect(answerBox().value).toBe("\nfirst\nsecond");
  });

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

  it("ends a write as its own human's, though another has signed in meanwhile", async () => {
    await page([row(1)]);
    fireEvent.change(answerBox(), { target: { value: "ada's answer" } });
    fireEvent.click(screen.getByText("Send answer"));
    const out = api.take("answer_blocker");
    // Ada is signed out, and Bob signs in and starts his own answer.
    cleanup();
    setDraftOwner("bob");
    saveDraft("blocker:1", "bob's answer");
    await act(async () => out.resolve({ blocker: row(1), replayed: false, message_id: 5 }));
    expect(loadDraft("blocker:1")).toBe("bob's answer");
    expect(loadKept("bob")).toEqual({});
    expect(loadKept("ada")).toEqual({});
    expect(loadDraft("blocker:1", "ada")).toBe("");
  });

  it("shows a write's outcome under StrictMode, which mounts the page twice", async () => {
    listed = [row(1)];
    api.auto.set("list_blockers", () => ({ blockers: listed, active_count: listed.length, has_more: false }));
    render(
      <StrictMode>
        <Board>
          <BlockersPage openNav={() => {}} />
        </Board>
      </StrictMode>,
    );
    await act(async () => {});
    fireEvent.change(answerBox(), { target: { value: "yes" } });
    fireEvent.click(screen.getByText("Send answer"));
    expect(screen.getByText("Sending…")).toBeTruthy();
    await act(async () => api.take("answer_blocker").reject(new api.ApiError("someone answered first", 409)));
    expect(screen.getByText("someone answered first")).toBeTruthy();
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
    fireEvent.keyDown(answerBox(), { key: "Enter" });
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

const choiceInfo: ServerInfo = { ...INFO, capabilities: [...INFO.capabilities, BLOCKER_CHOICES] };
const choiceRow = (mode: number = BlockerChoiceMode.SelectOne, id = 1): Blocker => ({
  ...row(1, id),
  choices: {
    mode,
    options: [
      { id: "approve", label: "Approve", detail: "Continue the plan" },
      { id: "wait", label: "Wait", detail: null },
      { id: "decline", label: "Decline", detail: null },
    ],
    min: mode === BlockerChoiceMode.SelectMany ? 2 : null,
    max: mode === BlockerChoiceMode.SelectMany ? 2 : null,
  },
});
async function choicePage(rows: Blocker[] = [choiceRow()]) {
  api.auto.set("server_info", () => choiceInfo);
  await page(rows, choiceInfo);
}
const sendButton = () => screen.getByRole("button", { name: "Send answer" }) as HTMLButtonElement;
const checkbox = (name: string) => screen.getByRole("checkbox", { name }) as HTMLInputElement;

describe("structured blocker answers", () => {
  it("sends a single pick immediately with its note frozen before the fresh capability check", async () => {
    await choicePage();
    api.auto.delete("server_info");
    fireEvent.change(answerBox(), { target: { value: "\n  keep this note\n" } });
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    const info = api.take("server_info");
    expect(api.pending("answer_blocker")).toHaveLength(0);
    expect((screen.getByRole("button", { name: "Wait" }) as HTMLButtonElement).disabled).toBe(true);
    expect(answerBox().readOnly).toBe(true);
    fireEvent.change(answerBox(), { target: { value: "changed" } });
    fireEvent.click(screen.getByRole("button", { name: "Wait" }));
    expect(api.pending("server_info")).toHaveLength(0);
    await act(async () => info.resolve(choiceInfo));
    const out = api.take("answer_blocker");
    expect(out.req.selected).toEqual(["approve"]);
    expect(out.req.answer).toBe("\n  keep this note\n");
    expect(loadKept()[1].selected).toEqual(["approve"]);
  });

  it("accepts a single pick without text, and still sends a note alone through Send", async () => {
    await choicePage();
    expect(sendButton().disabled).toBe(true);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Wait" })));
    const out = api.take("answer_blocker");
    expect(out.req.selected).toEqual(["wait"]);
    expect(out.req.answer).toBe("");
    await act(async () => out.reject(new api.ApiError("refused", 409)));
    fireEvent.change(answerBox(), { target: { value: "Another approach" } });
    fireEvent.click(sendButton());
    const note = api.take("answer_blocker");
    expect(note.req.answer).toBe("Another approach");
    expect(note.req.selected).toBeUndefined();
  });

  it("requires a valid checkbox count even with a note, and sends IDs in declared order", async () => {
    await choicePage([choiceRow(BlockerChoiceMode.SelectMany)]);
    fireEvent.change(answerBox(), { target: { value: "note" } });
    fireEvent.click(checkbox("Wait"));
    expect(sendButton().disabled).toBe(true);
    fireEvent.keyDown(answerBox(), { key: "Enter" });
    expect(api.pending("answer_blocker")).toHaveLength(0);
    fireEvent.click(checkbox("Approve"));
    expect(sendButton().disabled).toBe(false);
    expect(api.pending("answer_blocker")).toHaveLength(0);
    fireEvent.click(checkbox("Decline"));
    expect(sendButton().disabled).toBe(true);
    fireEvent.click(checkbox("Decline"));
    await act(async () => fireEvent.click(sendButton()));
    expect(api.take("answer_blocker").req.selected).toEqual(["approve", "wait"]);
  });

  it("can clear an incomplete selection and answer with only free text", async () => {
    await choicePage([choiceRow(BlockerChoiceMode.SelectMany)]);
    fireEvent.click(checkbox("Wait"));
    fireEvent.change(answerBox(), { target: { value: "None of these" } });
    expect(sendButton().disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Clear selection" }));
    expect(checkbox("Wait").checked).toBe(false);
    fireEvent.click(sendButton());
    expect(api.take("answer_blocker").req.selected).toBeUndefined();
  });

  it("keeps drafts on navigation and polls, but resets option IDs on replacement without stealing focus", async () => {
    const many = choiceRow(BlockerChoiceMode.SelectMany);
    await choicePage([many, row(2)]);
    fireEvent.click(checkbox("Wait"));
    fireEvent.change(answerBox(), { target: { value: "saved note" } });
    fireEvent.click(screen.getByText("question 2?", { selector: ".preview" }));
    fireEvent.click(screen.getByText("question 1?", { selector: ".preview" }));
    expect(checkbox("Wait").checked).toBe(true);
    const refresh = screen.getByText("refresh");
    refresh.focus();
    await poll([{ ...many }, row(2)]);
    expect(checkbox("Wait").checked).toBe(true);
    expect(document.activeElement).toBe(refresh);
    await poll([choiceRow(BlockerChoiceMode.SelectMany, 9), row(2)]);
    expect(checkbox("Wait").checked).toBe(false);
    expect(answerBox().value).toBe("saved note");
    expect(document.activeElement).toBe(refresh);
    expect(checkbox("Wait").disabled).toBe(true);
  });

  it("retries the exact frozen selection, note, version and key after replacement and remount", async () => {
    await choicePage([choiceRow(BlockerChoiceMode.SelectMany)]);
    fireEvent.click(checkbox("Wait"));
    fireEvent.click(checkbox("Approve"));
    fireEvent.change(answerBox(), { target: { value: "exact note" } });
    await act(async () => fireEvent.click(sendButton()));
    const first = api.take("answer_blocker");
    await act(async () => first.reject(new api.ApiError("timeout", 503)));
    vi.useFakeTimers();
    cleanup();
    await choicePage([choiceRow(BlockerChoiceMode.SelectMany, 9)]);
    expect(checkbox("Wait").disabled).toBe(true);
    expect(checkbox("Wait").checked).toBe(true);
    expect(answerBox().value).toBe("exact note");
    await act(async () => vi.advanceTimersByTime(REPLACED_GUARD_MS + 1));
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Retry the same answer" })));
    expect(api.take("answer_blocker").req).toEqual(first.req);
  });

  it("refuses a selected write when fresh server info lacks the capability despite cached support", async () => {
    await choicePage();
    api.auto.set("server_info", () => INFO);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Approve" })));
    expect(api.pending("answer_blocker")).toHaveLength(0);
    expect(screen.getByText(/cannot accept option selections/)).toBeTruthy();
    expect(loadKept()[1].state).toBe("refused");
  });

  it("keeps an earlier unconfirmed answer frozen when a retry's capability check fails", async () => {
    await choicePage();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Approve" })));
    const first = api.take("answer_blocker");
    await act(async () => first.reject(new api.ApiError("network", 0)));
    api.auto.set("server_info", () => INFO);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Retry the same answer" })));
    expect(api.pending("answer_blocker")).toHaveLength(0);
    expect(loadKept()[1].state).toBe("unconfirmed");
    expect(loadKept()[1].key).toBe(first.req.request_key);
    expect(answerBox().readOnly).toBe(true);
  });

  it("does not POST under another sign-in after the capability check finishes", async () => {
    await choicePage();
    api.auto.delete("server_info");
    fireEvent.change(answerBox(), { target: { value: "ada note" } });
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    const info = api.take("server_info");
    cleanup();
    setDraftOwner("bob");
    saveDraft("blocker:1", "bob note");
    await act(async () => info.resolve(choiceInfo));
    expect(api.pending("answer_blocker")).toHaveLength(0);
    expect(loadDraft("blocker:1", "bob")).toBe("bob note");
    expect(loadKept("bob")).toEqual({});
    expect(loadKept("ada")[1].state).toBe("refused");
  });

  it("keeps choice controls read-only when the blocker cannot be answered", async () => {
    await choicePage([{ ...choiceRow(), can_answer: false }]);
    expect((screen.getByRole("button", { name: "Approve" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(api.pending("answer_blocker")).toHaveLength(0);
    expect(api.pending("server_info")).toHaveLength(0);
  });
});
