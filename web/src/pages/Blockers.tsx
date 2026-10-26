import { useCallback, useEffect, useRef, useState } from "react";
import { rpc } from "../api";
import type { PageProps } from "../App";
import { Markdown } from "../Markdown";
import { PageHeader } from "../components/PageHeader";
import { Link, paths } from "../router";
import { useBoard } from "../store";
import { BlockerStatus, Kind, type Blocker, type BlockerMutationResponse } from "../types";
import { ago, draftOwnerNow, fullTime, loadDraft, ownedKey, randomKey, same, saveDraft, storage } from "../util";

/** After a blocker's text changes under the reader, its actions wait this long. */
export const REPLACED_GUARD_MS = 1000;

/**
 * One write per blocker (by root) at a time, kept in this browser for its
 * human until it is settled or dismissed. An unconfirmed write repeats the
 * exact request, its key, version and answer, never an edited one.
 */
export interface Kept {
  root: number;
  /** The version written to. */
  id: number;
  kind: "answer" | "cancel";
  key: string;
  answer?: string;
  state: "sending" | "unconfirmed" | "refused";
  error?: string;
  /** The row as it was, to show if the server stops listing it. */
  row: Blocker;
}

function keptKey(owner: string | null): string | null {
  return ownedKey("blocker-writes", "all", owner);
}

export function loadKept(owner: string | null = draftOwnerNow()): Record<number, Kept> {
  const key = keptKey(owner);
  if (key === null) return {};
  try {
    const kept = JSON.parse(storage.get(key) ?? "{}") as Record<number, Kept>;
    // A write cut off mid-flight is unconfirmed: only a repeat can tell.
    for (const k of Object.values(kept)) if (k.state === "sending") k.state = "unconfirmed";
    return kept;
  } catch {
    return {};
  }
}

function storeKept(kept: Record<number, Kept>, owner: string | null) {
  const key = keptKey(owner);
  if (key === null) return;
  if (Object.keys(kept).length === 0) storage.remove(key);
  else storage.set(key, JSON.stringify(kept));
}

/** Where the selection goes when its row leaves: the row now at its place. */
export function nearest(selected: number | null, old: number[], now: number[]): number | null {
  if (now.length === 0) return null;
  if (selected !== null && now.includes(selected)) return selected;
  const at = selected === null ? 0 : Math.max(0, old.indexOf(selected));
  return now[Math.min(at, now.length - 1)];
}

export function BlockersPage({ openNav }: PageProps) {
  const board = useBoard();
  // The human this page is for; a write ends as theirs even if another
  // human has signed in by the time it does.
  const [owner] = useState(draftOwnerNow);
  const [kept, setKept] = useState<Record<number, Kept>>(() => loadKept(owner));
  // Set on every mount, as StrictMode mounts twice in development.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const stillTheirs = () => mounted.current && draftOwnerNow() === owner;
  const [selected, setSelected] = useState<number | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const [guardUntil, setGuardUntil] = useState(0);
  const [, tick] = useState(0);

  // Kept writes change for this page's human only: shown if the page is
  // still theirs, and stored under their name either way.
  const update = useCallback(
    (f: (k: Record<number, Kept>) => Record<number, Kept>) => {
      if (mounted.current && draftOwnerNow() === owner) {
        setKept((old) => {
          const next = f(old);
          storeKept(next, owner);
          return next;
        });
      } else {
        storeKept(f(loadKept(owner)), owner);
      }
    },
    [owner],
  );

  // The server's rows, then rows it no longer lists that hold a write's
  // outcome, until dismissed.
  const listed = board.blockers?.blockers ?? [];
  const listedRoots = new Set(listed.map((b) => b.root_id));
  const held = Object.values(kept)
    .filter((k) => !listedRoots.has(k.root))
    .map((k) => k.row);
  const rows = [...listed, ...held];
  const roots = rows.map((b) => b.root_id);

  const order = useRef<number[]>([]);
  const target = nearest(selected, order.current, roots);
  const current = rows.find((b) => b.root_id === target) ?? null;
  useEffect(() => {
    order.current = roots;
    if (target !== selected) setSelected(target);
  });

  // A replaced or reworded blocker holds its actions briefly.
  const seen = useRef<{ root: number; id: number; body: string } | null>(null);
  useEffect(() => {
    if (!current) return;
    const was = seen.current;
    seen.current = { root: current.root_id, id: current.id, body: current.body };
    if (was && was.root === current.root_id && (was.id !== current.id || was.body !== current.body)) {
      setGuardUntil(Date.now() + REPLACED_GUARD_MS);
      setDone("Blocker updated.");
      const t = window.setTimeout(() => tick((n) => n + 1), REPLACED_GUARD_MS);
      return () => window.clearTimeout(t);
    }
  }, [current]);
  const guarded = Date.now() < guardUntil;

  const write = async (b: Blocker, kind: Kept["kind"], answer?: string) => {
    const prior = kept[b.root_id];
    // A repeat is the very request that went unconfirmed.
    const op: Kept =
      prior && prior.state === "unconfirmed"
        ? { ...prior, state: "sending", error: undefined }
        : { root: b.root_id, id: b.id, kind, key: randomKey(), answer, state: "sending", row: b };
    update((k) => ({ ...k, [b.root_id]: op }));
    setDone(null);
    try {
      const r =
        op.kind === "answer"
          ? await rpc<BlockerMutationResponse>("answer_blocker", { id: op.id, answer: op.answer, request_key: op.key })
          : await rpc<BlockerMutationResponse>("cancel_blocker", { id: op.id, request_key: op.key });
      update((k) => {
        const next = { ...k };
        delete next[op.root];
        return next;
      });
      if (op.kind === "answer") saveDraft(`blocker:${op.root}`, "", owner);
      if (!stillTheirs()) return;
      setDone(`${op.kind === "answer" ? "Answered" : "Cancelled"} #${r.blocker?.id ?? op.id} in @${op.row.group}.`);
      board.refreshBlockers();
    } catch (e) {
      const err = e as { status?: number; message?: string };
      const state = !err.status || err.status >= 500 || err.status === 429 ? "unconfirmed" : "refused";
      update((k) => ({ ...k, [op.root]: { ...op, state, error: err.message } }));
    }
  };

  const dismiss = (root: number) => {
    update((k) => {
      const next = { ...k };
      delete next[root];
      return next;
    });
    if (!listedRoots.has(root)) saveDraft(`blocker:${root}`, "", owner);
  };

  const unsupported = board.info !== null && !board.has("blockers-mine.v1");
  return (
    <>
      <PageHeader title="Blockers" sub={`${board.blockers?.active_count ?? 0} open`} openNav={openNav} />
      {unsupported ? (
        <div className="page">
          <div className="notice warning">The blockers queue needs a newer server (blockers-mine.v1).</div>
        </div>
      ) : (
        <div className="blockers">
          <div className="blocker-list">
            <div className="blocker-list-title">Open blockers</div>
            {rows.length === 0 && (
              <div className="empty">{board.blockers ? "No open blockers." : board.blockersError ?? "Loading…"}</div>
            )}
            {rows.map((b) => (
              <div
                key={b.root_id}
                className={`blocker-row${current?.root_id === b.root_id ? " sel" : ""}`}
                onClick={() => {
                  setSelected(b.root_id);
                  setDone(null);
                }}
              >
                <div className="meta">
                  #{b.id} <span className="group">@{b.group}</span> · <Creator b={b} /> · {ago(b.created_at)}
                  {!listedRoots.has(b.root_id) && <span className="faint"> · no longer open</span>}
                </div>
                <div className="preview">{b.body.split("\n")[0].replace(/[*_`#>]/g, "")}</div>
              </div>
            ))}
            {board.blockers?.has_more && <div className="blocker-row faint">More beyond the first 50…</div>}
          </div>
          <div className="blocker-detail">
            {done && <div className="notice info">{done}</div>}
            {current ? (
              <BlockerCard
                key={current.id}
                b={current}
                held={!listedRoots.has(current.root_id)}
                kept={kept[current.root_id]}
                guarded={guarded}
                onWrite={write}
                onDismiss={dismiss}
              />
            ) : (
              <div className="empty">Nothing needs you.</div>
            )}
          </div>
        </div>
      )}
    </>
  );
}

function Creator({ b }: { b: Blocker }) {
  if (!b.creator) return <span className="faint">unknown</span>;
  return <span className={b.creator.kind === Kind.Agent ? "agent" : "human"}>{b.creator.name}</span>;
}

function BlockerCard({
  b,
  held,
  kept,
  guarded,
  onWrite,
  onDismiss,
}: {
  b: Blocker;
  /** Listed only for its write's outcome: read only until dismissed. */
  held: boolean;
  kept?: Kept;
  guarded: boolean;
  onWrite: (b: Blocker, kind: Kept["kind"], answer?: string) => void;
  onDismiss: (root: number) => void;
}) {
  const board = useBoard();
  const draftKey = `blocker:${b.root_id}`;
  const [answer, setAnswer] = useState(() => loadDraft(draftKey));
  const [confirmCancel, setConfirmCancel] = useState(false);
  useEffect(() => saveDraft(draftKey, answer), [draftKey, answer]);
  useEffect(() => {
    if (guarded) setConfirmCancel(false);
  }, [guarded]);
  const sending = kept?.state === "sending";
  const unconfirmed = kept?.state === "unconfirmed";
  // While a write is out or unsettled, its answer is the one shown, unchanged.
  const frozen = sending || unconfirmed;
  const shown = frozen && kept?.kind === "answer" ? (kept.answer ?? "") : answer;
  const creatorAgent = b.creator?.kind === Kind.Agent ? b.creator : null;
  const live = creatorAgent && board.agents.some((a) => a.id === creatorAgent.id && same(a.name, creatorAgent.name));
  const open = !held && b.status === BlockerStatus.Open;
  const reason = !b.can_answer
    ? creatorAgent && !live
      ? "The agent that asked is gone, so it cannot be answered."
      : "You have no agent in this group, so you cannot answer it."
    : null;
  const canAnswer = open && b.can_answer && !frozen;
  const send = () => {
    if (canAnswer && !guarded && answer.trim()) onWrite(b, "answer", answer);
  };

  return (
    <div>
      <div className="mono muted" style={{ fontSize: 12, marginBottom: 8 }}>
        #{b.id} in <span className="group">@{b.group}</span> from <Creator b={b} />, {ago(b.created_at)} ·{" "}
        <span title={fullTime(b.created_at)}>{new Date(b.created_at).toLocaleString()}</span>
        {b.replaces && <span> · replaces #{b.replaces}</span>}
      </div>
      <Markdown text={b.body} />
      <div style={{ marginTop: 18 }}>
        {!open && <div className="notice warning">No longer open.</div>}
        {open && reason && <div className="notice info">{reason}</div>}
        {(b.can_answer || shown) && (
          <>
            <div className="mono muted" style={{ fontSize: 12, marginBottom: 4 }}>
              answer → <Creator b={b} /> <span className="group">@{b.group}</span>
            </div>
            <textarea
              className="textarea"
              value={shown}
              readOnly={!canAnswer}
              onChange={(e) => canAnswer && setAnswer(e.target.value)}
              placeholder="Your answer goes to the agent as mail."
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) send();
              }}
            />
          </>
        )}
        <div style={{ display: "flex", gap: 8, marginTop: 8, alignItems: "center", flexWrap: "wrap" }}>
          {unconfirmed && (
            <button className="btn primary" disabled={guarded} onClick={() => onWrite(b, kept!.kind)}>
              Retry the same {kept!.kind === "answer" ? "answer" : "cancel"}
            </button>
          )}
          {canAnswer && (
            <button className="btn primary" disabled={guarded || !answer.trim()} onClick={send}>
              Send answer
            </button>
          )}
          {sending && <span className="muted">Sending…</span>}
          {creatorAgent && live && (
            <Link to={paths.agent(creatorAgent.name)} className="btn">
              Discuss with {creatorAgent.name}
            </Link>
          )}
          {open &&
            b.can_cancel &&
            !frozen &&
            (confirmCancel ? (
              <>
                <span className="warning-text">Cancel this blocker? No mail is sent.</span>
                <button className="btn danger" disabled={guarded} onClick={() => onWrite(b, "cancel")}>
                  Yes, cancel it
                </button>
                <button className="btn" onClick={() => setConfirmCancel(false)}>
                  Back
                </button>
              </>
            ) : (
              <button className="btn danger" disabled={guarded} onClick={() => setConfirmCancel(true)}>
                Cancel blocker
              </button>
            ))}
          {kept && !sending && (
            <button className="btn ghost" onClick={() => onDismiss(b.root_id)}>
              Dismiss
            </button>
          )}
          {canAnswer && (
            <span className="faint" style={{ fontSize: 12 }}>
              ⌘Enter sends
            </span>
          )}
        </div>
        {unconfirmed && (
          <div className="notice warning">
            Not confirmed: {kept!.error}. Retrying repeats the same request, which the server applies at most once.
          </div>
        )}
        {kept?.state === "refused" && <div className="notice error">{kept.error}</div>}
      </div>
    </div>
  );
}
