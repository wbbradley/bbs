import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import { Login } from "./Login";
import { loadDraft, saveDraft, setDraftOwner, storage } from "./util";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  localStorage.clear();
});

describe("storage", () => {
  it("keeps drafts to the human who wrote them", () => {
    setDraftOwner("Ada");
    saveDraft("agent:mateo", "secret");
    setDraftOwner("bob");
    expect(loadDraft("agent:mateo")).toBe("");
    setDraftOwner(null);
    expect(loadDraft("agent:mateo")).toBe("");
    saveDraft("agent:mateo", "nobody's");
    setDraftOwner("ada");
    expect(loadDraft("agent:mateo")).toBe("secret");
  });

  it("signs in even when the browser refuses its storage", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("denied", "SecurityError");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("denied", "SecurityError");
    });
    render(<Login info={null} onSignedIn={() => {}} />);
    expect(screen.getByText("Sign in", { selector: "h1" })).toBeTruthy();
    // The visit still remembers what it was told.
    storage.set("k", "v");
    expect(storage.get("k")).toBe("v");
  });
});
