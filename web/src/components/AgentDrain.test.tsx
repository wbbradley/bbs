import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
vi.mock("../api", () => import("../test/fakeApi"));
import * as api from "../test/fakeApi";
import { AgentDrain } from "./AgentDrain";

beforeEach(() => { api.reset(); vi.useFakeTimers(); });
afterEach(() => { cleanup(); vi.useRealTimers(); });
const running = { frozen: false, status: "running", watermark: 0, deadline: "", agents: [] };
const draining = { frozen: true, status: "draining", watermark: 42, deadline: "2026-10-27T21:00:00Z", agents: [{ agent_id: 1, name: "Mateo", unread: 0, completed: false, side_pending: false }] };

it("starts a drain, distinguishes read from finished, and requires manual resume after ready", async () => {
  render(<AgentDrain />);
  await act(async () => api.take("agent_drain").resolve(running));
  fireEvent.click(screen.getByText("Prepare agents for restart"));
  const start = api.take("agent_drain");
  expect(start.req).toEqual({ action: "start" });
  await act(async () => start.resolve(draining));
  expect(screen.getByText(/waiting for turn completion/)).toBeTruthy();
  await act(async () => vi.advanceTimersByTime(5000));
  await act(async () => api.take("agent_drain").resolve({ ...draining, status: "ready", agents: [{ ...draining.agents[0], completed: true }] }));
  expect(screen.getByRole("status").textContent).toContain("Safe to restart");
  expect(api.pending("agent_drain")).toHaveLength(0);
  fireEvent.click(screen.getByText("Resume delivery"));
  const resume = api.take("agent_drain");
  expect(resume.req).toEqual({ action: "resume" });
  await act(async () => resume.resolve(running));
  expect(screen.getByRole("status").textContent).toContain("Delivery is running");
});

it("shows timeout stragglers and retains resume after a failed request", async () => {
  render(<AgentDrain />);
  await act(async () => api.take("agent_drain").resolve({ ...draining, status: "timed_out" }));
  expect(screen.getByRole("status").textContent).toContain("20-minute wait ended");
  fireEvent.click(screen.getByText("Resume delivery"));
  await act(async () => api.take("agent_drain").reject(new Error("offline")));
  expect(screen.getByRole("alert").textContent).toContain("offline");
  expect(screen.getByText("Resume delivery")).toBeTruthy();
});

it("does not let a stale status response overwrite a successful start", async () => {
  render(<AgentDrain />);
  await act(async () => api.take("agent_drain").resolve(running));
  await act(async () => vi.advanceTimersByTime(5000));
  const stale = api.take("agent_drain");
  fireEvent.click(screen.getByText("Prepare agents for restart"));
  await act(async () => api.take("agent_drain").resolve(draining));
  await act(async () => stale.resolve(running));
  expect(screen.getByText("Resume delivery")).toBeTruthy();
});

it("shows side questions still running after the mail turn completes", async () => {
  render(<AgentDrain />);
  await act(async () => api.take("agent_drain").resolve({ ...draining, agents: [{ ...draining.agents[0], completed: true, side_pending: true }] }));
  expect(screen.getByText(/waiting for side question completion/)).toBeTruthy();
});
