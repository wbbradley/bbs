import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => import("./test/fakeApi"));

import { Login } from "./Login";
import { loadDraft, saveDraft, setDraftOwner, storage } from "./util";
import { loadKept } from "./pages/Blockers";

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

  it("never reads drafts from before keys carried their human, and clears them", () => {
    localStorage.setItem("bbs-draft:agent:mateo", "someone's");
    setDraftOwner("ada");
    expect(loadDraft("agent:mateo")).toBe("");
    expect(localStorage.getItem("bbs-draft:agent:mateo")).toBeNull();
  });

  it("keeps what a full browser store refused for the rest of the visit", () => {
    setDraftOwner("ada");
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("full", "QuotaExceededError");
    });
    saveDraft("blocker:1", "an answer");
    storage.set("bbs-v2-blocker-writes:ada:all", JSON.stringify({ 1: { root: 1, state: "unconfirmed" } }));
    // Read again as a page coming back would, though the browser kept nothing.
    expect(localStorage.getItem("bbs-v2-draft:ada:blocker:1")).toBeNull();
    expect(loadDraft("blocker:1")).toBe("an answer");
    expect(loadKept("ada")[1].state).toBe("unconfirmed");
    vi.restoreAllMocks();
    // A later write the browser takes is read from it again.
    saveDraft("blocker:1", "stored");
    expect(localStorage.getItem("bbs-v2-draft:ada:blocker:1")).toBe("stored");
    expect(loadDraft("blocker:1")).toBe("stored");
    saveDraft("blocker:1", "");
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
