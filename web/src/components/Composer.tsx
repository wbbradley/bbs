import { forwardRef, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { loadDraft, saveDraft } from "../util";
import { sendsOnEnter } from "../keys";

/** Empty placeholders do not add rows; non-empty text fits its wrapped height. */
function fit(area: HTMLTextAreaElement) {
  area.style.height = "auto";
  if (area.value) area.style.height = `${area.scrollHeight}px`;
}

export interface ComposerHandle {
  focus: () => void;
}

/**
 * A growing text box. Enter sends; Shift-Enter adds a line. The
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
  const [text, setText] = useState(() => {
    const draft = loadDraft(draftKey);
    return draft.trim() ? draft : "";
  });
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const area = useRef<HTMLTextAreaElement>(null);

  useImperativeHandle(ref, () => ({ focus: () => area.current?.focus() }), []);
  useEffect(() => saveDraft(draftKey, text), [draftKey, text]);
  useLayoutEffect(() => {
    if (area.current) fit(area.current);
  }, [text]);
  useEffect(() => {
    const el = area.current;
    if (!el) return;
    let width = el.clientWidth;
    const observer = new ResizeObserver(() => {
      if (el.clientWidth === width) return;
      width = el.clientWidth;
      fit(el);
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

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
          <textarea
            ref={area}
            rows={1}
            value={text}
            placeholder={placeholder}
            disabled={disabled}
            autoFocus
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (sendsOnEnter(e.nativeEvent)) {
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
