import { useState } from "react";
import { rpc } from "../api";
import { useBoard } from "../store";
import { ago } from "../util";

/** Focus mode, explained wherever an agent is marked as focused. */
function focusTitle(owner: string | null, since: string): string {
  return `Focused ${ago(since)}: only ${owner ?? "its owner"}'s direct messages reach it, and it can message only ${owner ?? "its owner"}.`;
}

/** Turn events after which the agent is between turns. */
const BETWEEN_TURNS = ["turn_end", "turn_error", "cancel", "stop"];

/** Whether the agent's newest turn event says it is between turns. */
export function betweenTurns(turnEvent: string | null | undefined): boolean {
  return !!turnEvent && BETWEEN_TURNS.includes(turnEvent);
}

/**
 * The mark beside a focused agent's name; nothing when it is not focused.
 * Between turns, a focused agent waits for its owner, so the mark says so.
 */
export function FocusMark({
  owner,
  since,
  turnEvent,
}: {
  owner: string | null;
  since: string | null;
  turnEvent?: string | null;
}) {
  if (!since) return null;
  if (betweenTurns(turnEvent)) {
    return (
      <span
        className="badge focus waiting"
        title={`${focusTitle(owner, since)} Idle: waiting for ${owner ?? "its owner"}.`}
        aria-label={`In focus mode, waiting for ${owner ?? "its owner"}`}
      >
        ⧖
      </span>
    );
  }
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
          ? `${agent} left focus mode and was told so; others can reach it again.`
          : `${agent} is in focus mode and was told so: only your direct messages reach it, and it can message only you.`,
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
          ? "End focus mode"
          : "Focus mode: refuse other senders, skip group mail, and keep this agent from messaging anyone but you"
      }
    >
      ◎ Focus
    </button>
  );
}
