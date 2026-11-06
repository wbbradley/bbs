import { useState } from "react";
import { rpc } from "../api";
import { clock, fullTime } from "../util";
import { Control, GoalStatus, type GoalReport } from "../types";
import { Dialog } from "./Dialog";

const NAMES: Record<number, string> = {
  [GoalStatus.Active]: "active",
  [GoalStatus.Waiting]: "waiting",
  [GoalStatus.Blocked]: "blocked",
  [GoalStatus.Complete]: "complete",
  [GoalStatus.Paused]: "paused",
  [GoalStatus.Limited]: "limited",
  [GoalStatus.Stalled]: "stalled",
};

/** The objective's limit, as the server checks it. */
const MAX_OBJECTIVE = 4096;

export function goalStatus(goal: GoalReport): string {
  return NAMES[goal.status] ?? "unknown";
}

/** Why the goal is where it is, in a few words. */
function detail(goal: GoalReport): string | null {
  switch (goal.status) {
    case GoalStatus.Waiting:
      return goal.wake_at ? `until ${clock(goal.wake_at)}${goal.reason ? `: ${goal.reason}` : ""}` : goal.reason;
    case GoalStatus.Complete:
      return goal.evidence;
    default:
      return goal.reason || null;
  }
}

async function control(agent: string, control: number, body: string) {
  await rpc("send", { to: agent, body, control });
}

/** The owner's dialog for giving the agent a goal, replacing any it has. */
export function GoalDialog({
  agent,
  current,
  onClose,
  onNotice,
}: {
  agent: string;
  current: GoalReport | null;
  onClose: () => void;
  onNotice: (message: string) => void;
}) {
  const [objective, setObjective] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const bytes = new TextEncoder().encode(objective).length;
  const set = async () => {
    setBusy(true);
    setError(null);
    try {
      await control(agent, Control.GoalSet, objective.trim());
      onNotice(`Asked ${agent}'s runner to work toward the new goal.`);
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={`Goal for ${agent}`}
      onClose={onClose}
      actions={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy || !objective.trim() || bytes > MAX_OBJECTIVE} onClick={set}>
            Set goal
          </button>
        </>
      }
    >
      <p className="muted" style={{ marginTop: 0 }}>
        The runner prompts {agent} toward this, turn after turn, until it marks the goal complete, blocks or waits,
        or you pause it.{current ? " This replaces the current goal." : ""}
      </p>
      <label className="field">
        <span>Objective</span>
        <textarea
          className="textarea"
          rows={5}
          value={objective}
          onChange={(e) => setObjective(e.target.value)}
          placeholder="Get the integration tests passing on main"
          autoFocus
        />
      </label>
      {bytes > MAX_OBJECTIVE && <div className="notice error">At most {MAX_OBJECTIVE} bytes.</div>}
      {error && <div className="notice error">{error}</div>}
    </Dialog>
  );
}

/**
 * The agent's goal above the composer: objective, status, turns used and why,
 * with the owner's pause, resume and clear.
 */
export function GoalStrip({
  agent,
  goal,
  owned,
  onEdit,
  onNotice,
}: {
  agent: string;
  goal: GoalReport;
  owned: boolean;
  onEdit: () => void;
  onNotice: (message: string) => void;
}) {
  const [busy, setBusy] = useState(false);
  const status = goalStatus(goal);
  const why = detail(goal);
  const act = async (kind: number, body: string, done: string) => {
    setBusy(true);
    try {
      await control(agent, kind, body);
      onNotice(done);
    } catch (e) {
      onNotice(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const running = goal.status === GoalStatus.Active || goal.status === GoalStatus.Waiting || goal.status === GoalStatus.Blocked;
  return (
    <div className={`attention goal goal-${status}`} title={`Set by ${goal.set_by}, ${fullTime(goal.set_at)}`}>
      <span className="goal-text">
        <span className="goal-status">goal · {status}</span>{" "}
        <span className="muted">
          {goal.iteration}/{goal.budget} turns
        </span>{" "}
        <span className="goal-objective">{goal.objective}</span>
        {why && <span className="muted"> — {why}</span>}
      </span>
      <span className="spacer" />
      {owned && (
        <>
          {running ? (
            <button className="btn small" disabled={busy} onClick={() => act(Control.GoalPause, "Pause your goal.", `Asked ${agent}'s runner to pause its goal.`)}>
              Pause
            </button>
          ) : (
            goal.status !== GoalStatus.Complete && (
              <button className="btn small" disabled={busy} onClick={() => act(Control.GoalResume, "Resume your goal.", `Asked ${agent}'s runner to resume its goal.`)}>
                Resume
              </button>
            )
          )}
          <button className="btn small" disabled={busy} onClick={onEdit}>
            New
          </button>
          <button className="btn small ghost" disabled={busy} onClick={() => act(Control.GoalClear, "Clear your goal.", `Asked ${agent}'s runner to clear its goal.`)}>
            Clear
          </button>
        </>
      )}
    </div>
  );
}
