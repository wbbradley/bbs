import type { Seek } from "../useMessages";

/** What became of a message opened from search, when it is not plain. */
export function SeekNote({ seek }: { seek: Seek }) {
  if (seek === "loading") return <span>reading the found message…</span>;
  if (seek === "missing") return <span className="warning-text">that message is not in this view</span>;
  if (seek === "failed") return <span className="error-text">the found message could not be read</span>;
  return null;
}
