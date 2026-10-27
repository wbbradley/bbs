import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { Nav } from "./Nav";
import { countCreators, sortAgents, useBoard, type LiveAgent } from "./store";
import { Board } from "./test/setup";
import type { Agent, AgentSummary, Blocker, ListBlockersResponse } from "./types";

const NOW = Date.parse("2026-10-27T12:00:00Z");
const before = (secs: number) => new Date(NOW - secs * 1000).toISOString();

const agent = (id: number, name: string, owner: string, extra: Partial<Agent> = {}): Agent => ({
  id,
  name,
  status: null,
  pwd: "/",
  hostname: "h",
  active_at: before(0),
  retired: false,
  groups: ["crew"],
  owner,
  last_sent_at: null,
  effort: null,
  effort_reported_at: null,
  last_human_conversation_at: null,
  // Every agent in @crew shares the group's open blockers.
  blocker_count: 4,
  ...extra,
});

const summary = (a: Agent, extra: Partial<AgentSummary> = {}): AgentSummary => ({
  agent: a.name,
  unread_messages: 0,
  oldest_unread_message: null,
  unseen_stream: 0,
  oldest_unseen_stream: null,
  last_sent_at: a.last_sent_at,
  effort: null,
  effort_reported_at: null,
  last_human_conversation_at: null,
  blocker_count: a.blocker_count,
  agent_id: a.id,
  ...extra,
});

let root = 0;
const blocker = (creator: { kind: 1 | 2; id: number; name: string }): Blocker => {
  root++;
  return {
    id: root,
    group_id: 1,
    group: "crew",
    creator: { ...creator, owner: null },
    body: `question ${root}?`,
    created_at: before(60),
    root_id: root,
    replaces: null,
    replaced_by: null,
    status: 1,
    outcome: null,
    can_answer: true,
    can_cancel: true,
  } as unknown as Blocker;
};

const JEFE = agent(1, "Jefe", "ada", { last_sent_at: before(30) });
const FRANK = agent(2, "Frank", "ada", { last_sent_at: before(2 * 3600 + 36 * 60) });
const ZED = agent(3, "Zed", "ada");
const MATEO = agent(7, "Mateo", "bob", { last_sent_at: before(60) });
const by = (a: Agent) => ({ kind: 1 as const, id: a.id!, name: a.name });

/** One answer per listing, by cursor; the same object each time, as a server that has nothing new. */
function listings(pages: Map<number, ListBlockersResponse | Error>) {
  let asked = 0;
  api.auto.set("list_blockers", (req) => {
    asked++;
    const page = pages.get(Number(req.after_root_id ?? 0));
    if (!page || page instanceof Error) throw page ?? new Error("no such page");
    return page;
  });
  return () => asked;
}

const page = (blockers: Blocker[], next: number | null = null): ListBlockersResponse => ({
  blockers,
  active_count: blockers.length,
  has_more: next !== null,
  next_after_root_id: next,
});

function Queue() {
  const board = useBoard();
  return <span data-testid="queue">{board.blockers?.blockers.length ?? "-"}</span>;
}

/** The names and right-hand labels of the sidebar's agent rows, in order. */
const rows = () =>
  [...document.querySelectorAll("a.nav-item")]
    .filter((a) => a.getAttribute("href")?.startsWith("/agents/"))
    .map((a) => a.textContent!.replace("◆", "").trim());

beforeEach(() => {
  api.reset();
  localStorage.clear();
  root = 0;
  vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
  vi.setSystemTime(NOW);
  const agents = { agents: [JEFE, FRANK, ZED, MATEO], humans: [] };
  api.auto.set("list_agents", () => agents);
  // Unchanged answers, so only the clock can draw the sidebar again.
  const groups = { groups: [] };
  api.auto.set("group_list", () => groups);
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

async function show(withNav = true) {
  render(
    <Board>
      {withNav && <Nav onSearch={() => {}} />}
      <Queue />
    </Board>,
  );
  await act(async () => {});
}

describe("the sidebar's agents", () => {
  it("flag only the blockers each agent opened, by its id, in alphabetical order", async () => {
    listings(
      new Map([
        [
          0,
          page([
            blocker(by(FRANK)),
            blocker(by(JEFE)),
            blocker(by(JEFE)),
            blocker(by(MATEO)),
            // A human's, and one from a retired agent once also named Zed.
            blocker({ kind: 2, id: 3, name: "ada" }),
            blocker({ kind: 1, id: 99, name: "Zed" }),
          ]),
        ],
      ]),
    );
    await show();
    expect(rows()).toEqual(["Frank⚑12h36m ago", "Jefe⚑2now", "Zed"]);
    // Others' agents keep their mail counts; their flags are their own too.
    act(() => api.stream("summary").push({ agents: [summary(MATEO, { unread_messages: 3 }), summary(ZED, { unread_messages: 4 })] }));
    fireEvent.click(screen.getByText("show 1"));
    expect(rows()).toEqual(["Frank⚑12h36m ago", "Jefe⚑2now", "Zed", "Mateo@bob⚑1✉3"]);
  });

  it("show how long ago each last sent or posted, keeping time with no requests of their own", async () => {
    const listed = listings(new Map([[0, page([])]]));
    let agentLists = 0;
    const agents = { agents: [JEFE, FRANK, ZED, MATEO], humans: [] };
    api.auto.set("list_agents", () => (agentLists++, agents));
    // The same minute on a board without the sidebar asks exactly as much.
    const asked = async (withNav: boolean) => {
      const from = [listed(), agentLists];
      await show(withNav);
      await act(async () => vi.advanceTimersByTime(60000));
      cleanup();
      return [listed() - from[0], agentLists - from[1]];
    };
    const without = await asked(false);
    expect(without[0]).toBeGreaterThan(0);
    vi.setSystemTime(NOW);
    expect(await asked(true)).toEqual(without);

    vi.setSystemTime(NOW);
    await show();
    expect(rows()).toEqual(["Frank2h36m ago", "Jefenow", "Zed"]);
    // The clock alone moves them on.
    await act(async () => vi.advanceTimersByTime(30000));
    expect(rows()).toEqual(["Frank2h36m ago", "Jefe1m ago", "Zed"]);
    // And the live summary brings news at once.
    act(() => api.stream("summary").push({ agents: [summary({ ...ZED, last_sent_at: new Date().toISOString() })] }));
    expect(rows()).toEqual(["Frank2h36m ago", "Jefe1m ago", "Zednow"]);
    await act(async () => vi.advanceTimersByTime(60000));
    expect(rows()).toEqual(["Frank2h37m ago", "Jefe2m ago", "Zed1m ago"]);
  });

  it("keeps mixed-case names alphabetical when blockers, activity and listed order change", async () => {
    const alpha = agent(10, "alpha", "ada");
    const beta = agent(11, "Beta", "ada");
    let agents = [ZED, beta, JEFE, alpha, FRANK, MATEO];
    api.auto.set("list_agents", () => ({ agents, humans: [] }));
    const pages = new Map([[0, page([blocker(by(ZED)), blocker(by(ZED)), blocker(by(JEFE))])]]);
    listings(pages);
    await show();
    const names = () => [...document.querySelectorAll("a.nav-item .label .agent")].map((a) => a.textContent);
    const alphabetical = ["alpha", "Beta", "Frank", "Jefe", "Zed"];
    expect(names()).toEqual(alphabetical);

    // The newest activity and the largest flag both move to different agents.
    pages.set(0, page([blocker(by(beta)), blocker(by(beta)), blocker(by(beta))]));
    act(() => api.stream("summary").push({ agents: [summary({ ...ZED, last_sent_at: new Date().toISOString() })] }));
    await act(async () => vi.advanceTimersByTime(3000));
    expect(names()).toEqual(alphabetical);
    expect(rows()).toContain("Beta⚑3");
    expect(rows()).toContain("Zednow");

    // A later list response reverses its order, and the age clock advances.
    agents = [...agents].reverse();
    await act(async () => vi.advanceTimersByTime(30000));
    expect(names()).toEqual(alphabetical);
    expect(rows()).toContain("Zednow");
  });

  it("count past the first page, keeping the last whole count when a later page fails or stalls", async () => {
    const first = Array.from({ length: 50 }, () => blocker(by(JEFE)));
    const rest = [...Array.from({ length: 5 }, () => blocker(by(JEFE))), blocker(by(FRANK))];
    const pages = new Map<number, ListBlockersResponse | Error>([
      [0, page(first, 50)],
      [50, page(rest)],
    ]);
    const asked = listings(pages);
    await show();
    expect(rows().slice(0, 2)).toEqual(["Frank⚑12h36m ago", "Jefe⚑55now"]);
    // The queue shows the first page as before.
    expect(screen.getByTestId("queue").textContent).toBe("50");
    expect(asked()).toBe(2);

    // A later page fails: the queue moves on, the counts stay whole.
    pages.set(0, page(first.slice(0, 40), 50));
    pages.set(50, new Error("The server could not be reached."));
    await act(async () => vi.advanceTimersByTime(3000));
    expect(screen.getByTestId("queue").textContent).toBe("40");
    expect(rows().slice(0, 2)).toEqual(["Frank⚑12h36m ago", "Jefe⚑55now"]);

    // A cursor that does not move on is not followed.
    pages.set(50, page(rest, 50));
    const before = asked();
    await act(async () => vi.advanceTimersByTime(3000));
    expect(asked() - before).toBe(2);
    expect(rows().slice(0, 2)).toEqual(["Frank⚑12h36m ago", "Jefe⚑55now"]);

    // Whole again: the new count.
    pages.set(50, page(rest));
    await act(async () => vi.advanceTimersByTime(3000));
    expect(rows().slice(0, 2)).toEqual(["Frank⚑12h36m ago", "Jefe⚑45now"]);
  });
});

describe("the board", () => {
  it("asks nothing more for blockers once signed out, though a listing was queued", async () => {
    const held: ((r: ListBlockersResponse) => void)[] = [];
    api.auto.set("list_blockers", () => new Promise((resolve) => held.push(resolve)));
    function Refresh() {
      const board = useBoard();
      return <button onClick={board.refreshBlockers}>refresh</button>;
    }
    render(
      <Board>
        <Refresh />
      </Board>,
    );
    await act(async () => {});
    expect(held).toHaveLength(1);
    // Asked for again while the first is out: it waits its turn.
    fireEvent.click(screen.getByText("refresh"));
    expect(held).toHaveLength(1);
    // Signed out, and in again: the new board lists for itself.
    cleanup();
    render(
      <Board>
        <Queue />
      </Board>,
    );
    await act(async () => {});
    expect(held).toHaveLength(2);
    // The old board's answer arrives: nothing shown, and nothing more asked.
    await act(async () => held[0](page([blocker(by(JEFE))], 1)));
    expect(held).toHaveLength(2);
    expect(screen.getByTestId("queue").textContent).toBe("-");
  });
});

describe("creator counts", () => {
  it("are given up when the board is gone before the last page arrives", async () => {
    const first = page([blocker(by(JEFE))], 1);
    let going = true;
    const counting = countCreators(first, () => going);
    const later = api.take("list_blockers");
    expect(later.req).toEqual({ mine: true, limit: 50, after_root_id: 1 });
    going = false;
    later.resolve(page([blocker(by(JEFE))]));
    expect(await counting).toBeNull();
  });

  it("leave the Agents page's order to the groups' blockers", () => {
    const live = (a: Agent, blocker_count: number): LiveAgent => ({ ...a, blocker_count, unread: 0, unseen: 0, oldestUnread: null });
    expect(sortAgents([live(JEFE, 1), live(ZED, 3)]).map((a) => a.name)).toEqual(["Zed", "Jefe"]);
    expect(sortAgents([live(JEFE, 1), live(ZED, 3)], (a) => (a.name === "Jefe" ? 2 : 0)).map((a) => a.name)).toEqual([
      "Jefe",
      "Zed",
    ]);
  });
});
