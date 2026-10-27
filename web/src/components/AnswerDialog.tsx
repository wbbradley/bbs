import { useState } from "react";
import { rpc } from "../api";
import { Markdown } from "../Markdown";
import { AnswerAction, type OwnerInput } from "../types";
import { Dialog } from "./Dialog";

/**
 * The agent's structured questions. Nothing is chosen until the owner
 * chooses it; Submit needs every required question answered, and leaves
 * unanswered optional ones out. Replies are never retried automatically.
 */
export function AnswerDialog({
  agent,
  input,
  onClose,
}: {
  agent: string;
  input: OwnerInput;
  onClose: () => void;
}) {
  // By question id, which may be any string, "constructor" included.
  const [answers, setAnswers] = useState<Map<string, string>>(() => new Map());
  const [free, setFree] = useState<Map<string, boolean>>(() => new Map());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const missing = input.questions.filter((q) => q.required && !answers.get(q.id)?.trim());
  const reply = async (action: number) => {
    setBusy(true);
    setError(null);
    try {
      const list =
        action === AnswerAction.Submit
          ? [...answers.entries()]
              .filter(([, v]) => v.trim())
              .map(([question_id, value]) => ({ question_id, value }))
          : [];
      await rpc("answer_owner_input", { request_id: input.request_id, action, answers: list });
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      wide
      title={
        <>
          <span className="agent">{agent}</span> asks {input.questions.length === 1 ? "a question" : `${input.questions.length} questions`}
        </>
      }
      onClose={onClose}
      actions={
        <>
          {error && <span className="error-text" style={{ marginRight: "auto" }}>{error}</span>}
          <button className="btn danger" disabled={busy} onClick={() => reply(AnswerAction.Cancel)}>
            Cancel request
          </button>
          <button className="btn" disabled={busy} onClick={() => reply(AnswerAction.Decline)}>
            Decline
          </button>
          <button
            className="btn primary"
            disabled={busy || missing.length > 0}
            title={missing.length > 0 ? "Answer every required question" : undefined}
            onClick={() => reply(AnswerAction.Submit)}
          >
            Submit
          </button>
        </>
      }
    >
      {input.message && (
        <div style={{ marginBottom: 16 }}>
          <Markdown text={input.message} />
        </div>
      )}
      {input.questions.map((q) => {
        const value = answers.get(q.id) ?? "";
        const chosen = q.options.some((o) => o.value === value);
        const typing = free.get(q.id) || (q.options.length === 0 && q.allow_free_text) || (value && !chosen);
        const set = (v: string) => setAnswers((a) => new Map(a).set(q.id, v));
        return (
          <div className="question" key={q.id}>
            <div className="qhead">
              {q.header}
              {q.required ? " · required" : " · optional"}
            </div>
            <Markdown text={q.text} />
            {q.options.map((o) => (
              <button
                key={o.value}
                className={`choice${value === o.value ? " on" : ""}`}
                onClick={() => {
                  setFree((f) => new Map(f).set(q.id, false));
                  // Choosing an optional answer again clears it.
                  set(value === o.value && !q.required ? "" : o.value);
                }}
              >
                <span>{value === o.value ? "◉" : "○"}</span>
                <span>
                  <div>{o.label || o.value}</div>
                  {o.description && <div className="desc">{o.description}</div>}
                </span>
              </button>
            ))}
            {q.allow_free_text && q.options.length > 0 && !typing && (
              <button className="choice" onClick={() => (setFree((f) => new Map(f).set(q.id, true)), set(""))}>
                <span>○</span>
                <span className="muted">Other…</span>
              </button>
            )}
            {q.allow_free_text && typing && (
              <textarea
                className="textarea"
                style={{ minHeight: 60, marginTop: 4 }}
                maxLength={4096}
                value={chosen ? "" : value}
                placeholder="Your answer"
                onChange={(e) => set(e.target.value)}
              />
            )}
          </div>
        );
      })}
    </Dialog>
  );
}
