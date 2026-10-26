import { describe, expect, it } from "vitest";
import { KEEP, empty, mergeContext, mergeWindow } from "./merge";
import { sideDraft } from "./util";

const ids = (...n: number[]) => n.map((id) => ({ id }));

describe("merging windows", () => {
  it("keeps what is held when a reconnect sends a newer window", () => {
    let held = mergeWindow(empty<{ id: number }>(), ids(1, 2, 3), 3);
    held = mergeWindow(held, ids(3, 4, 5), 3);
    expect(held.rows.map((r) => r.id)).toEqual([1, 2, 3, 4, 5]);
    expect(held.gaps.size).toBe(0);
  });

  it("marks where a full window starts past everything held", () => {
    let held = mergeWindow(empty<{ id: number }>(), ids(1, 2), 3);
    held = mergeWindow(held, ids(7, 8, 9), 3);
    expect([...held.gaps]).toEqual([7]);
    // A window that is not full reached back to what was held: no gap.
    const quiet = mergeWindow(mergeWindow(empty<{ id: number }>(), ids(1, 2), 3), ids(7, 8), 3);
    expect(quiet.gaps.size).toBe(0);
  });

  it("keeps the newest rows past the cap, and drops gaps that fell off", () => {
    let held = mergeWindow(empty<{ id: number }>(), ids(1, 2), 2);
    held = mergeWindow(held, Array.from({ length: KEEP }, (_, i) => ({ id: 100 + i })), KEEP);
    expect(held.rows.length).toBe(KEEP);
    expect(held.rows[0].id).toBe(100);
    expect(held.gaps.size).toBe(0);
  });

  it("marks the live rows when a found message's context does not reach them", () => {
    const live = mergeWindow(empty<{ id: number }>(), ids(400, 401), 300);
    const joined = mergeContext(live, ids(49, 50, 51), false);
    expect(joined.rows.map((r) => r.id)).toEqual([49, 50, 51, 400, 401]);
    expect([...joined.gaps]).toEqual([400]);
    expect(mergeContext(live, ids(399), true).gaps.size).toBe(0);
  });
});

describe("/btw in a draft", () => {
  it("unescapes only //btw, as connect does", () => {
    expect(sideDraft("// TODO: check")).toEqual({ kind: "mail" });
    expect(sideDraft("//host/path")).toEqual({ kind: "mail" });
    expect(sideDraft("///btw")).toEqual({ kind: "mail" });
    expect(sideDraft("//btw is a command")).toEqual({ kind: "literal", body: "/btw is a command" });
    expect(sideDraft("  /btw  why? ")).toEqual({ kind: "ask", question: "why?" });
    expect(sideDraft("/btwx no")).toEqual({ kind: "mail" });
    expect(sideDraft("/btw")).toEqual({ kind: "ask", question: "" });
  });
});
