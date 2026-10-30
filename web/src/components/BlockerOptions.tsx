import { useId } from "react";
import { BlockerChoiceMode, type BlockerChoices } from "../types";

export function BlockerOptions({ choices, selected, disabled, onPick, onChange }: {
  choices: BlockerChoices;
  selected: string[];
  disabled: boolean;
  onPick: (id: string) => void;
  onChange: (ids: string[]) => void;
}) {
  const prefix = useId();
  const one = choices.mode === BlockerChoiceMode.SelectOne;
  const many = choices.mode === BlockerChoiceMode.SelectMany;
  if (!one && !many) return <div className="notice warning">These options cannot be shown. You can still write an answer.</div>;
  const min = choices.min ?? 1;
  const max = choices.max ?? choices.options.length;
  return (
    <fieldset className="blocker-options" disabled={disabled}>
      <legend>{one ? "Choose an answer" : min === max ? `Select ${min}` : `Select ${min} to ${max}`}</legend>
      <p className="muted">{one ? "Choosing an option sends it immediately, with any note below." : "Choose options, then send your answer. You can also clear them and answer with only a note."}</p>
      <div className="blocker-option-list">
        {choices.options.map((option) => {
          const detailId = `${prefix}-${option.id}`;
          const content = <span><span className="blocker-option-label">{option.label}</span>{option.detail && <span id={detailId} className="blocker-option-detail muted">{option.detail}</span>}</span>;
          const props = { "aria-label": option.label, "aria-describedby": option.detail ? detailId : undefined };
          return one ? (
            <button key={option.id} type="button" disabled={disabled} className={`blocker-option${selected.includes(option.id) ? " selected" : ""}`} {...props} onClick={() => onPick(option.id)}>
              {content}
            </button>
          ) : (
            <label key={option.id} className={`blocker-option${selected.includes(option.id) ? " selected" : ""}`}>
              <input type="checkbox" disabled={disabled} checked={selected.includes(option.id)} {...props} onChange={(e) => onChange(e.target.checked ? [...selected, option.id] : selected.filter((id) => id !== option.id))} />
              {content}
            </label>
          );
        })}
      </div>
    </fieldset>
  );
}
