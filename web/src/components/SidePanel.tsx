import { useState } from "react";
import { rpc } from "../api";
import { Markdown } from "../Markdown";
import { SideStatus, type SideReport, type SideRequest } from "../types";

const STATUS: Record<number, string> = {
  [SideStatus.Open]: "answering…",
  [SideStatus.Completed]: "answered",
  [SideStatus.Failed]: "failed",
  [SideStatus.Cancelled]: "cancelled",
  [SideStatus.Interrupted]: "interrupted by a runner restart",
  [SideStatus.Ended]: "the side conversation ended",
};

/**
 * Side questions (`/btw`): asked of the agent in an isolated turn, outside
 * its mail and history. A finished answer can take a follow-up when the
 * runner continues side conversations.
 */
export function SidePanel({
  agent,
  report,
  history,
  onClose,
}: {
  agent: string;
  report: SideReport | null;
  /** This visit's earlier exchanges in the same side conversation. */
  history: SideRequest[];
  onClose: () => void;
}) {
  const [question, setQuestion] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const side = report?.side ?? null;
  const open = side?.status === SideStatus.Open;
  const canFollow =
    !!side && !open && !side.ended && !!report?.side_followups && side.status !== SideStatus.Interrupted;
  const unsupported = report && report.side_supported === false;

  const ask = async () => {
    if (!question.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await rpc("ask_side", { agent, question, follows: canFollow ? side!.id : null });
      setQuestion("");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const close = async () => {
    if (side && (open || canFollow)) {
      // Ending stops a running answer and the conversation; it is idempotent.
      await rpc("cancel_side", { id: side.id, end: true }).catch(() => {});
    }
    onClose();
  };

  const shown = [...history.filter((h) => h.id !== side?.id), ...(side ? [side] : [])];
  return (
    <div className="side-panel">
      <div className="side-panel-head">
        <span>
          aside with <span className="agent">{agent}</span>
        </span>
        <span className="faint">{side ? STATUS[side.status] ?? "" : ""}</span>
        <span style={{ flex: 1 }} />
        {open && (
          <button className="btn small" onClick={() => rpc("cancel_side", { id: side!.id, end: false }).catch(() => {})}>
            Stop
          </button>
        )}
        <button className="btn small" onClick={close}>
          Close
        </button>
      </div>
      <div className="side-panel-body">
        {unsupported && <div className="notice warning">{report?.side_reason || "This agent's runner does not answer side questions."}</div>}
        {shown.length === 0 && !unsupported && (
          <p className="muted">Ask the agent something on the side. It answers in an isolated turn, outside its mail and history.</p>
        )}
        {shown.map((s) => (
          <div key={s.id}>
            <div className="side-q">› {s.question}</div>
            {s.answer ? <Markdown text={s.answer} /> : <span className="faint">{STATUS[s.status]}</span>}
          </div>
        ))}
        <div style={{ display: "flex", gap: 8, marginTop: 10 }}>
          <input
            className="input"
            placeholder={canFollow ? "Follow up…" : side && !open ? "Ask a new side question…" : "Ask on the side…"}
            value={question}
            maxLength={4096}
            disabled={busy || open}
            onChange={(e) => setQuestion(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && ask()}
          />
          <button className="btn" disabled={busy || open || !question.trim()} onClick={ask}>
            Ask
          </button>
        </div>
        {error && <div className="notice error">{error}</div>}
      </div>
    </div>
  );
}
