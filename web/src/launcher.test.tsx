import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState, type ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import * as api from "./test/fakeApi";
import { Composer } from "./components/Composer";
import { Launcher } from "./Launcher";
import { addLeaveGuard, go } from "./router";
import { Board } from "./test/setup";

beforeEach(() => {
  api.reset();
  localStorage.clear();
  go("/agents");
});
afterEach(cleanup);

/** A page with something focused, and the launcher toggled as ⌘K does. */
function Page({ children }: { children: ReactNode }) {
  const [open, setOpen] = useState(false);
  return (
    <Board>
      {children}
      <button onClick={() => setOpen((o) => !o)}>⌘K</button>
      {open && <Launcher onClose={() => setOpen(false)} />}
    </Board>
  );
}

const search = () => screen.getByPlaceholderText("Pages, agents, groups, notes, messages…") as HTMLInputElement;

/** Open the launcher over `el`, focused, as ⌘K does: focus moves to its box. */
async function launchOver(el: HTMLElement) {
  el.focus();
  // fireEvent clicks leave focus where it is, as a key press would.
  fireEvent.click(screen.getByText("⌘K"));
  await act(async () => {});
  expect(document.activeElement).toBe(search());
}

async function composer() {
  render(
    <Page>
      <Composer draftKey="agent:Grove" placeholder="Message Grove" onSubmit={async () => true} />
    </Page>,
  );
  await act(async () => {});
  const box = screen.getByPlaceholderText("Message Grove") as HTMLTextAreaElement;
  fireEvent.change(box, { target: { value: "hello world" } });
  box.setSelectionRange(5, 5);
  return box;
}

describe("the launcher gives focus back", () => {
  it("to a text box, cursor and all, when dismissed by Esc, a click outside or ⌘K", async () => {
    const box = await composer();
    const dismiss = {
      esc: () => fireEvent.keyDown(search(), { key: "Escape" }),
      outside: () => {
        // The click's own focus change is held back.
        expect(fireEvent.mouseDown(document.querySelector(".overlay")!)).toBe(false);
      },
      toggle: () => fireEvent.click(screen.getByText("⌘K")),
    };
    for (const [how, close] of Object.entries(dismiss)) {
      await launchOver(box);
      await act(async () => close());
      expect(document.querySelector(".launcher"), how).toBeNull();
      expect(document.activeElement, how).toBe(box);
      expect([box.selectionStart, box.selectionEnd], how).toEqual([5, 5]);
      expect(box.value, how).toBe("hello world");
    }
  });

  it("to a button, such as the one that opened it", async () => {
    render(
      <Page>
        <button>Compact</button>
      </Page>,
    );
    await act(async () => {});
    await launchOver(screen.getByText("Compact"));
    await act(async () => fireEvent.keyDown(search(), { key: "Escape" }));
    expect(document.activeElement).toBe(screen.getByText("Compact"));
  });

  it("nowhere when what had it is gone", async () => {
    function Gone() {
      const [shown, setShown] = useState(true);
      return shown ? <textarea aria-label="answer" onBlur={() => setShown(false)} /> : null;
    }
    render(
      <Page>
        <Gone />
      </Page>,
    );
    await act(async () => {});
    await launchOver(screen.getByLabelText("answer"));
    expect(screen.queryByLabelText("answer")).toBeNull();
    await act(async () => fireEvent.keyDown(search(), { key: "Escape" }));
    expect(document.activeElement).toBe(document.body);
  });

  it("not when a result opens another page, but when a guard keeps you here or it is this page", async () => {
    const box = await composer();
    const choose = async (query: string) => {
      await launchOver(box);
      fireEvent.change(search(), { target: { value: query } });
      await act(async () => fireEvent.keyDown(search(), { key: "Enter" }));
    };
    // Another page: focus is the new page's to give.
    await choose("feed");
    expect(location.pathname).toBe("/feed");
    expect(document.activeElement).not.toBe(box);
    // The page you are on: back to the text box.
    await choose("feed");
    expect(document.activeElement).toBe(box);
    // A leave guard that keeps you here: back to the text box.
    const remove = addLeaveGuard(() => false);
    await choose("groups");
    remove();
    expect(location.pathname).toBe("/feed");
    expect(document.activeElement).toBe(box);
    expect(box.selectionStart).toBe(5);
  });
});
