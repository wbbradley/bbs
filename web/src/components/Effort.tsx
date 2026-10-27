import { useState } from "react";
import { rpc } from "../api";
import { Control, EffortResult, type EffortReport } from "../types";

function label(report: EffortReport | null, value: string | null | undefined): string {
  if (!value) return "unknown";
  return report?.state?.choices.find((c) => c.value === value)?.label || value;
}

/** "effort low", "effort low → medium next turn", or "effort unknown". */
export function effortLabel(report: EffortReport | null): string {
  if (!report?.state?.supported) return report?.state ? "unsupported" : "–";
  const current = label(report, report.state.current);
  if (report.pending) return `${current} → ${label(report, report.pending.value)} next turn`;
  return current;
}

/**
 * The agent's reported effort, and a picker that asks its driver for another
 * level with a SET_EFFORT control message. Only the owner may send it.
 */
export function EffortControl({
  agent,
  report,
  owned,
}: {
  agent: string;
  report: EffortReport | null;
  owned: boolean;
}) {
  const [requested, setRequested] = useState<{ id: number | null; value: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const state = report?.state;
  if (!state?.supported) {
    return (
      <span className="mono muted" style={{ fontSize: 12 }} title={state?.reason}>
        effort {state ? "unsupported" : "unknown"}
      </span>
    );
  }
  const answered = report?.answered;
  const settled =
    requested &&
    (state.current === requested.value ||
      (answered && answered.request_id === requested.id && answered.result !== EffortResult.Applied));
  const pending = report?.pending?.value ?? (settled ? null : requested?.value);
  const failed =
    answered && requested && answered.request_id === requested.id && answered.result === EffortResult.Failed
      ? answered.detail || "refused"
      : null;
  const choices = [...state.choices].sort((a, b) => (a.rank ?? 1e9) - (b.rank ?? 1e9));

  const set = async (value: string) => {
    setError(null);
    try {
      const r = await rpc<{ id: number | null }>("send", { to: agent, body: value, control: Control.SetEffort });
      setRequested({ id: r.id, value });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <span className="mono" style={{ fontSize: 12, display: "inline-flex", gap: 6, alignItems: "center" }}>
      <span className="muted">effort</span>
      {owned && choices.length > 0 ? (
        <select
          className="input"
          style={{ width: "auto", padding: "1px 4px", fontSize: 12 }}
          value={pending ?? state.current ?? ""}
          onChange={(e) => set(e.target.value)}
        >
          {!state.current && <option value="">unknown</option>}
          {choices.map((c) => (
            <option key={c.value} value={c.value}>
              {c.label || c.value}
            </option>
          ))}
        </select>
      ) : (
        <span>{label(report, state.current)}</span>
      )}
      {pending && pending !== state.current && <span className="muted">→ {label(report, pending)} next turn</span>}
      {(error || failed) && <span className="error-text">{error || failed}</span>}
    </span>
  );
}
