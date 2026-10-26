import { useEffect, useState } from "react";
import { rpc } from "../api";
import type { PageProps } from "../App";
import { Markdown } from "../Markdown";
import { PageHeader } from "../components/PageHeader";
import { Link, paths } from "../router";
import { useBoard } from "../store";
import { BlockerStatus, Kind, type Blocker, type BlockerMutationResponse } from "../types";
import { ago, fullTime, loadDraft, randomKey, same, saveDraft } from "../util";

/** One write per blocker at a time; a retry repeats the same request key. */
interface Write {
  kind: "answer" | "cancel";
  key: string;
  state: "sending" | "unconfirmed" | "refused";
  error?: string;
}

export function BlockersPage({ openNav }: PageProps) {
  const board = useBoard();
  const list = board.blockers?.blockers ?? [];
  const [selected, setSelected] = useState<number | null>(null);
  const [writes, setWrites] = useState<Record<number, Write>>({});
  const [done, setDone] = useState<string | null>(null);

  // Keep the selection on the same queue position through replacements.
  const current =
    list.find((b) => b.root_id === selected) ?? (list.length > 0 && selected === null ? list[0] : null) ?? list[0] ?? null;
  useEffect(() => {
    if (current && current.root_id !== selected) setSelected(current.root_id);
  }, [current, selected]);

  const write = async (b: Blocker, kind: Write["kind"], answer?: string) => {
    const prior = writes[b.id];
    const key = prior && prior.kind === kind && prior.state === "unconfirmed" ? prior.key : randomKey();
    setWrites((w) => ({ ...w, [b.id]: { kind, key, state: "sending" } }));
    setDone(null);
    try {
      const r =
        kind === "answer"
          ? await rpc<BlockerMutationResponse>("answer_blocker", { id: b.id, answer, request_key: key })
          : await rpc<BlockerMutationResponse>("cancel_blocker", { id: b.id, request_key: key });
      setWrites((w) => {
        const next = { ...w };
        delete next[b.id];
        return next;
      });
      if (kind === "answer") saveDraft(`blocker:${b.root_id}`, "");
      setDone(`${kind === "answer" ? "Answered" : "Cancelled"} #${r.blocker?.id ?? b.id} in @${b.group}.`);
      board.refreshBlockers();
    } catch (e) {
      const err = e as { status?: number; message?: string };
      const state = !err.status || err.status >= 500 ? "unconfirmed" : "refused";
      setWrites((w) => ({ ...w, [b.id]: { kind, key, state, error: err.message } }));
    }
  };

  const unsupported = !board.has("blockers-mine.v1");
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
            {list.length === 0 && (
              <div className="empty">{board.blockers ? "No open blockers." : board.blockersError ?? "Loading…"}</div>
            )}
            {list.map((b) => (
              <div
                key={b.root_id}
                className={`blocker-row${current?.root_id === b.root_id ? " sel" : ""}`}
                onClick={() => setSelected(b.root_id)}
              >
                <div className="meta">
                  #{b.id} <span className="group">@{b.group}</span> · <Creator b={b} /> · {ago(b.created_at)}
                </div>
                <div className="preview">{b.body.split("\n")[0].replace(/[*_`#>]/g, "")}</div>
              </div>
            ))}
            {board.blockers?.has_more && <div className="blocker-row faint">More beyond the first 50…</div>}
          </div>
          <div className="blocker-detail">
            {done && <div className="notice info">{done}</div>}
            {current ? (
              <BlockerCard key={current.id} b={current} write={writes[current.id]} onWrite={write} />
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
  write,
  onWrite,
}: {
  b: Blocker;
  write?: Write;
  onWrite: (b: Blocker, kind: Write["kind"], answer?: string) => void;
}) {
  const board = useBoard();
  const draftKey = `blocker:${b.root_id}`;
  const [answer, setAnswer] = useState(() => loadDraft(draftKey));
  const [confirmCancel, setConfirmCancel] = useState(false);
  useEffect(() => saveDraft(draftKey, answer), [draftKey, answer]);
  const sending = write?.state === "sending";
  const creatorAgent = b.creator?.kind === Kind.Agent ? b.creator : null;
  const live = creatorAgent && board.agents.some((a) => a.id === creatorAgent.id && same(a.name, creatorAgent.name));
  const reason = !b.can_answer
    ? creatorAgent && !live
      ? "The agent that asked is gone, so it cannot be answered."
      : "You have no agent in this group, so you cannot answer it."
    : null;

  return (
    <div>
      <div className="mono muted" style={{ fontSize: 12, marginBottom: 8 }}>
        #{b.id} in <span className="group">@{b.group}</span> from <Creator b={b} />, {ago(b.created_at)} ·{" "}
        <span title={fullTime(b.created_at)}>{new Date(b.created_at).toLocaleString()}</span>
        {b.replaces && <span> · replaces #{b.replaces}</span>}
      </div>
      <Markdown text={b.body} />
      <div style={{ marginTop: 18 }}>
        {b.status !== BlockerStatus.Open && <div className="notice warning">No longer open.</div>}
        {reason && <div className="notice info">{reason}</div>}
        {b.can_answer && (
          <>
            <div className="mono muted" style={{ fontSize: 12, marginBottom: 4 }}>
              answer → <Creator b={b} /> <span className="group">@{b.group}</span>
            </div>
            <textarea
              className="textarea"
              value={answer}
              disabled={sending}
              onChange={(e) => setAnswer(e.target.value)}
              placeholder="Your answer goes to the agent as mail."
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && answer.trim()) onWrite(b, "answer", answer);
              }}
            />
          </>
        )}
        <div style={{ display: "flex", gap: 8, marginTop: 8, alignItems: "center", flexWrap: "wrap" }}>
          {b.can_answer && (
            <button
              className="btn primary"
              disabled={sending || !answer.trim()}
              onClick={() => onWrite(b, "answer", answer)}
            >
              {sending && write?.kind === "answer" ? "Sending…" : "Send answer"}
            </button>
          )}
          {creatorAgent && live && (
            <Link to={paths.agent(creatorAgent.name)} className="btn">
              Discuss with {creatorAgent.name}
            </Link>
          )}
          {b.can_cancel &&
            (confirmCancel ? (
              <>
                <span className="warning-text">Cancel this blocker? No mail is sent.</span>
                <button className="btn danger" disabled={sending} onClick={() => onWrite(b, "cancel")}>
                  Yes, cancel it
                </button>
                <button className="btn" onClick={() => setConfirmCancel(false)}>
                  Back
                </button>
              </>
            ) : (
              <button className="btn danger" disabled={sending} onClick={() => setConfirmCancel(true)}>
                Cancel blocker
              </button>
            ))}
          <span className="faint" style={{ fontSize: 12 }}>
            ⌘Enter sends
          </span>
        </div>
        {write?.state === "unconfirmed" && (
          <div className="notice warning">Not confirmed: {write.error}. Sending again repeats the same request.</div>
        )}
        {write?.state === "refused" && <div className="notice error">{write.error}</div>}
      </div>
    </div>
  );
}
