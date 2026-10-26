import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from "react";
import { rpc } from "../api";
import { addLeaveGuard } from "../router";
import { useBoard } from "../store";
import type { AgentNote } from "../types";
import { fullTime, loadDraft, saveDraft } from "../util";

const LIMIT = 32768;

export interface NoteHandle {
  /** Store a changed note, then agree to close; false when it could not. */
  requestClose: () => Promise<boolean>;
}

/**
 * Your private note on an agent: kept on the server for you alone, never
 * shown to agents or other humans. Saving is explicit and the last save wins.
 * Nothing is editable until every stored note has arrived, so a blank editor
 * never replaces one. Unsaved text is kept in this browser until stored.
 */
export const NoteDrawer = forwardRef<NoteHandle, { agentId: number; agent: string; onClose: () => void }>(
  function NoteDrawer({ agentId, agent, onClose }, ref) {
    const board = useBoard();
    const stored = board.notes.get(agentId);
    const loaded = board.notesLoaded;
    const draftKey = `note:${agentId}`;
    // The text and the stored body it started from, once the notes are in.
    const [text, setText] = useState<string | null>(null);
    const [base, setBase] = useState("");
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const saving = useRef(false);

    useEffect(() => {
      if (!loaded || text !== null) return;
      const body = stored?.body ?? "";
      const draft = loadDraft(draftKey);
      setBase(body);
      setText(draft && draft !== body ? draft : body);
    }, [loaded, text, stored, draftKey]);

    const current = text ?? "";
    const changed = text !== null && current !== base;
    const readOnly = text === null || !!stored?.retired;
    const bytes = new TextEncoder().encode(current).length;

    useEffect(() => {
      if (text !== null) saveDraft(draftKey, changed ? current : "");
    }, [draftKey, text, changed, current]);

    // A note you have not changed follows saves from elsewhere.
    useEffect(() => {
      if (stored && text !== null && !changed && stored.body !== base) {
        setText(stored.body);
        setBase(stored.body);
      }
    }, [stored, text, changed, base]);

    // One save at a time: a later answer never lands under an earlier one.
    const save = useCallback(async (): Promise<boolean> => {
      if (saving.current || text === null) return false;
      saving.current = true;
      setBusy(true);
      setError(null);
      const body = text;
      try {
        const r = await rpc<{ note: AgentNote }>("put_agent_note", { agent_id: agentId, body });
        board.setNote(r.note);
        setBase(r.note.body);
        return true;
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
        return false;
      } finally {
        saving.current = false;
        setBusy(false);
      }
    }, [agentId, board, text]);

    const requestClose = useCallback(async () => !changed || (await save()), [changed, save]);
    useImperativeHandle(ref, () => ({ requestClose }), [requestClose]);

    // Leaving the page stores a changed note first, and stays if it cannot.
    useEffect(() => addLeaveGuard(requestClose), [requestClose]);
    useEffect(() => {
      if (!changed) return;
      const warn = (e: BeforeUnloadEvent) => e.preventDefault();
      window.addEventListener("beforeunload", warn);
      return () => window.removeEventListener("beforeunload", warn);
    }, [changed]);

    const status = !loaded
      ? board.notesError
        ? `notes unavailable: ${board.notesError}`
        : "loading the note…"
      : stored?.retired
        ? "the agent retired; its note is read only"
        : (error ?? (changed ? "unsaved changes" : stored ? `saved ${fullTime(stored.updated_at)}` : "not saved yet"));

    return (
      <aside className="drawer">
        <div className="drawer-head">
          <span>
            Note on <span className="agent">{agent}</span>
          </span>
          <span className="spacer" />
          {!loaded && board.notesError && (
            <button className="btn small" onClick={() => board.syncNotes()}>
              Retry
            </button>
          )}
          <button className="btn small primary" disabled={!changed || busy || bytes > LIMIT} onClick={save}>
            {busy ? "Saving…" : "Save"}
          </button>
          <button
            className="btn small"
            disabled={busy}
            onClick={async () => {
              if (await requestClose()) onClose();
            }}
          >
            {changed ? "Save & close" : "Close"}
          </button>
        </div>
        <div className="drawer-body">
          <textarea
            className="textarea"
            value={current}
            readOnly={readOnly}
            onChange={(e) => !readOnly && setText(e.target.value)}
            onKeyDown={(e) => {
              if ((e.metaKey || e.ctrlKey) && e.key === "s") {
                e.preventDefault();
                if (changed && !busy) save();
              }
            }}
            placeholder={loaded ? "Private to you. Agents and other humans never see it." : ""}
            autoFocus
          />
          <div className="composer-hint">
            <span className={error || (!loaded && board.notesError) ? "error-text" : ""}>{status}</span>
            <span className={bytes > LIMIT ? "error-text" : ""}>
              {bytes}/{LIMIT} bytes · ⌘S saves
            </span>
          </div>
        </div>
      </aside>
    );
  },
);
