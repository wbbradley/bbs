import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { createRef } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { NoteDrawer, type NoteHandle } from "./components/NoteDrawer";
import { mergeNotes } from "./store";
import { navigate } from "./router";
import { Board } from "./test/setup";
import type { AgentNote } from "./types";
import { loadDraft } from "./util";

const note = (body: string, revision: number): AgentNote => ({
  agent_id: 7,
  agent: "Mateo",
  retired: false,
  body,
  revision,
  updated_at: "2026-10-26T12:00:00Z",
});

function drawer(onClose = () => {}) {
  const ref = createRef<NoteHandle>();
  render(
    <Board>
      <NoteDrawer ref={ref} agentId={7} agent="Mateo" onClose={onClose} />
    </Board>,
  );
  return ref;
}

const area = () => screen.getByRole("textbox") as HTMLTextAreaElement;

beforeEach(() => {
  api.reset();
  localStorage.clear();
  history.replaceState(null, "", "/agents/Mateo");
});
afterEach(cleanup);

describe("notes", () => {
  it("cannot be edited until every stored note has arrived, so a blank never replaces one", async () => {
    drawer();
    expect(area().readOnly).toBe(true);
    expect(screen.getByText("loading the note…")).toBeTruthy();
    // Typing into the read-only box changes nothing that could be saved.
    fireEvent.change(area(), { target: { value: "typed early" } });
    expect((screen.getByText("Save") as HTMLButtonElement).disabled).toBe(true);
    await act(async () => api.take("list_agent_notes").resolve({ notes: [note("kept", 3)], has_more: false, next_after_revision: 3 }));
    expect(area().readOnly).toBe(false);
    expect(area().value).toBe("kept");
    expect(api.pending("put_agent_note")).toHaveLength(0);
  });

  it("says when the notes could not be listed, and lists them again on retry", async () => {
    drawer();
    await act(async () => api.take("list_agent_notes").reject(new Error("down")));
    expect(screen.getByText(/notes unavailable: down/)).toBeTruthy();
    expect(area().readOnly).toBe(true);
    fireEvent.click(screen.getByText("Retry"));
    await act(async () => api.take("list_agent_notes").resolve({ notes: [], has_more: false, next_after_revision: 0 }));
    expect(area().readOnly).toBe(false);
  });

  it("stores a change before closing, and stays open when it cannot", async () => {
    const onClose = vi.fn();
    const ref = drawer(onClose);
    await act(async () => api.take("list_agent_notes").resolve({ notes: [note("old", 1)], has_more: false, next_after_revision: 1 }));
    fireEvent.change(area(), { target: { value: "new" } });
    let closed: Promise<boolean>;
    act(() => {
      closed = ref.current!.requestClose();
    });
    const put = api.take("put_agent_note");
    expect(put.req).toEqual({ agent_id: 7, body: "new" });
    await act(async () => put.reject(new Error("refused")));
    expect(await closed!).toBe(false);
    expect(area().value).toBe("new");
    // Unsaved text is kept in this browser, for this human only.
    expect(loadDraft("note:7")).toBe("new");
  });

  it("keeps navigation from leaving a changed note until it is stored", async () => {
    drawer();
    await act(async () => api.take("list_agent_notes").resolve({ notes: [], has_more: false, next_after_revision: 0 }));
    fireEvent.change(area(), { target: { value: "unsaved" } });
    let moved: Promise<boolean>;
    act(() => {
      moved = navigate("/feed");
    });
    expect(location.pathname).toBe("/agents/Mateo");
    await act(async () => api.take("put_agent_note").resolve({ note: note("unsaved", 1) }));
    expect(await moved!).toBe(true);
    expect(location.pathname).toBe("/feed");
    expect(loadDraft("note:7")).toBe("");
  });

  it("restores unsaved text kept from before when it reopens", async () => {
    drawer();
    await act(async () => api.take("list_agent_notes").resolve({ notes: [note("stored", 2)], has_more: false, next_after_revision: 2 }));
    fireEvent.change(area(), { target: { value: "half written" } });
    cleanup();
    drawer();
    await act(async () => api.take("list_agent_notes").resolve({ notes: [note("stored", 2)], has_more: false, next_after_revision: 2 }));
    expect(area().value).toBe("half written");
    expect(screen.getByText("unsaved changes")).toBeTruthy();
  });

  it("saves one at a time, so an earlier answer never lands over a later one", async () => {
    drawer();
    await act(async () => api.take("list_agent_notes").resolve({ notes: [], has_more: false, next_after_revision: 0 }));
    fireEvent.change(area(), { target: { value: "one" } });
    fireEvent.keyDown(area(), { key: "s", ctrlKey: true });
    fireEvent.keyDown(area(), { key: "s", ctrlKey: true });
    expect(api.pending("put_agent_note")).toHaveLength(1);
    await act(async () => api.take("put_agent_note").resolve({ note: note("one", 4) }));
    await waitFor(() => expect(screen.getByText(/saved/)).toBeTruthy());
  });

  it("keeps the newer revision when a late listing arrives after a save", () => {
    const saved = mergeNotes(new Map(), [note("after save", 5)]);
    const late = mergeNotes(saved, [note("before save", 4)]);
    expect(late.get(7)!.body).toBe("after save");
  });
});
