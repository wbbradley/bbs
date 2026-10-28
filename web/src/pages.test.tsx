import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { Dialog } from "./components/Dialog";
import { MessageView } from "./components/MessageView";
import { Launcher } from "./Launcher";
import { seedFound } from "./useMessages";
import { addLeaveGuard, go } from "./router";
import { Conversation } from "./pages/Conversation";
import { useBoard } from "./store";
import { Board, ADA } from "./test/setup";
import type { Agent, Message } from "./types";
import { useState } from "react";

const mail = (id: number, from: string, to: string, extra: Partial<Message> = {}): Message => ({
  id,
  created_at: `2026-10-26T12:${String(Math.floor(id / 60) % 60).padStart(2, "0")}:${String(id % 60).padStart(2, "0")}Z`,
  from_kind: from === "ada" || from === "bob" ? 2 : 1,
  from_name: from,
  to_group: null,
  to_name: to,
  read_at: null,
  body: `body ${id}`,
  from_owner: from === "Mateo" ? "ada" : null,
  reader_owner: null,
  to_owner: to === "Mateo" ? "ada" : null,
  control: null,
  timer_id: null,
  auto_reply: null,
  ...extra,
});

const mateo = { name: "Mateo", owner: "ada", id: 7, groups: [], status: null, effort: null } as unknown as Agent;

function Cover() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button onClick={() => setOpen(true)}>cover</button>
      {open && (
        <Dialog title="t" onClose={() => setOpen(false)}>
          over the page
        </Dialog>
      )}
    </>
  );
}

async function conversation(search = "") {
  go(`/agents/Mateo${search}`);
  api.auto.set("list_agents", () => ({ agents: [mateo], humans: [] }));
  api.auto.set("list_agent_notes", () => ({ notes: [], has_more: false, next_after_revision: 0 }));
  api.auto.set("side_report", () => ({}));
  render(
    <Board>
      <Cover />
      <Conversation agent="Mateo" openNav={() => {}} />
    </Board>,
  );
  await act(async () => {});
}

const feed = () => api.streams.filter((s) => s.open && s.kind === "feed" && s.req.with === "Mateo").at(-1)!;

beforeEach(() => {
  api.reset();
  localStorage.clear();
});
afterEach(cleanup);

describe("reading", () => {
  it("marks mail read when shown, but not beneath an overlay nor before a reconnect has sent what is new", async () => {
    await conversation();
    await act(async () => feed().push({ messages: [mail(1, "Mateo", "ada")] }, true));
    expect(api.take("mark_read").req).toEqual({ from: "Mateo" });
    fireEvent.click(screen.getByText("cover"));
    await act(async () => feed().push({ messages: [mail(1, "Mateo", "ada"), mail(2, "Mateo", "ada")] }));
    expect(api.pending("mark_read")).toHaveLength(0);
    fireEvent.keyDown(window, { key: "Escape" });
    await act(async () => {});
    expect(api.take("mark_read").req).toEqual({ from: "Mateo" });
    // The stream drops; what it held is not read again until it is live.
    await act(async () => feed().setState({ state: "retrying", error: "connection lost" }));
    await act(async () => {});
    expect(api.pending("mark_read")).toHaveLength(0);
  });
});

describe("an overlay opening with new mail", () => {
  it("keeps that mail unread when both come in one update", async () => {
    await conversation();
    await act(async () => feed().push({ messages: [mail(1, "Mateo", "ada", { read_at: "x" })] }, true));
    await act(async () => {
      fireEvent.click(screen.getByText("cover"));
      feed().push({ messages: [mail(1, "Mateo", "ada", { read_at: "x" }), mail(2, "Mateo", "ada")] });
    });
    expect(api.pending("mark_read")).toHaveLength(0);
    fireEvent.keyDown(window, { key: "Escape" });
    await act(async () => {});
    expect(api.take("mark_read").req).toEqual({ from: "Mateo" });
  });
});

describe("reconnects", () => {
  it("keep the activity already shown and mark where records may be missing", async () => {
    await conversation();
    const rec = (id: number, text: string) => ({
      id,
      created_at: `2026-10-26T12:00:${String(id % 60).padStart(2, "0")}Z`,
      record: JSON.stringify({ event: "text", text }),
    });
    const activity = () => api.streams.filter((s) => s.open && s.kind === "activity").at(-1)!;
    await act(async () => activity().push({ events: [rec(1, "early thought")] }, true));
    // A new connection's first window, full and starting well past what is held.
    const window = Array.from({ length: 200 }, (_, i) => rec(1000 + i, `later ${i}`));
    await act(async () => activity().push({ events: window }, true));
    expect(screen.getByText("early thought")).toBeTruthy();
    expect(screen.getByText(/activity may be missing here/)).toBeTruthy();
  });
});

describe("a message found by search", () => {
  it("reads its context when the live window does not hold it, then lets go at the newest", async () => {
    await conversation("?m=50");
    const live = Array.from({ length: 300 }, (_, i) => mail(400 + i, "Mateo", "ada", { read_at: "x" }));
    await act(async () => feed().push({ messages: live }, true));
    // Held: nothing is read while a found message holds the view.
    expect(api.pending("mark_read")).toHaveLength(0);
    const after = api.take("watch:feed");
    expect(after.req).toMatchObject({ with: "Mateo", after_id: 49, backlog: 50 });
    const context = Array.from({ length: 50 }, (_, i) => mail(50 + i, "Mateo", "ada"));
    await act(async () => after.resolve({ messages: context }));
    const before = api.take("watch:feed");
    expect(before.req).toMatchObject({ with: "Mateo", until: context[0].created_at, backlog: 50 });
    await act(async () => before.resolve({ messages: [mail(49, "Mateo", "ada")] }));
    expect(screen.getByText("body 50")).toBeTruthy();
    expect(document.getElementById("msg-50")!.className).toContain("highlight");
    expect(screen.getByText(/messages may be missing here/)).toBeTruthy();
    // Back at the newest, the view is the reader's again.
    await act(async () => fireEvent.scroll(document.querySelector(".timeline")!));
    expect(location.search).toBe("");
  });

  it("shows a message the launcher found at once, and reads both sides of it together", async () => {
    const found = mail(50, "Mateo", "ada", { body: "the found one" });
    seedFound(found);
    await conversation("?m=50");
    await act(async () => feed().push({ messages: [mail(400, "Mateo", "ada", { read_at: "x" })] }, true));
    expect(screen.getByText("the found one")).toBeTruthy();
    const reads = api.pending("watch:feed").map((c) => c.req);
    expect(reads).toHaveLength(2);
    expect(reads).toContainEqual(expect.objectContaining({ until: found.created_at, backlog: 50 }));
    expect(reads).toContainEqual(expect.objectContaining({ after_id: 49, backlog: 50 }));
  });

  it("replaces a found message's cut-short search body with the full message", async () => {
    seedFound(mail(50, "Mateo", "ada", { body: "the beginning of a long" }));
    await conversation("?m=50");
    await act(async () => feed().push({ messages: [mail(400, "Mateo", "ada", { read_at: "x" })] }, true));
    expect(screen.getByText("the beginning of a long")).toBeTruthy();
    const after = api.pending("watch:feed").find((c) => c.req.after_id === 49)!;
    await act(async () =>
      after.resolve({ messages: [mail(50, "Mateo", "ada", { body: "the beginning of a long message, in full" })] }),
    );
    await act(async () => api.take("watch:feed").resolve({ messages: [] }));
    expect(screen.getByText("the beginning of a long message, in full")).toBeTruthy();
    expect(screen.queryByText("the beginning of a long")).toBeNull();
  });

  it("opens an old found message even after a long session holds the most rows a page keeps", async () => {
    await conversation("?m=1");
    for (let w = 0; w < 10; w++) {
      const window = Array.from({ length: 300 }, (_, i) => mail(1000 + w * 300 + i, "Mateo", "ada", { read_at: "x" }));
      await act(async () => feed().push({ messages: window }));
    }
    const after = api.take("watch:feed");
    await act(async () =>
      after.resolve({ messages: Array.from({ length: 50 }, (_, i) => mail(1 + i, "Mateo", "ada", { read_at: "x" })) }),
    );
    await act(async () => api.take("watch:feed").resolve({ messages: [] }));
    expect(document.getElementById("msg-1")).toBeTruthy();
    expect(document.getElementById("msg-1")!.className).toContain("highlight");
    expect(screen.queryByText("that message is not in this view")).toBeNull();
    expect(screen.getAllByText(/messages may be missing here/).length).toBeGreaterThan(0);
  });

  it("drops a found message that turns out not to belong to this page", async () => {
    seedFound(mail(50, "Zed", "bob", { body: "someone else's" }));
    await conversation("?m=50");
    await act(async () => feed().push({ messages: [mail(400, "Mateo", "ada", { read_at: "x" })] }, true));
    expect(screen.getByText("someone else's")).toBeTruthy();
    const after = api.pending("watch:feed").find((c) => c.req.after_id === 49)!;
    await act(async () => after.resolve({ messages: [mail(60, "Mateo", "ada")] }));
    await act(async () => api.pending("watch:feed").find((c) => "until" in c.req)!.resolve({ messages: [] }));
    expect(screen.queryByText("someone else's")).toBeNull();
    expect(screen.getByText("that message is not in this view")).toBeTruthy();
  });

  it("says when the found message is not in this view", async () => {
    await conversation("?m=50");
    await act(async () => feed().push({ messages: [mail(400, "Mateo", "ada")] }, true));
    await act(async () => api.take("watch:feed").resolve({ messages: [mail(60, "Mateo", "ada")] }));
    expect(screen.getByText("that message is not in this view")).toBeTruthy();
  });
});

describe("the launcher", () => {
  it("finds pages without server search and keeps a same-name agent distinct", async () => {
    api.auto.set("list_agents", () => ({ agents: [{ ...mateo, name: "Feed" }], humans: [] }));
    go("/agents/Mateo");
    const close = vi.fn();
    render(<Board info={null}><Launcher onClose={close} /></Board>);
    await act(async () => {});
    const input = screen.getByPlaceholderText("Pages, agents, groups, notes, messages…");
    expect([...document.querySelectorAll(".launcher-item .kind")].slice(0, 5).map((e) => e.textContent))
      .toEqual(["page", "page", "page", "page", "page"]);
    fireEvent.change(input, { target: { value: "Feed" } });
    expect([...document.querySelectorAll(".launcher-item .kind")].map((e) => e.textContent)).toEqual(["page", "agent"]);
    await act(async () => fireEvent.keyDown(input, { key: "Enter" }));
    expect(location.pathname).toBe("/feed");
    fireEvent.keyDown(input, { key: "ArrowDown" });
    await act(async () => fireEvent.keyDown(input, { key: "Enter" }));
    expect(location.pathname).toBe("/agents/Feed");
    expect(close).toHaveBeenCalledTimes(2);
    expect(api.pending("search")).toHaveLength(0);
  });

  it("fuzzy page navigation respects a refused leave guard", async () => {
    go("/agents/Mateo");
    render(<Board info={null}><Launcher onClose={() => {}} /></Board>);
    await act(async () => {});
    const guard = vi.fn(async () => false);
    const remove = addLeaveGuard(guard);
    try {
      const input = screen.getByPlaceholderText("Pages, agents, groups, notes, messages…");
      fireEvent.change(input, { target: { value: "blkr" } });
      expect(document.querySelector(".launcher-item")?.textContent).toContain("Blockers");
      await act(async () => fireEvent.keyDown(input, { key: "Enter" }));
      expect(guard).toHaveBeenCalledTimes(1);
      expect(location.pathname).toBe("/agents/Mateo");
    } finally {
      remove();
    }
  });

  function Launch() {
    const board = useBoard();
    return (
      <>
        <button onClick={board.refreshBlockers}>poll</button>
        <Launcher onClose={() => {}} />
      </>
    );
  }

  it("shows only the newest query's hits and does not search again for other reasons", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      render(
        <Board>
          <Launch />
        </Board>,
      );
      await act(async () => {});
      const input = screen.getByPlaceholderText("Pages, agents, groups, notes, messages…");
      fireEvent.change(input, { target: { value: "ab" } });
      await act(async () => vi.advanceTimersByTime(250));
      fireEvent.change(input, { target: { value: "abc" } });
      await act(async () => vi.advanceTimersByTime(250));
      const [older, newer] = api.pending("search");
      await act(async () => newer.resolve({ hits: [{ message: mail(2, "Mateo", "ada", { body: "abc newer" }) }] }));
      await act(async () => older.resolve({ hits: [{ message: mail(1, "Mateo", "ada", { body: "ab older" }) }] }));
      expect(screen.getByText(/newer/)).toBeTruthy();
      expect(screen.queryByText(/older/)).toBeNull();
      api.take("search");
      api.take("search");
      // The board changing underneath sends nothing.
      fireEvent.click(screen.getByText("poll"));
      await act(async () => vi.advanceTimersByTime(1000));
      expect(api.pending("search")).toHaveLength(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("leaves Ctrl-J and Ctrl-K to the browser, with arrow keys for selection", async () => {
    render(
      <Board>
        <Launcher onClose={() => {}} />
      </Board>,
    );
    await act(async () => {});
    const input = screen.getByPlaceholderText("Pages, agents, groups, notes, messages…");
    fireEvent.keyDown(input, { key: "ArrowDown" });
    const selected = document.querySelector(".launcher-item.sel")!.textContent;
    const outside = vi.fn();
    window.addEventListener("keydown", outside);
    for (const key of ["j", "k"]) {
      const control = new KeyboardEvent("keydown", { key, ctrlKey: true, bubbles: true, cancelable: true });
      fireEvent(input, control);
      expect(control.defaultPrevented).toBe(false);
      expect(document.querySelector(".launcher-item.sel")!.textContent).toBe(selected);
    }
    window.removeEventListener("keydown", outside);
    expect(outside).toHaveBeenCalledTimes(2);
    fireEvent.keyDown(input, { key: "ArrowUp" });
    expect(document.querySelector(".launcher-item.sel")!.textContent).not.toBe(selected);
  });
});

describe("feed cards", () => {
  it("name senders bare, keep your own on your surface and mark unread direct mail", () => {
    render(
      <>
        <MessageView m={mail(1, "ada", "Mateo")} me={ADA.name} mode={{ kind: "feed" }} />
        <MessageView m={mail(2, "Mateo", "bob", { read_at: "2026-10-26T13:00:00Z" })} me={ADA.name} mode={{ kind: "feed" }} />
        <MessageView m={mail(3, "Mateo", "", { to_name: null, to_group: "crew" })} me={ADA.name} mode={{ kind: "feed" }} />
      </>,
    );
    const own = document.getElementById("msg-1")!;
    expect(own.className).toContain("own");
    expect(own.textContent).toContain("unread");
    expect(document.body.textContent).not.toContain("@ada");
    expect(document.getElementById("msg-2")!.textContent).not.toContain("unread");
    expect(document.getElementById("msg-3")!.className).toContain("groupmail");
    expect(document.getElementById("msg-3")!.textContent).not.toContain("unread");
  });
});
