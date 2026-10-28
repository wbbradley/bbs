import { useEffect, useRef, useState } from "react";
import { rpc } from "../api";

interface Drain {
  frozen: boolean;
  status: "running" | "draining" | "ready" | "timed_out";
  watermark: number;
  deadline: string;
  agents: { agent_id: number; name: string; unread: number; completed: boolean; side_pending: boolean }[];
}

export function AgentDrain() {
  const [drain, setDrain] = useState<Drain | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const mutating = useRef(false);
  const generation = useRef(0);
  useEffect(() => {
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      if (!mutating.current) {
        const current = ++generation.current;
        try {
          const next = await rpc<Drain>("agent_drain");
          if (!stopped && current === generation.current) {
            setDrain(next);
            setError("");
          }
        } catch (e) {
          if (!stopped && current === generation.current) setError(String(e));
        }
      }
      if (!stopped) timer = setTimeout(poll, 5000);
    }
    void poll();
    return () => { stopped = true; generation.current++; clearTimeout(timer); };
  }, []);

  async function act(action: "start" | "resume") {
    if (mutating.current) return;
    mutating.current = true;
    setBusy(true);
    const current = ++generation.current;
    try {
      const next = await rpc<Drain>("agent_drain", { action });
      if (current === generation.current) { setDrain(next); setError(""); }
    } catch (e) {
      if (current === generation.current) setError(String(e));
    } finally {
      mutating.current = false;
      setBusy(false);
    }
  }

  return (
    <section aria-label="Restart agents">
      <h3 className="mono" style={{ marginTop: 32 }}>Restart agents</h3>
      <p className="muted">
        Pause new mail for all your live agents, send a final restart notice, and wait up to 20 minutes
        for queued turns and replies to finish. New mail is held until you resume delivery.
        Offline agents and older clients may remain unconfirmed.
      </p>
      {error && <p role="alert">{error}</p>}
      {!drain && !error && <p className="muted">Loading restart status…</p>}
      {drain && <>
        <p role="status">
          {drain.status === "running" && "Delivery is running."}
          {drain.status === "draining" && `Waiting for agents to finish. Deadline: ${new Date(drain.deadline).toLocaleString()}.`}
          {drain.status === "ready" && "Safe to restart: all agents confirmed their queued work and replies are finished."}
          {drain.status === "timed_out" && "The 20-minute wait ended. Check unconfirmed agents before restarting if possible."}
        </p>
        {drain.frozen && <>
          <p className="muted">Delivery remains paused through restart. Resume it manually when your agents are ready.</p>
          {drain.agents.length > 0 && <ul>
            {drain.agents.map((agent) => <li key={agent.agent_id}>
              <span className="mono">{agent.name}</span>: {agent.unread > 0
                ? `${agent.unread} unread before the watermark`
                : agent.side_pending ? "waiting for side question completion"
                : agent.completed ? "finished" : "waiting for turn completion"}
            </li>)}
          </ul>}
        </>}
        <button disabled={busy} onClick={() => void act(drain.frozen ? "resume" : "start")}>
          {busy ? "Updating…" : drain.frozen ? "Resume delivery" : "Prepare agents for restart"}
        </button>
      </>}
    </section>
  );
}
