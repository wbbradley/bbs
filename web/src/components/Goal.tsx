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
  [GoalStatus.Expired]: "expired",
};

const DAY_MS = 24 * 60 * 60 * 1000;

/** "turn 3", or "turn 3 of 50" under a turn budget (0 is none). */
export function goalProgress(goal: GoalReport): string {
  return goal.budget > 0 ? `turn ${goal.iteration} of ${goal.budget}` : `turn ${goal.iteration}`;
}

/** A day past the deadline, or past now if it has passed or there is none. */
export function extendedDeadline(goal: GoalReport, now = Date.now()): string {
  const from = Math.max(now, goal.deadline ? Date.parse(goal.deadline) : 0);
  return new Date(from + DAY_MS).toISOString().replace(/\.\d{3}Z$/, "Z");
}

/** The objective's limit, as the server checks it. */
const MAX_OBJECTIVE = 4096;

export function goalStatus(goal: GoalReport): string {
  return NAMES[goal.status] ?? "unknown";
}

/** Goal states the runner is still working through on its own. */
const WORKING: number[] = [GoalStatus.Active, GoalStatus.Waiting, GoalStatus.Blocked];
/** Goal states that stopped short and wait for the owner to resume them. */
const HALTED: number[] = [GoalStatus.Paused, GoalStatus.Limited, GoalStatus.Stalled];

/**
 * The mark beside an agent's name while it is in goal mode: its goal is
 * working, or halted until its owner resumes it. Nothing once the goal is
 * complete or expired, or when it has none.
 */
export function GoalMark({ goal }: { goal: GoalReport | null | undefined }) {
  if (!goal) return null;
  const working = WORKING.includes(goal.status);
  if (!working && !HALTED.includes(goal.status)) return null;
  const why = detail(goal);
  const state = `${goalStatus(goal)}, ${goalProgress(goal)}${why ? `, ${why}` : ""}`;
  return (
    <span
      className={`badge goal${working ? "" : " halted"}`}
      title={`Goal (${state}): ${goal.objective}`}
      aria-label={`In goal mode, ${goalStatus(goal)}`}
    >
      ▸
    </span>
  );
}

/**
 * Whether the goal waits on the blocker chain that starts at `rootId`; null
 * when a blocked goal does not say which blocker it waits on.
 */
export function blocksGoal(goal: GoalReport | null | undefined, rootId: number): boolean | null {
  if (goal?.status !== GoalStatus.Blocked) return false;
  return goal.blocker_id ? goal.blocker_id === rootId : null;
}

/** Why the goal is where it is, in a few words. */
function detail(goal: GoalReport): string | null {
  switch (goal.status) {
    case GoalStatus.Waiting:
      return goal.wake_at ? `until ${clock(goal.wake_at)}${goal.reason ? `: ${goal.reason}` : ""}` : goal.reason;
    case GoalStatus.Complete:
      return goal.evidence;
    case GoalStatus.Blocked:
      return goal.blocker_id ? `on blocker #${goal.blocker_id}${goal.reason ? `: ${goal.reason}` : ""}` : goal.reason || null;
    default:
      return goal.reason || null;
  }
}

async function control(agent: string, control: number, body: string) {
  await rpc("send", { to: agent, body, control });
}

/** An RFC 3339 time as a datetime-local picker value, in local time. */
export function pickerValue(iso: string | null | undefined): string {
  if (!iso) return "";
  const t = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${t.getFullYear()}-${pad(t.getMonth() + 1)}-${pad(t.getDate())}T${pad(t.getHours())}:${pad(t.getMinutes())}`;
}

/**
 * The controls that turn the dialog into a goal. Editing a goal with the same
 * objective only moves its deadline, keeping its turns and status; a changed
 * objective sets a new goal. `due` is the picked deadline, null for none.
 */
export function goalControls(
  current: GoalReport | null,
  objective: string,
  due: string | null,
  deadlineChanged: boolean,
): [number, string][] {
  if (current && objective === current.objective) {
    return deadlineChanged ? [[Control.GoalDeadline, due ?? "none"]] : [];
  }
  return due ? [[Control.GoalSet, objective], [Control.GoalDeadline, due]] : [[Control.GoalSet, objective]];
}

/**
 * The owner's dialog for giving the agent a goal, replacing any it has, or
 * with `edit`, for changing the current one.
 */
export function GoalDialog({
  agent,
  current,
  edit = false,
  onClose,
  onNotice,
}: {
  agent: string;
  current: GoalReport | null;
  edit?: boolean;
  onClose: () => void;
  onNotice: (message: string) => void;
}) {
  const editing = edit ? current : null;
  const [objective, setObjective] = useState(editing?.objective ?? "");
  // A local date and time from the picker; empty for no deadline.
  const initialDeadline = pickerValue(editing?.deadline);
  const [deadline, setDeadline] = useState(initialDeadline);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const bytes = new TextEncoder().encode(objective.trim()).length;
  const set = async () => {
    setBusy(true);
    setError(null);
    try {
      const deadlineChanged = deadline !== initialDeadline;
      const date = deadline ? new Date(deadline) : null;
      const due = date ? date.toISOString().replace(/\.\d{3}Z$/, "Z") : null;
      const steps = goalControls(editing, objective.trim(), due, deadlineChanged);
      const sendsDeadline = steps.some(([kind, body]) => kind === Control.GoalDeadline && body !== "none");
      if (date && sendsDeadline && !(date.getTime() > Date.now())) throw new Error("The deadline must be in the future.");
      for (const [kind, body] of steps) await control(agent, kind, body);
      const when = due ? `, due ${fullTime(due)}` : "";
      if (steps.length === 0) onNotice("Nothing changed.");
      else if (steps[0][0] === Control.GoalSet)
        onNotice(`Asked ${agent}'s runner to work toward the ${editing ? "edited" : "new"} goal${when}.`);
      else onNotice(`Asked ${agent}'s runner to ${due ? `move its goal's deadline to ${fullTime(due)}` : "drop its goal's deadline"}.`);
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={editing ? `Edit ${agent}'s goal` : `Goal for ${agent}`}
      onClose={onClose}
      actions={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy || !objective.trim() || bytes > MAX_OBJECTIVE} onClick={set}>
            {editing ? "Save" : "Set goal"}
          </button>
        </>
      }
    >
      <p className="muted" style={{ marginTop: 0 }}>
        The runner prompts {agent} toward this, turn after turn, until it marks the goal complete, blocks or waits,
        the deadline passes, or you pause it.
        {editing
          ? " Changing only the deadline keeps the goal's turns and status; changing the objective starts it over."
          : current
            ? " This replaces the current goal."
            : ""}
      </p>
      <label className="field">
        <span>Objective, with what done looks like: merged, released, live, or a check that passes</span>
        <textarea
          className="textarea"
          rows={5}
          value={objective}
          onChange={(e) => setObjective(e.target.value)}
          placeholder="Get the integration tests passing on main; done when the fix is merged and CI is green"
          autoFocus
        />
      </label>
      <label className="field">
        <span>Deadline (optional)</span>
        <input className="input" type="datetime-local" value={deadline} onChange={(e) => setDeadline(e.target.value)} />
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
  onNew,
  onNotice,
}: {
  agent: string;
  goal: GoalReport;
  owned: boolean;
  onEdit: () => void;
  onNew: () => void;
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
  const running = ([GoalStatus.Active, GoalStatus.Waiting, GoalStatus.Blocked] as number[]).includes(goal.status);
  // Only these can be resumed; an unknown status offers just New and Clear.
  const stopped = ([GoalStatus.Paused, GoalStatus.Limited, GoalStatus.Stalled] as number[]).includes(goal.status);
  return (
    <div className={`attention goal goal-${status}`} title={`Set by ${goal.set_by}, ${fullTime(goal.set_at)}`}>
      <span className="goal-text">
        <span className="goal-status">goal · {status}</span>{" "}
        <span className="muted">
          {goalProgress(goal)}
          {goal.deadline && <> · due {clock(goal.deadline)}</>}
        </span>{" "}
        <span className="goal-objective">{goal.objective}</span>
        {why && <span className="muted"> — {why}</span>}
      </span>
      <span className="spacer" />
      {owned && (
        <>
          {running && (
            <button className="btn small" disabled={busy} onClick={() => act(Control.GoalPause, "Pause your goal.", `Asked ${agent}'s runner to pause its goal.`)}>
              Pause
            </button>
          )}
          {(running || goal.status === GoalStatus.Expired) && (
            <button
              className="btn small"
              disabled={busy}
              title="Move the deadline a day later; an expired goal resumes"
              onClick={() => {
                const due = extendedDeadline(goal);
                act(Control.GoalDeadline, due, `Asked ${agent}'s runner to move its goal's deadline to ${fullTime(due)}.`);
              }}
            >
              +1 day
            </button>
          )}
          {stopped && (
            <button className="btn small" disabled={busy} onClick={() => act(Control.GoalResume, "Resume your goal.", `Asked ${agent}'s runner to resume its goal.`)}>
              Resume
            </button>
          )}
          <button className="btn small" disabled={busy} onClick={onEdit} title="Change the objective or deadline">
            Edit
          </button>
          <button className="btn small" disabled={busy} onClick={onNew}>
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
