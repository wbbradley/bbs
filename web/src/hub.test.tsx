import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { onSignedOut, useWatch, watchKey, type WatchState } from "./api";

/** EventSources the page opened, newest last. */
class FakeSource {
  static all: FakeSource[] = [];
  closed = false;
  onmessage: ((e: MessageEvent) => void) | null = null;
  onerror: (() => void) | null = null;
  private fails: ((e: MessageEvent) => void)[] = [];
  constructor(readonly url: string) {
    FakeSource.all.push(this);
  }
  get subs(): { key: string; kind: string; q: unknown }[] {
    return JSON.parse(decodeURIComponent(this.url.split("?s=")[1]));
  }
  addEventListener(_: string, f: (e: MessageEvent) => void) {
    this.fails.push(f);
  }
  close() {
    this.closed = true;
  }
  send(key: string, snapshot: unknown) {
    this.onmessage?.(new MessageEvent("message", { data: JSON.stringify({ key, snapshot }) }));
  }
  fail(key: string | null, status: number, error = "no") {
    this.fails.forEach((f) => f(new MessageEvent("fail", { data: JSON.stringify({ key, status, error }) })));
  }
}
const open = () => FakeSource.all.filter((s) => !s.closed);

interface Seen {
  snaps: [unknown, boolean][];
  state?: WatchState;
}
const seen = (): Seen => ({ snaps: [] });

function View({ kind, q, seen }: { kind: string; q: object; seen: Seen }) {
  seen.state = useWatch(kind, q, (snap, fresh) => seen.snaps.push([snap, fresh]));
  return null;
}

beforeEach(() => {
  FakeSource.all = [];
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  vi.stubGlobal("EventSource", FakeSource);
  vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({ name: "ada" }), { status: 200 })));
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const settle = () => act(async () => vi.advanceTimersByTime(150));

describe("one event stream per tab", () => {
  it("carries every watch, reopening once the set settles, each key fresh after an open", async () => {
    const a = seen();
    const b = seen();
    const { rerender } = render(<View kind="summary" q={{}} seen={a} />);
    await settle();
    expect(open()).toHaveLength(1);
    const ka = watchKey("summary", "{}");
    expect(ka.length).toBeLessThanOrEqual(64);
    act(() => open()[0].send(ka, 1));
    act(() => open()[0].send(ka, 2));
    expect(a.snaps).toEqual([
      [1, true],
      [2, false],
    ]);
    rerender(
      <>
        <View kind="summary" q={{}} seen={a} />
        <View kind="feed" q={{ with: "Mateo" }} seen={b} />
      </>,
    );
    await settle();
    expect(open()).toHaveLength(1);
    expect(open()[0].subs.map((s) => s.kind).sort()).toEqual(["feed", "summary"]);
    act(() => open()[0].send(ka, 3));
    expect(a.snaps.at(-1)).toEqual([3, true]);
  });

  it("shares a watch between views, starting a joining view from its last snapshot", async () => {
    const a = seen();
    const b = seen();
    const { rerender } = render(<View kind="summary" q={{}} seen={a} />);
    await settle();
    act(() => open()[0].send(watchKey("summary", "{}"), "now"));
    rerender(
      <>
        <View kind="summary" q={{}} seen={a} />
        <View kind="summary" q={{}} seen={b} />
      </>,
    );
    await settle();
    expect(open()[0].subs).toHaveLength(1);
    expect(b.snaps).toEqual([["now", true]]);
  });

  it("ends a refused key alone, and signs out when the session ends", async () => {
    const a = seen();
    const b = seen();
    render(
      <>
        <View kind="summary" q={{}} seen={a} />
        <View kind="feed" q={{ group: "nope" }} seen={b} />
      </>,
    );
    await settle();
    act(() => open()[0].fail(watchKey("feed", JSON.stringify({ group: "nope" })), 400, "no such group"));
    expect(b.state).toEqual({ state: "failed", error: "no such group" });
    expect(a.state).toEqual({ state: "connecting" });
    const out = vi.fn();
    const stop = onSignedOut(out);
    act(() => open()[0].fail(null, 401, "session ended"));
    expect(out).toHaveBeenCalled();
    expect(open()).toHaveLength(0);
    stop();
  });

  it("asks for the stream again after it drops, with every watch waiting meanwhile", async () => {
    const a = seen();
    render(<View kind="summary" q={{}} seen={a} />);
    await settle();
    await act(async () => open()[0].onerror?.());
    expect(a.state).toEqual({ state: "retrying", error: "connection lost" });
    await act(async () => vi.advanceTimersByTime(1100));
    expect(open()).toHaveLength(1);
    act(() => open()[0].send(watchKey("summary", "{}"), "back"));
    expect(a.snaps.at(-1)).toEqual(["back", true]);
  });
});
