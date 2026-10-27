import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { Composer } from "./Composer";
import { setDraftOwner } from "../util";

beforeEach(() => { localStorage.clear(); setDraftOwner("ada"); });
afterEach(cleanup);

it("keeps modified/IME Enter as text and sends the exact draft once until settled", async () => {
  let finish!: (accepted: boolean) => void;
  const submit = vi.fn(() => new Promise<boolean>((resolve) => { finish = resolve; }));
  render(<Composer draftKey="agent:Mateo" placeholder="reply" onSubmit={submit} />);
  const box = screen.getByPlaceholderText("reply") as HTMLTextAreaElement;
  fireEvent.change(box, { target: { value: "\nfirst\nsecond" } });
  for (const modifier of [{ shiftKey: true }, { altKey: true }, { isComposing: true }, { keyCode: 229 }]) {
    expect(fireEvent.keyDown(box, { key: "Enter", ...modifier })).toBe(true);
    expect(submit).not.toHaveBeenCalled();
  }
  expect(fireEvent.keyDown(box, { key: "Enter" })).toBe(false);
  expect(submit).toHaveBeenCalledExactlyOnceWith("\nfirst\nsecond");
  fireEvent.keyDown(box, { key: "Enter" });
  expect(submit).toHaveBeenCalledTimes(1);
  await act(async () => finish(false));
  expect(box.value).toBe("\nfirst\nsecond");
});

it("refits a wrapped draft when its width changes and keeps an empty placeholder to one row", () => {
  let width = 600;
  let notify!: () => void;
  vi.spyOn(HTMLTextAreaElement.prototype, "clientWidth", "get").mockImplementation(() => width);
  vi.spyOn(HTMLTextAreaElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLTextAreaElement) {
    // Browser layout is absent in jsdom; model wrapping and a tall placeholder.
    return this.value ? Math.ceil(this.value.length / (width / 10)) * 20 + 4 : 204;
  });
  vi.stubGlobal("ResizeObserver", class {
    constructor(callback: () => void) { notify = callback; }
    observe() {}
    disconnect() {}
  });
  try {
    render(<Composer draftKey="agent:Mateo" placeholder="reply" onSubmit={vi.fn(async () => true)} />);
    const box = screen.getByPlaceholderText("reply") as HTMLTextAreaElement;
    expect(box.style.height).toBe("auto");
    fireEvent.change(box, { target: { value: "x".repeat(120) } });
    expect(box.style.height).toBe("44px");
    width = 300;
    act(() => notify());
    expect(box.style.height).toBe("84px");
    width = 600;
    act(() => notify());
    expect(box.style.height).toBe("44px");
    fireEvent.change(box, { target: { value: "" } });
    expect(box.style.height).toBe("auto");
  } finally {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  }
});
