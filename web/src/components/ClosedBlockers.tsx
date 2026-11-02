import { useCallback, useEffect, useRef, useState } from "react";
import { rpc } from "../api";
import { Markdown } from "../Markdown";
import { BlockerStatus, Kind, type Blocker, type ListBlockersResponse } from "../types";
import { ago, fullTime } from "../util";
import { BlockerBody, BlockerCreator } from "./BlockerBody";

const PAGE = 50;
const statusName = (status: number) => status === BlockerStatus.Answered ? "Answered"
  : status === BlockerStatus.Cancelled ? "Cancelled"
  : status === BlockerStatus.Superseded ? "Superseded" : "Closed";

/** Closed current versions in the same scope as the open queue. No writes. */
export function ClosedBlockers() {
  const [listing, setListing] = useState<ListBlockersResponse | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const generation = useRef(0);

  const load = useCallback(async (before?: number) => {
    const request = ++generation.current;
    setBusy(true);
    setError(null);
    try {
      const page = await rpc<ListBlockersResponse>("list_blockers", {
        mine: true, closed_only: true, limit: PAGE,
        ...(before === undefined ? {} : { before_root_id: before }),
      });
      if (request !== generation.current) return;
      const next = page.next_before_root_id;
      if (page.has_more && (!Number.isSafeInteger(next) || (next ?? 0) <= 0 || (before !== undefined && next! >= before))) {
        throw new Error("The server returned a history cursor that did not advance.");
      }
      setListing((old) => ({
        ...page,
        blockers: before === undefined ? page.blockers : [...new Map(
          [...(old?.blockers ?? []), ...page.blockers].map((b) => [b.root_id, b]),
        ).values()],
      }));
    } catch (e) {
      if (request === generation.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (request === generation.current) setBusy(false);
    }
  }, []);

  useEffect(() => {
    load();
    return () => { generation.current++; };
  }, [load]);

  const rows = listing?.blockers.filter((b) => b.status !== BlockerStatus.Open) ?? [];
  const current = rows.find((b) => b.root_id === selected) ?? rows[0] ?? null;
  return (
    <section className="blockers" aria-label="Closed blockers" aria-busy={busy}>
      <div className="blocker-list">
        <div className="blocker-list-title closed-blockers-title">
          <span>Closed blockers</span>
          <button className="btn small" disabled={busy} onClick={() => load()}>{busy ? "Loading…" : error ? "Retry" : "Refresh"}</button>
        </div>
        {error && <div className="notice error" role="alert">Could not load closed blockers: {error}{listing ? ". Showing the last successful results." : ""}</div>}
        {!listing && !error && <div className="empty" role="status">Loading closed blockers…</div>}
        {listing && rows.length === 0 && <div className="empty">{listing.has_more ? "No closed blockers in these results. Load more to continue." : "No closed blockers."}</div>}
        {rows.map((b) => (
          <button type="button" key={b.root_id}
            className={`blocker-row closed-blocker-row${current?.root_id === b.root_id ? " sel" : ""}`}
            aria-pressed={current?.root_id === b.root_id}
            onClick={() => setSelected(b.root_id)}>
            <div className="meta">#{b.id} · {statusName(b.status)} · <span className="group">@{b.group}</span> · <BlockerCreator b={b} /> · {ago(b.outcome?.created_at ?? b.created_at)}</div>
            <div className="preview">{b.body.split("\n")[0].replace(/[*_`#>]/g, "")}</div>
          </button>
        ))}
        {listing?.has_more && <div className="blocker-list-title">
          <button className="btn small" disabled={busy} onClick={() => load(listing.next_before_root_id!)}>{busy ? "Loading…" : "Load more"}</button>
        </div>}
      </div>
      <div className="blocker-detail">
        {current ? <ClosedBlocker b={current} /> : <div className="empty">Select a closed blocker.</div>}
      </div>
    </section>
  );
}

function ClosedBlocker({ b }: { b: Blocker }) {
  const outcome = b.outcome;
  return <>
    <BlockerBody b={b} />
    <section className="blocker-outcome" aria-label="Blocker outcome">
      <div className="mono muted">
        <strong>{statusName(b.status)}</strong>
        {outcome?.actor && <> by <span className={outcome.actor.kind === Kind.Agent ? "agent" : "human"}>{outcome.actor.name}</span></>}
        {outcome && <> · <time dateTime={outcome.created_at} title={fullTime(outcome.created_at)}>{fullTime(outcome.created_at)}</time></>}
        {b.replaced_by && <> · replaced by #{b.replaced_by}</>}
      </div>
      {outcome?.answer && <Markdown text={outcome.answer} />}
    </section>
  </>;
}
