import { useEffect, useState } from "react";
import { ApiError, rpc } from "../api";
import type { Timer } from "../types";
import { fullTime } from "../util";

const POLL_MS = 15000;

/** Read the server's schedules directly; opening this panel never messages the agent. */
export function TimerDrawer({ agent, onClose }: { agent: string; onClose: () => void }) {
  const [timers, setTimers] = useState<Timer[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [updated, setUpdated] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [visible, setVisible] = useState(document.visibilityState === "visible");

  useEffect(() => {
    const changed = () => setVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", changed);
    return () => document.removeEventListener("visibilitychange", changed);
  }, []);

  useEffect(() => {
    if (!visible) return;
    let stopped = false;
    let timeout: number | undefined;
    const load = async () => {
      setBusy(true);
      let refused = false;
      try {
        const result = await rpc<{ timers: Timer[] }>("timer_list", { agent });
        if (stopped) return;
        setTimers(result.timers);
        setUpdated(new Date().toISOString());
        setError(null);
      } catch (e) {
        if (stopped) return;
        setError(e instanceof Error ? e.message : String(e));
        refused = e instanceof ApiError && e.status >= 400 && e.status < 500 && e.status !== 429;
      } finally {
        if (!stopped) setBusy(false);
      }
      if (!stopped && !refused) timeout = window.setTimeout(load, POLL_MS);
    };
    load();
    return () => {
      stopped = true;
      window.clearTimeout(timeout);
    };
  }, [agent, refresh, visible]);

  return (
    <aside className="drawer timer-drawer" aria-label={`Timers for ${agent}`}>
      <div className="drawer-head">
        <span>Timers for <span className="agent">{agent}</span></span>
        <span className="spacer" />
        <button className="btn small" disabled={busy} onClick={() => setRefresh((n) => n + 1)}>
          {error ? "Retry" : "Refresh"}
        </button>
        <button className="btn small" onClick={onClose}>Close</button>
      </div>
      <div className="drawer-body">
        <p className="muted">Scheduled messages from the server. Refreshes every 15 seconds while visible.</p>
        {error && <p role="alert" className="error-text">Timers unavailable: {error}{timers !== null ? ". Showing the last successful list." : ""}</p>}
        {timers === null && !error && <p role="status">Loading timers…</p>}
        {timers?.length === 0 && <p>No timers set for {agent}.</p>}
        {timers && timers.length > 0 && (
          <ul className="timer-list">
            {timers.map((timer) => (
              <li key={timer.id} className="timer-card">
                <div className="timer-title">Timer #{timer.id} · {timer.repeat ? "Recurring" : "One-time"}</div>
                <dl>
                  <dt>Next run</dt><dd><time dateTime={timer.next_run} title={timer.next_run}>{fullTime(timer.next_run)}</time> (local)</dd>
                  <dt>Schedule</dt><dd>{timer.repeat ? `${timer.repeat} (UTC)` : "Once"}</dd>
                  <dt>Created by</dt><dd>{timer.creator}</dd>
                </dl>
                <div className="timer-message">{timer.body}</div>
              </li>
            ))}
          </ul>
        )}
        {updated && <p className="muted">Last updated {fullTime(updated)} (local)</p>}
      </div>
    </aside>
  );
}
