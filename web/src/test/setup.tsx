import type { ReactNode } from "react";
import { BoardProvider } from "../store";
import type { Principal, ServerInfo } from "../types";
import { auto } from "./fakeApi";
import { setDraftOwner } from "../util";

export const ADA: Principal = { name: "ada", kind: 2 } as unknown as Principal;

export const INFO = {
  capabilities: ["agent-notes.v1", "blockers-mine.v1", "agent-activity.v1", "agent-interactions.v1", "message-search.v1"],
  release: null,
  require_human_token: true,
} as unknown as ServerInfo;

/** The board for `ada`, its lists answered empty unless a test says more. */
export function Board({ children, info = INFO }: { children: ReactNode; info?: ServerInfo | null }) {
  setDraftOwner(ADA.name);
  if (!auto.has("list_agents")) auto.set("list_agents", () => ({ agents: [], humans: [] }));
  if (!auto.has("group_list")) auto.set("group_list", () => ({ groups: [] }));
  if (!auto.has("list_blockers"))
    auto.set("list_blockers", () => ({ blockers: [], active_count: 0, has_more: false }));
  return (
    <BoardProvider me={ADA} info={info}>
      {children}
    </BoardProvider>
  );
}
