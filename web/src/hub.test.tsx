import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { onSignedOut, useWatch, watchKey, watchOnce, type WatchState } from "./api";

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
    return JSON.parse(decodeURIComponent(this.url.split("&s=")[1]));
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

function View({ kind, q, seen, resume }: { kind: string; q: object; seen: Seen; resume?: () => object | null }) {
  seen.state = useWatch(kind, q, (snap, fresh) => seen.snaps.push([snap, fresh]), resume);
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
    const ka = watchKey("summary", {})!;
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
    act(() => open()[0].send(watchKey("summary", {})!, "now"));
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
    act(() => open()[0].fail(watchKey("feed", { group: "nope" })!, 400, "no such group"));
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
    act(() => open()[0].send(watchKey("summary", {})!, "back"));
    expect(a.snaps.at(-1)).toEqual(["back", true]);
  });

  it("carries a resumable watch on after what it holds when the stream reopens", async () => {
    const a = seen();
    render(<View kind="activity" q={{ agent: "Mateo", backlog: 200 }} seen={a} resume={() => ({ agent: "Mateo", backlog: 200, after_id: 41 })} />);
    await settle();
    expect(open()[0].subs[0].q).toEqual({ agent: "Mateo", backlog: 200 });
    const key = open()[0].subs[0].key;
    act(() => open()[0].send(key, "window"));
    await act(async () => open()[0].onerror?.());
    await act(async () => vi.advanceTimersByTime(1100));
    expect(open()[0].subs[0].q).toEqual({ agent: "Mateo", backlog: 200, after_id: 41 });
    act(() => open()[0].send(key, "more"));
    // Carried on, not afresh.
    expect(a.snaps).toEqual([
      ["window", true],
      ["more", false],
    ]);
  });

  it("reads a one-shot over the same stream, then leaves it", async () => {
    const a = seen();
    render(<View kind="summary" q={{}} seen={a} />);
    await settle();
    let got: unknown;
    void watchOnce("feed", { with: "Mateo", after_id: 49, backlog: 50 }).then((r) => (got = r));
    await settle();
    expect(FakeSource.all.filter((x) => !x.closed)).toHaveLength(1);
    const once = open()[0].subs.find((x) => x.kind === "feed")!;
    act(() => open()[0].send(once.key, { messages: [1] }));
    await act(async () => {});
    expect(got).toEqual({ messages: [1] });
    await settle();
    expect(open()[0].subs.map((x) => x.kind)).toEqual(["summary"]);
  });

  it("names its tab on every stream, the same across reopens", async () => {
    const a = seen();
    const { rerender } = render(<View kind="summary" q={{}} seen={a} />);
    await settle();
    const tab = new URL(open()[0].url, "http://x").searchParams.get("tab");
    expect(tab).toMatch(/^[0-9a-f]{32}$/);
    rerender(
      <>
        <View kind="summary" q={{}} seen={a} />
        <View kind="feed" q={{ with: "Zed" }} seen={seen()} />
      </>,
    );
    await settle();
    expect(new URL(open()[0].url, "http://x").searchParams.get("tab")).toBe(tab);
  });

  it("drops events still queued from a stream it has closed", async () => {
    const a = seen();
    const { rerender } = render(<View kind="summary" q={{}} seen={a} />);
    await settle();
    const old = open()[0];
    const key = watchKey("summary", {})!;
    rerender(
      <>
        <View kind="summary" q={{}} seen={a} />
        <View kind="feed" q={{ with: "Zed" }} seen={seen()} />
      </>,
    );
    await settle();
    expect(old.closed).toBe(true);
    const out = vi.fn();
    const stop = onSignedOut(out);
    act(() => {
      old.send(key, "stale");
      old.fail(key, 400, "stale refusal");
      old.fail(null, 401, "stale sign-out");
      old.onerror?.();
    });
    expect(a.snaps).toEqual([]);
    expect(a.state).toEqual({ state: "connecting" });
    expect(out).not.toHaveBeenCalled();
    act(() => open()[0].send(key, "current"));
    expect(a.snaps).toEqual([["current", true]]);
    stop();
  });

  it("replays a last snapshot to a joining view only while its stream is live", async () => {
    const a = seen();
    const { rerender } = render(<View kind="summary" q={{ n: 1 }} seen={a} />);
    await settle();
    const key = watchKey("summary", { n: 1 })!;
    act(() => open()[0].send(key, "before the drop"));
    await act(async () => open()[0].onerror?.());
    // Dropped: a view joining now must not take the old snapshot as fresh.
    const b = seen();
    rerender(
      <>
        <View kind="summary" q={{ n: 1 }} seen={a} />
        <View kind="summary" q={{ n: 1 }} seen={b} />
      </>,
    );
    expect(b.snaps).toEqual([]);
    await act(async () => vi.advanceTimersByTime(1100));
    act(() => open()[0].send(key, "after"));
    expect(b.snaps).toEqual([["after", true]]);
  });

  it("forgets what it held when the session ends", async () => {
    const a = seen();
    const { rerender, unmount } = render(<View kind="summary" q={{ n: 2 }} seen={a} />);
    await settle();
    const key = watchKey("summary", { n: 2 })!;
    act(() => open()[0].send(key, "private"));
    act(() => open()[0].fail(null, 401, "session ended"));
    expect(open()).toHaveLength(0);
    const b = seen();
    rerender(
      <>
        <View kind="summary" q={{ n: 2 }} seen={a} />
        <View kind="summary" q={{ n: 2 }} seen={b} />
      </>,
    );
    expect(b.snaps).toEqual([]);
    unmount();
  });

  it("gives a crowded watch its place once one frees up", async () => {
    const views = Array.from({ length: 9 }, (_, i) => ({ q: { n: 100 + i }, seen: seen() }));
    const all = (n: number) => (
      <>
        {views.slice(0, n).map((v) => (
          <View key={v.q.n} kind="summary" q={v.q} seen={v.seen} />
        ))}
      </>
    );
    const { rerender } = render(all(9));
    await settle();
    expect(open()[0].subs).toHaveLength(8);
    expect(views[8].seen.state).toEqual({ state: "failed", error: "too many live views in this tab" });
    rerender(<>{views.slice(1, 9).map((v) => <View key={v.q.n} kind="summary" q={v.q} seen={v.seen} />)}</>);
    await settle();
    expect(open()[0].subs).toHaveLength(8);
    expect(open()[0].subs.map((x) => x.q)).toContainEqual({ n: 108 });
    expect(views[8].seen.state).toEqual({ state: "connecting" });
  });

  it("keys distinct requests apart, however alike", async () => {
    render(
      <>
        <View kind="feed" q={{ with: "a" }} seen={seen()} />
        <View kind="feed" q={{ with: "b" }} seen={seen()} />
        <View kind="summary" q={{ with: "a" }} seen={seen()} />
      </>,
    );
    await settle();
    const subs = open()[0].subs;
    expect(new Set(subs.map((x) => x.key)).size).toBe(3);
    for (const x of subs) expect(x.key.length).toBeLessThanOrEqual(64);
  });

  it("ignores a reconnect probe answered after another stream has opened", async () => {
    let answer: (r: Response) => void = () => {};
    vi.stubGlobal("fetch", vi.fn(() => new Promise<Response>((r) => (answer = r))));
    const a = seen();
    const { rerender } = render(<View kind="summary" q={{ n: 3 }} seen={a} />);
    await settle();
    await act(async () => open()[0].onerror?.());
    // While the probe is out, the set changes and a new stream opens.
    rerender(
      <>
        <View kind="summary" q={{ n: 3 }} seen={a} />
        <View kind="feed" q={{ with: "Ann" }} seen={seen()} />
      </>,
    );
    await settle();
    const opened = FakeSource.all.length;
    const live = open()[0];
    await act(async () => answer(new Response(JSON.stringify({ name: "ada" }), { status: 200 })));
    await act(async () => vi.advanceTimersByTime(31000));
    // The late answer reopened nothing: the stream that opened stays.
    expect(FakeSource.all.length).toBe(opened);
    expect(live.closed).toBe(false);
  });
});
