import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { dayLabel } from "../util";

/**
 * A scrolling thread, newest at the bottom. It follows new content until the
 * reader scrolls up, then counts what arrived until they return.
 */
export function Timeline({
  rows,
  version,
  empty,
  onFollowing,
  scrollTo,
}: {
  /** Rendered rows, each with the time it sits at, for day dividers. */
  rows: { key: string; at: string; node: ReactNode }[];
  /** Changes whenever content changes. */
  version: unknown;
  empty?: ReactNode;
  onFollowing?: (following: boolean) => void;
  /** An element id to bring into view once it renders. */
  scrollTo?: string | null;
}) {
  const box = useRef<HTMLDivElement>(null);
  const following = useRef(true);
  const [paused, setPaused] = useState(false);
  const [arrived, setArrived] = useState(0);
  const lastCount = useRef(rows.length);
  const report = useRef(onFollowing);
  report.current = onFollowing;

  const toBottom = useCallback(() => {
    const el = box.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, []);

  useLayoutEffect(() => {
    if (following.current) toBottom();
    else if (rows.length > lastCount.current) setArrived((n) => n + rows.length - lastCount.current);
    lastCount.current = rows.length;
  }, [version, rows.length, toBottom]);

  // Fonts and Markdown can change heights after layout; keep following.
  useEffect(() => {
    const el = box.current?.firstElementChild;
    if (!el) return;
    const observer = new ResizeObserver(() => following.current && toBottom());
    observer.observe(el);
    return () => observer.disconnect();
  }, [toBottom]);

  // A found message is brought into view once, when it first renders;
  // later content leaves the reader where they are.
  const scrolled = useRef<string | null>(null);
  useEffect(() => {
    if (!scrollTo) {
      scrolled.current = null;
      return;
    }
    if (scrolled.current === scrollTo) return;
    const el = document.getElementById(scrollTo);
    if (!el) return;
    scrolled.current = scrollTo;
    following.current = false;
    setPaused(true);
    report.current?.(false);
    el.scrollIntoView?.({ block: "center" });
  }, [scrollTo, version]);

  const onScroll = () => {
    const el = box.current!;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
    if (atBottom !== following.current) {
      following.current = atBottom;
      setPaused(!atBottom);
      if (atBottom) setArrived(0);
      report.current?.(atBottom);
    }
  };

  let lastDay = "";
  return (
    <div className="timeline" ref={box} onScroll={onScroll} style={{ position: "relative" }}>
      <div className="timeline-inner">
        {rows.length === 0 && empty}
        {rows.map((r) => {
          const day = r.at ? new Date(r.at).toDateString() : lastDay;
          const divider = day !== lastDay && r.at;
          lastDay = day;
          return (
            <div key={r.key} className="entry">
              {divider && <div className="time-divider">{dayLabel(r.at)}</div>}
              {r.node}
            </div>
          );
        })}
      </div>
      {paused && (
        <button
          className="btn small"
          style={{ position: "sticky", bottom: 10, left: "100%", marginRight: 16, float: "right" }}
          onClick={() => {
            following.current = true;
            setPaused(false);
            setArrived(0);
            report.current?.(true);
            toBottom();
          }}
        >
          ↓ {arrived > 0 ? `${arrived} new` : "Latest"}
        </button>
      )}
    </div>
  );
}
