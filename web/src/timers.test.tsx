import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { TimerDrawer } from "./components/TimerDrawer";
import { Conversation } from "./pages/Conversation";
import { Board, INFO } from "./test/setup";
import type { Agent, Timer } from "./types";

const timer = (extra: Partial<Timer> = {}): Timer => ({
  id: 558, agent: "Mateo", creator: "Mateo", repeat: "0 5 * * *",
  next_run: "2026-11-01T05:00:00Z", body: "Check the queue.\nReport findings.", ...extra,
});

beforeEach(() => {
  api.reset();
  localStorage.clear();
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
});
afterEach(() => { cleanup(); vi.useRealTimers(); });

describe("agent timers", () => {
  it("reads schedules from the server, updates removals, and stops polling when closed", async () => {
    const view = render(<TimerDrawer agent="Mateo" onClose={() => {}} />);
    expect(screen.getByRole("status").textContent).toContain("Loading");
    const request = api.take("timer_list");
    expect(request.req).toEqual({ agent: "Mateo" });
    await act(async () => request.resolve({ timers: [timer(), timer({ id: 559, repeat: null, creator: "ada", body: "Once only" })] }));
    expect(screen.getByText("0 5 * * * (UTC)")).toBeTruthy();
    expect(screen.getByText(/Timer #559 · One-time/)).toBeTruthy();
    expect(screen.getByText("ada")).toBeTruthy();
    expect(screen.getByText(/Check the queue/).textContent).toBe("Check the queue.\nReport findings.");
    await act(async () => vi.advanceTimersByTime(15000));
    await act(async () => api.take("timer_list").resolve({ timers: [] }));
    expect(screen.getByText("No timers set for Mateo.")).toBeTruthy();
    expect(screen.queryByText("Once only")).toBeNull();
    expect(api.pending("send")).toHaveLength(0);
    view.unmount();
    await act(async () => vi.advanceTimersByTime(30000));
    expect(api.pending("timer_list")).toHaveLength(0);
  });

  it("keeps a failed refresh visibly stale, and a manual retry can recover", async () => {
    render(<TimerDrawer agent="Mateo" onClose={() => {}} />);
    await act(async () => api.take("timer_list").resolve({ timers: [timer()] }));
    fireEvent.click(screen.getByText("Refresh"));
    await act(async () => api.take("timer_list").reject(new api.ApiError("not allowed", 403)));
    expect(screen.getByRole("alert").textContent).toContain("Showing the last successful list");
    expect(screen.getByText(/Timer #558/)).toBeTruthy();
    await act(async () => vi.advanceTimersByTime(30000));
    expect(api.pending("timer_list")).toHaveLength(0);
    fireEvent.click(screen.getByText("Retry"));
    await act(async () => api.take("timer_list").resolve({ timers: [] }));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByText("No timers set for Mateo.")).toBeTruthy();
  });

  it("ignores a late response from a previous conversation", async () => {
    const view = render(<TimerDrawer key="Mateo" agent="Mateo" onClose={() => {}} />);
    const old = api.take("timer_list");
    view.rerender(<TimerDrawer key="Zed" agent="Zed" onClose={() => {}} />);
    await act(async () => api.take("timer_list").resolve({ timers: [] }));
    await act(async () => old.resolve({ timers: [timer()] }));
    expect(screen.getByText("No timers set for Zed.")).toBeTruthy();
    expect(screen.queryByText(/Timer #558/)).toBeNull();
  });

  it("pauses while the tab is hidden and reloads when visible again", async () => {
    render(<TimerDrawer agent="Mateo" onClose={() => {}} />);
    await act(async () => api.take("timer_list").resolve({ timers: [] }));
    const visibility = vi.spyOn(document, "visibilityState", "get");
    try {
      visibility.mockReturnValue("hidden");
      fireEvent(document, new Event("visibilitychange"));
      await act(async () => vi.advanceTimersByTime(30000));
      expect(api.pending("timer_list")).toHaveLength(0);
      visibility.mockReturnValue("visible");
      fireEvent(document, new Event("visibilitychange"));
      await act(async () => api.take("timer_list").resolve({ timers: [timer()] }));
      expect(screen.getByText(/Timer #558/)).toBeTruthy();
    } finally { visibility.mockRestore(); }
  });

  it("opens on demand and preserves a note if saving it fails before switching panels", async () => {
    api.auto.set("list_agents", () => ({ agents: [{ name: "Mateo", id: 7, groups: [], owner: "ada", effort: null } as unknown as Agent], humans: [] }));
    api.auto.set("list_agent_notes", () => ({ notes: [], has_more: false, next_after_revision: 0 }));
    api.auto.set("side_report", () => ({}));
    const view = render(<Board info={{ ...INFO, capabilities: [...INFO.capabilities, "timers.v1"] }}><Conversation agent="Mateo" openNav={() => {}} /></Board>);
    await act(async () => {});
    expect(api.pending("timer_list")).toHaveLength(0);
    fireEvent.click(screen.getByText("Note"));
    const note = screen.getByPlaceholderText(/Private to you/);
    fireEvent.change(note, { target: { value: "Keep this note" } });
    fireEvent.click(screen.getByText("Timers"));
    await act(async () => api.take("put_agent_note").reject(new Error("save failed")));
    expect((note as HTMLTextAreaElement).value).toBe("Keep this note");
    expect(api.pending("timer_list")).toHaveLength(0);
    fireEvent.click(screen.getByText("Timers"));
    await act(async () => api.take("put_agent_note").resolve({ note: { agent_id: 7, body: "Keep this note", revision: 1 } }));
    await act(async () => api.take("timer_list").resolve({ timers: [] }));
    expect(screen.queryByPlaceholderText(/Private to you/)).toBeNull();
    expect(screen.getByRole("complementary", { name: "Timers for Mateo" })).toBeTruthy();
    fireEvent.click(screen.getByText("Close"));
    expect(screen.queryByRole("complementary")).toBeNull();
    view.unmount();
    render(<Board><Conversation agent="Mateo" openNav={() => {}} /></Board>);
    await act(async () => {});
    expect(screen.queryByText("Timers")).toBeNull();
  });
});
