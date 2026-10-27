import { forwardRef, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { loadDraft, saveDraft } from "../util";

export interface ComposerHandle {
  focus: () => void;
}

/**
 * A growing text box. Enter sends; Shift-Enter or Alt-Enter adds a line. The
 * draft is kept in this browser per discussion until it is sent.
 */
export const Composer = forwardRef<
  ComposerHandle,
  {
    draftKey: string;
    placeholder: string;
    disabled?: boolean;
    /** Resolves true when the text was taken and the draft should clear. */
    onSubmit: (text: string) => Promise<boolean>;
    hint?: ReactNode;
    above?: ReactNode;
  }
>(function Composer({ draftKey, placeholder, disabled, onSubmit, hint, above }, ref) {
  const [text, setText] = useState(() => loadDraft(draftKey));
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const area = useRef<HTMLTextAreaElement>(null);

  useImperativeHandle(ref, () => ({ focus: () => area.current?.focus() }), []);
  useEffect(() => saveDraft(draftKey, text), [draftKey, text]);
  useLayoutEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight}px`;
  }, [text]);

  const submit = async () => {
    const body = text;
    if (!body.trim() || sending) return;
    setSending(true);
    setError(null);
    try {
      if (await onSubmit(body)) {
        // Only clear what was sent; typing during the send stays.
        setText((now) => (now === body ? "" : now));
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSending(false);
      area.current?.focus();
    }
  };

  return (
    <div className="composer">
      <div className="composer-inner">
        {above}
        <div className="composer-box" style={sending ? { opacity: 0.7 } : undefined}>
          <span className="prompt">›</span>
          <textarea
            ref={area}
            rows={1}
            value={text}
            placeholder={placeholder}
            disabled={disabled}
            autoFocus
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.altKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                submit();
              }
            }}
          />
          <button className="btn primary small" onClick={submit} disabled={disabled || sending || !text.trim()}>
            {sending ? "Sending…" : "Send"}
          </button>
        </div>
        <div className="composer-hint">
          <span className={error ? "error-text" : undefined}>{error ?? hint}</span>
          <span>Enter sends · Shift-Enter new line</span>
        </div>
      </div>
    </div>
  );
});
