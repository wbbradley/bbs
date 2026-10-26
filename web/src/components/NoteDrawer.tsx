import { useEffect, useState } from "react";
import { rpc } from "../api";
import { useBoard } from "../store";
import type { AgentNote } from "../types";
import { fullTime } from "../util";

const LIMIT = 32768;

/**
 * Your private note on an agent: kept on the server for you alone, never
 * shown to agents or other humans. Saving is explicit and the last save wins.
 */
export function NoteDrawer({ agentId, agent, onClose }: { agentId: number; agent: string; onClose: () => void }) {
  const board = useBoard();
  const stored = board.notes.get(agentId);
  const [text, setText] = useState(stored?.body ?? "");
  const [base, setBase] = useState(stored?.body ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const changed = text !== base;
  const bytes = new TextEncoder().encode(text).length;

  // A note you have not changed follows saves from elsewhere.
  useEffect(() => {
    if (stored && !changed && stored.body !== base) {
      setText(stored.body);
      setBase(stored.body);
    }
  }, [stored, changed, base]);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await rpc<{ note: AgentNote }>("put_agent_note", { agent_id: agentId, body: text });
      board.setNote(r.note);
      setBase(r.note.body);
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  return (
    <aside className="drawer">
      <div className="drawer-head">
        <span>
          Note on <span className="agent">{agent}</span>
        </span>
        <span className="spacer" />
        <button className="btn small primary" disabled={!changed || busy || bytes > LIMIT} onClick={save}>
          {busy ? "Saving…" : "Save"}
        </button>
        <button
          className="btn small"
          onClick={async () => {
            if (!changed || (await save())) onClose();
          }}
        >
          {changed ? "Save & close" : "Close"}
        </button>
      </div>
      <div className="drawer-body">
        <textarea
          className="textarea"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if ((e.metaKey || e.ctrlKey) && e.key === "s") {
              e.preventDefault();
              if (changed) save();
            }
          }}
          placeholder="Private to you. Agents and other humans never see it."
          autoFocus
        />
        <div className="composer-hint">
          <span className={error ? "error-text" : ""}>
            {error ?? (stored ? `saved ${fullTime(stored.updated_at)}` : "not saved yet")}
          </span>
          <span className={bytes > LIMIT ? "error-text" : ""}>
            {bytes}/{LIMIT} bytes · ⌘S saves
          </span>
        </div>
      </div>
    </aside>
  );
}
