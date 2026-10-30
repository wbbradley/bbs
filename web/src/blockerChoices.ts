import { BlockerChoiceMode, type BlockerChoices } from "./types";
import { draftOwnerNow, ownedKey, storage } from "./util";

export const BLOCKER_CHOICES = "blocker-choices.v1";

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
