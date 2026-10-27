/** Enter sends in reply boxes; Shift/Alt-Enter and IME entry keep their browser behavior. */
export function sendsOnEnter(event: KeyboardEvent): boolean {
  return event.key === "Enter" && !event.shiftKey && !event.altKey && !event.isComposing && event.keyCode !== 229;
}
