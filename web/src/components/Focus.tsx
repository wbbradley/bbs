import { useState } from "react";
import { rpc } from "../api";
import { useBoard } from "../store";
import { ago } from "../util";

/** Focus mode, explained wherever an agent is marked as focused. */
function focusTitle(owner: string | null, since: string): string {
  return `Focused ${ago(since)}: only ${owner ?? "its owner"}'s direct messages reach it, and it can message only ${owner ?? "its owner"}.`;
}

/** The mark beside a focused agent's name; nothing when it is not focused. */
export function FocusMark({ owner, since }: { owner: string | null; since: string | null }) {
  if (!since) return null;
  return (
    <span className="badge focus" title={focusTitle(owner, since)} aria-label="In focus mode">
      ◎
    </span>
  );
}

/**
 * The owner's switch for focus mode on the agent's page. Others see only the
 * mark, which the page shows beside the name.
 */
export function FocusToggle({
  agent,
  since,
  onNotice,
}: {
  agent: string;
  since: string | null;
  onNotice: (message: string | null) => void;
}) {
  const board = useBoard();
  const [busy, setBusy] = useState(false);
  const toggle = async () => {
    setBusy(true);
    try {
      await rpc("set_focus", { agent, focused: !since });
      onNotice(
        since
          ? `${agent} left focus mode; mail held during focus is now delivered.`
          : `${agent} is in focus mode: only your direct messages reach it, and it can message only you.`,
      );
      board.refreshAgents();
    } catch (e) {
      onNotice(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <button
      className={`btn small${since ? " primary" : ""}`}
      onClick={toggle}
      disabled={busy}
      aria-pressed={!!since}
      title={
        since
          ? "End focus mode and deliver the mail held during it"
          : "Focus mode: refuse other senders, hold group mail, and keep this agent from messaging anyone but you"
      }
    >
      ◎ Focus
    </button>
  );
}
