import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { go } from "./router";
import { TIMERS_EDIT, TimerPage, TimersPage, fromLocalInput, toLocalInput } from "./pages/Timers";
import { Board, INFO } from "./test/setup";
import type { Timer } from "./types";
import { repeatLabel, until } from "./util";

const HOUR = 3600 * 1000;
const at = (ms: number) => new Date(Date.now() + ms).toISOString();

const timer = (id: number, extra: Partial<Timer> = {}): Timer => ({
  id,
  agent: "Mateo",
  creator: "ada",
  repeat: "every 1h",
  next_run: at(42 * 60 * 1000 + 30000),
  body: `check CI ${id}\nsecond line`,
  created_at: "2026-11-01T09:00:00.000000Z",
  ...extra,
});

const EDIT_INFO = { ...INFO, capabilities: [...INFO.capabilities, TIMERS_EDIT] };

beforeEach(() => {
  api.reset();
  localStorage.clear();
});
afterEach(cleanup);

describe("formatting", () => {
  it("says how long until a time to come", () => {
    const now = Date.parse("2026-11-06T12:00:00Z");
    expect(until("2026-11-06T12:00:30Z", now)).toBe("now");
    expect(until("2026-11-06T12:42:00Z", now)).toBe("in 42m");
    expect(until("2026-11-09T16:10:00Z", now)).toBe("in 3d4h");
    expect(until("2026-11-06T11:00:00Z", now)).toBe("now");
  });

  it("shows a repeat as every span, a cron expression as written, or once", () => {
    expect(repeatLabel(null)).toBe("once");
    expect(repeatLabel("every 1h")).toBe("every 1h");
    expect(repeatLabel("every 1h 30m")).toBe("every 1h30m");
    expect(repeatLabel("every 2days")).toBe("every 2d");
    expect(repeatLabel("every 1day 2h 30m")).toBe("every 1d2h");
    expect(repeatLabel("0 9 * * 1-5")).toBe("0 9 * * 1-5");
    expect(repeatLabel("every 3fortnights")).toBe("every 3fortnights");
  });

  it("round-trips local input times", () => {
    const iso = "2026-11-06T09:05:00.000Z";
    expect(fromLocalInput(toLocalInput(iso))).toBe(iso);
    expect(fromLocalInput("")).toBeNull();
  });
});

async function list(timers: Timer[]) {
  go("/timers");
  api.auto.set("timer_list", () => ({ timers }));
  render(
    <Board>
      <TimersPage openNav={() => {}} />
    </Board>,
  );
  await act(async () => {});
}

describe("Timers list", () => {
  it("lists every timer with its first line, next run and repeat", async () => {
    await list([timer(12), timer(13, { repeat: null, creator: "Jules" })]);
    const rows = document.querySelectorAll("tbody tr");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("12");
    expect(rows[0].textContent).toContain("Mateo");
    expect(rows[0].textContent).toContain("check CI 12");
    expect(rows[0].textContent).not.toContain("second line");
    expect(rows[0].textContent).toContain("in 42m");
    expect(rows[0].textContent).toContain("every 1h");
    expect(rows[1].textContent).toContain("once");
    expect(screen.getAllByRole("link", { name: "Mateo" })[0].getAttribute("href")).toBe("/agents/Mateo");
  });

  it("opens a timer from its row", async () => {
    await list([timer(12)]);
    await act(async () => fireEvent.click(screen.getByText("check CI 12")));
    expect(location.pathname).toBe("/timers/12");
  });

  it("says how agents set timers when there are none", async () => {
    await list([]);
    expect(screen.getByText(/No timers/).textContent).toContain("bbs timer add");
  });
});

async function detail(timers: Timer[], info = EDIT_INFO) {
  go("/timers/12");
  api.auto.set("timer_list", () => ({ timers }));
  render(
    <Board info={info}>
      <TimerPage id={12} openNav={() => {}} />
    </Board>,
  );
  await act(async () => {});
}

describe("Timer details", () => {
  it("saves only the fields that changed", async () => {
    await detail([timer(12)]);
    const save = screen.getByRole("button", { name: "Save" }) as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Text"), { target: { value: "check the deploy" } });
    fireEvent.change(screen.getByLabelText("Repeat"), { target: { value: " every 2h " } });
    expect(save.disabled).toBe(false);
    fireEvent.click(save);
    const call = api.take("timer_update");
    expect(call.req).toEqual({ id: 12, body: "check the deploy", repeat: "every 2h" });
    await act(async () => call.resolve(timer(12)));
  });

  it("sends a chosen next run in UTC", async () => {
    await detail([timer(12)]);
    const local = toLocalInput(at(5 * HOUR));
    fireEvent.change(screen.getByLabelText("Next run (your local time)"), { target: { value: local } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    const call = api.take("timer_update");
    expect(call.req).toEqual({ id: 12, next_run: fromLocalInput(local) });
  });

  it("keeps the human's edits while the timer changes underneath", async () => {
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    try {
      await detail([timer(12)]);
      fireEvent.change(screen.getByLabelText("Text"), { target: { value: "mine" } });
      const moved = timer(12, { next_run: at(5 * HOUR), body: "theirs" });
      api.auto.set("timer_list", () => ({ timers: [moved] }));
      await act(async () => vi.advanceTimersByTime(30000));
      expect((screen.getByLabelText("Text") as HTMLTextAreaElement).value).toBe("mine");
      expect((screen.getByLabelText("Next run (your local time)") as HTMLInputElement).value).toBe(
        toLocalInput(moved.next_run),
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it("cancels only after confirming, then returns to the list", async () => {
    await detail([timer(12)]);
    fireEvent.click(screen.getByRole("button", { name: "Cancel timer" }));
    expect(api.pending("timer_remove")).toHaveLength(0);
    expect(screen.getByText(/Mateo will be told/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel timer" }));
    const call = api.take("timer_remove");
    expect(call.req).toEqual({ id: 12 });
    await act(async () => call.resolve(timer(12)));
    expect(location.pathname).toBe("/timers");
  });

  it("says when the timer is gone", async () => {
    await detail([timer(13)]);
    expect(screen.getByText(/Timer #12 no longer exists/)).toBeTruthy();
  });

  it("treats a save refused for a missing timer as gone", async () => {
    await detail([timer(12)]);
    fireEvent.change(screen.getByLabelText("Text"), { target: { value: "x" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await act(async () => api.take("timer_update").reject(new api.ApiError("no timer 12", 400)));
    expect(screen.getByText(/Timer #12 no longer exists/)).toBeTruthy();
  });

  it("offers only cancelling on a server that cannot edit", async () => {
    await detail([timer(12)], INFO);
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    expect((screen.getByLabelText("Text") as HTMLTextAreaElement).disabled).toBe(true);
    expect(screen.getByRole("button", { name: "Cancel timer" })).toBeTruthy();
  });
});
