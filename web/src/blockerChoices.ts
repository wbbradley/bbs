import { BlockerChoiceMode, type BlockerChoices, type BlockerGoal } from "./types";
import { draftOwnerNow, ownedKey, storage } from "./util";

export const BLOCKER_CHOICES = "blocker-choices.v1";

const GOAL_HEADING = "\n\n**Proposed goal**";

/** A goal proposal's details without the goal the server appended to them, so
 * the board shows the goal only in its own box and an agent's text cannot pass
 * for it. Bodies without the appended goal are returned whole. */
export function goalDetails(body: string, goal: BlockerGoal): string {
  const longest = Math.max(0, ...(goal.objective.match(/`+/g) ?? []).map((run) => run.length));
  const fence = "`".repeat(Math.max(longest, 2) + 1);
  const tail = ` Approving sets it, replacing any current goal and its deadline:\n\n${fence}\n${goal.objective}\n${fence}`;
  if (!body.endsWith(tail)) return body;
  // The heading and time limit precede the tail; the objective is past it.
  const head = body.slice(0, body.length - tail.length);
  const at = head.lastIndexOf(GOAL_HEADING);
  return at < 0 ? body : head.slice(0, at);
}

/** A time limit in whole units, such as "1d 2h 30m". */
export function goalLimit(seconds: number): string {
  const parts: string[] = [];
  for (const [size, unit] of [[86400, "d"], [3600, "h"], [60, "m"], [1, "s"]] as const) {
    if (seconds >= size) {
      parts.push(`${Math.floor(seconds / size)}${unit}`);
      seconds %= size;
    }
  }
  return parts.join(" ") || "0s";
}

/** A valid selection in declared order, or null. The server validates again. */
export function answerSelection(choices: BlockerChoices | null | undefined, ids: string[], note: string): string[] | null {
  if (ids.length === 0) return note.trim() ? [] : null;
  if (!choices || new Set(ids).size !== ids.length || ids.some((id) => !choices.options.some((o) => o.id === id))) return null;
  if (choices.mode === BlockerChoiceMode.SelectOne) {
    if (ids.length !== 1) return null;
  } else if (choices.mode === BlockerChoiceMode.SelectMany) {
    if (ids.length < (choices.min ?? 1) || ids.length > (choices.max ?? choices.options.length)) return null;
  } else {
    return null;
  }
  return choices.options.filter((o) => ids.includes(o.id)).map((o) => o.id);
}

export function loadSelection(id: number, owner: string | null = draftOwnerNow()): string[] {
  const key = ownedKey("blocker-selection", String(id), owner);
  if (!key) return [];
  try {
    const value: unknown = JSON.parse(storage.get(key) ?? "[]");
    return Array.isArray(value) && value.every((v) => typeof v === "string") ? value : [];
  } catch {
    return [];
  }
}

export function saveSelection(id: number, ids: string[], owner: string | null = draftOwnerNow()) {
  const key = ownedKey("blocker-selection", String(id), owner);
  if (!key) return;
  if (ids.length) storage.set(key, JSON.stringify(ids));
  else storage.remove(key);
}
