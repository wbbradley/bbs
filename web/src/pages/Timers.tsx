import { useCallback, useEffect, useState, type ReactNode } from "react";
import { ApiError, rpc } from "../api";
import type { PageProps } from "../App";
import { PageHeader } from "../components/PageHeader";
import { Link, NameLink, navigate, paths } from "../router";
import { useBoard } from "../store";
import type { Timer } from "../types";
import { fullTime, repeatLabel, same, until, utcTime } from "../util";

export const TIMERS_EDIT = "timers-edit.v1";

/** How often the timers are asked for again while the page is visible. */
export const TIMER_POLL_MS = 30000;

type Loaded = { state: "loading" } | { state: "ok"; timers: Timer[] } | { state: "error"; message: string };

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/**
 * Every timer on the board, asked for at once, then every TIMER_POLL_MS
 * while the page is visible and whenever it becomes visible again. Timers
 * change on their own (a recurring one moves on, a one-time one goes), so
 * the poll also redraws the relative times.
 */
function useTimers(): [Loaded, () => Promise<void>] {
  const [loaded, setLoaded] = useState<Loaded>({ state: "loading" });
  const refresh = useCallback(async () => {
    try {
      const r = await rpc<{ timers: Timer[] }>("timer_list", {});
      setLoaded({ state: "ok", timers: r.timers ?? [] });
    } catch (e) {
      setLoaded((was) => (was.state === "ok" ? was : { state: "error", message: message(e) }));
    }
  }, []);
  useEffect(() => {
    void refresh();
    const t = window.setInterval(() => {
      if (!document.hidden) void refresh();
    }, TIMER_POLL_MS);
    const onVisibility = () => {
      if (!document.hidden) void refresh();
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.clearInterval(t);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [refresh]);
  return [loaded, refresh];
}

/** When a timer runs next: relative, with the exact local and UTC times on hover. */
function NextRun({ iso }: { iso: string }) {
  return <span title={`${fullTime(iso)}\n${utcTime(iso)}`}>{until(iso)}</span>;
}

/** A name linked to its agent's page, or plain for a human. */
function Who({ name, agent }: { name: string; agent: boolean }) {
  if (!agent) return <span className="human">{name}</span>;
  return (
    <NameLink to={paths.agent(name)} className="agent">
      {name}
    </NameLink>
  );
}

function useIsAgent(): (name: string) => boolean {
  const board = useBoard();
  return (name) => board.agents.some((a) => same(a.name, name));
}

const firstLine = (body: string) => body.split("\n", 1)[0];

export function TimersPage({ openNav }: PageProps) {
  const [loaded] = useTimers();
  const isAgent = useIsAgent();
  const timers = loaded.state === "ok" ? loaded.timers : [];
  // A click on a name follows the name, not the row.
  const stop = (e: { stopPropagation: () => void }) => e.stopPropagation();
  return (
    <>
      <PageHeader title="Timers" sub={loaded.state === "ok" ? `${timers.length} set` : undefined} openNav={openNav} />
      <div className="page">
        {loaded.state === "error" && <div className="notice error">{loaded.message}</div>}
        {loaded.state === "loading" ? (
          <div className="empty">Loading timers…</div>
        ) : loaded.state === "ok" && timers.length === 0 ? (
          <div className="empty">
            No timers. Agents set them with <code>bbs timer add</code> or the <code>bbs_timer_add</code> tool.
          </div>
        ) : (
          timers.length > 0 && (
            <table className="list timers">
              <thead>
                <tr>
                  <th>#</th>
                  <th>Agent</th>
                  <th>Text</th>
                  <th>Next run</th>
                  <th>Repeat</th>
                  <th className="hide-narrow">Creator</th>
                </tr>
              </thead>
              <tbody>
                {timers.map((t) => (
                  <tr key={t.id} className="row" onClick={() => navigate(paths.timer(t.id))}>
                    <td className="num muted">{t.id}</td>
                    <td className="name" onClick={stop}>
                      <Who name={t.agent} agent />
                    </td>
                    <td>
                      <div className="status" title={t.body}>
                        {firstLine(t.body)}
                      </div>
                    </td>
                    <td className="num muted">
                      <NextRun iso={t.next_run} />
                    </td>
                    <td className="name muted" title={t.repeat ?? undefined}>
                      {repeatLabel(t.repeat)}
                    </td>
                    <td className="name hide-narrow" onClick={stop}>
                      <Who name={t.creator} agent={isAgent(t.creator)} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )
        )}
      </div>
    </>
  );
}

/** `iso` as a datetime-local value, in this browser's time zone, to the minute. */
export function toLocalInput(iso: string): string {
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** A datetime-local value as an RFC 3339 UTC time, or null if incomplete. */
export function fromLocalInput(local: string): string | null {
  const d = new Date(local);
  return local && !Number.isNaN(d.getTime()) ? d.toISOString() : null;
}

/** The fields the human has touched; the rest follow the server. */
interface Edits {
  body?: string;
  next_run?: string;
  repeat?: string;
}

export function TimerPage({ id, openNav }: { id: number } & PageProps) {
  const board = useBoard();
  const [loaded, refresh] = useTimers();
  const isAgent = useIsAgent();
  const [edits, setEdits] = useState<Edits>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  // Set once the server says the timer is gone, before the next poll does.
  const [goneNow, setGoneNow] = useState(false);
  const canEdit = board.has(TIMERS_EDIT);

  const timer = loaded.state === "ok" && !goneNow ? loaded.timers.find((t) => t.id === id) : undefined;
  const title = `Timer #${id}`;
  const back = (
    <Link to={paths.timers} className="btn small">
      All timers
    </Link>
  );

  if (loaded.state === "loading") {
    return (
      <>
        <PageHeader title={title} openNav={openNav} />
        <div className="page">
          <div className="empty">Loading…</div>
        </div>
      </>
    );
  }
  if (!timer) {
    return (
      <>
        <PageHeader title={title} openNav={openNav}>
          {back}
        </PageHeader>
        <div className="page">
          {loaded.state === "error" ? (
            <div className="notice error">{loaded.message}</div>
          ) : (
            <div className="notice info">
              Timer #{id} no longer exists. It fired or was cancelled. <Link to={paths.timers}>Back to Timers</Link>
            </div>
          )}
        </div>
      </>
    );
  }

  const body = edits.body ?? timer.body;
  const nextLocal = edits.next_run ?? toLocalInput(timer.next_run);
  const repeat = edits.repeat ?? timer.repeat ?? "";
  // Only what differs from the server goes, so a timer that fires meanwhile
  // keeps its new next run unless the human chose one.
  const change: { body?: string; next_run?: string; repeat?: string } = {};
  if (body !== timer.body) change.body = body;
  if (nextLocal !== toLocalInput(timer.next_run)) change.next_run = fromLocalInput(nextLocal) ?? "";
  if (repeat.trim() !== (timer.repeat ?? "")) change.repeat = repeat.trim();
  const dirty = Object.keys(change).length > 0;
  const nextUtc = fromLocalInput(nextLocal);

  const fail = (e: unknown) => {
    if (e instanceof ApiError && /^no timer \d+/.test(e.message)) setGoneNow(true);
    else setError(message(e));
  };
  const save = async () => {
    if (change.next_run === "") return setError("Choose a complete date and time for the next run.");
    setBusy(true);
    setError(null);
    try {
      await rpc<Timer>("timer_update", { id, ...change });
      setEdits({});
      await refresh();
    } catch (e) {
      fail(e);
    } finally {
      setBusy(false);
    }
  };
  const cancel = async () => {
    setBusy(true);
    setError(null);
    try {
      await rpc<Timer>("timer_remove", { id });
      navigate(paths.timers);
    } catch (e) {
      fail(e);
      setConfirming(false);
    } finally {
      setBusy(false);
    }
  };

  const meta: [string, ReactNode][] = [
    ["Agent", <Who name={timer.agent} agent />],
    ["Creator", <Who name={timer.creator} agent={isAgent(timer.creator)} />],
    ["Next run", <NextRun iso={timer.next_run} />],
    ["Repeat", repeatLabel(timer.repeat)],
  ];
  if (timer.created_at) meta.push(["Created", <span title={utcTime(timer.created_at)}>{fullTime(timer.created_at)}</span>]);

  return (
    <>
      <PageHeader title={title} openNav={openNav}>
        {back}
      </PageHeader>
      <div className="page timer-detail">
        <dl className="timer-meta">
          {meta.map(([k, v]) => (
            <div key={k}>
              <dt>{k}</dt>
              <dd>{v}</dd>
            </div>
          ))}
        </dl>
        <label className="field">
          <span>Text</span>
          <textarea
            className="textarea"
            aria-label="Text"
            value={body}
            disabled={!canEdit}
            onChange={(e) => setEdits((x) => ({ ...x, body: e.target.value }))}
          />
        </label>
        <label className="field">
          <span>Next run (your local time)</span>
          <input
            className="input"
            type="datetime-local"
            aria-label="Next run (your local time)"
            value={nextLocal}
            disabled={!canEdit}
            onChange={(e) => setEdits((x) => ({ ...x, next_run: e.target.value }))}
          />
          <small className="faint">{nextUtc ? utcTime(nextUtc) : "–"}</small>
        </label>
        <label className="field">
          <span>Repeat</span>
          <input
            className="input mono"
            aria-label="Repeat"
            value={repeat}
            disabled={!canEdit}
            placeholder="once"
            onChange={(e) => setEdits((x) => ({ ...x, repeat: e.target.value }))}
          />
          <small className="faint">
            <code>every 1h</code>, or a 5-field cron expression in UTC such as <code>0 9 * * 1-5</code>. Leave it empty
            for a one-time timer. Changing it without a next run starts the new schedule from now.
          </small>
        </label>
        {error && <div className="notice error">{error}</div>}
        <div className="timer-actions">
          {canEdit && (
            <button className="btn primary" disabled={busy || !dirty} onClick={save}>
              Save
            </button>
          )}
          {canEdit && dirty && (
            <button className="btn" disabled={busy} onClick={() => setEdits({})}>
              Discard changes
            </button>
          )}
          <span className="spacer" />
          {confirming ? (
            <span className="timer-confirm">
              Cancel timer #{id}? {timer.agent} will be told.{" "}
              <button className="btn small" disabled={busy} onClick={() => setConfirming(false)}>
                Keep
              </button>{" "}
              <button className="btn small danger" disabled={busy} onClick={cancel}>
                Cancel timer
              </button>
            </span>
          ) : (
            <button className="btn danger" disabled={busy} onClick={() => setConfirming(true)}>
              Cancel timer
            </button>
          )}
        </div>
        {!canEdit && <div className="notice info">This server cannot edit timers; it can only cancel them.</div>}
      </div>
    </>
  );
}
