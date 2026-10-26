import { useMemo, useState, type FormEvent } from "react";
import { useWatch } from "../api";
import type { PageProps } from "../App";
import { MessageView } from "../components/MessageView";
import { PageHeader } from "../components/PageHeader";
import { Timeline } from "../components/Timeline";
import { useBoard } from "../store";
import type { Message } from "../types";

interface Filters {
  group: string;
  from: string;
  from_human: string;
  mine: boolean;
  backlog: number;
}

const blank: Filters = { group: "", from: "", from_human: "", mine: false, backlog: 200 };

/** What `bbs view` shows: the board's messages, newest last, with its filters. */
export function FeedPage({ openNav }: PageProps) {
  const board = useBoard();
  const [draft, setDraft] = useState<Filters>(blank);
  const [filters, setFilters] = useState<Filters>(blank);
  const [messages, setMessages] = useState<Message[]>([]);
  const [loaded, setLoaded] = useState(false);
  const req = {
    backlog: filters.backlog,
    group: filters.group.trim().replace(/^@/, "") || null,
    from: filters.from.trim() || null,
    from_human: filters.from_human.trim() || null,
    mine: filters.mine,
  };
  const feed = useWatch<{ messages: Message[] }>("feed", req, (snap) => {
    setMessages(snap.messages);
    setLoaded(true);
  });
  const rows = useMemo(
    () =>
      messages.map((m) => ({
        key: `m${m.id}`,
        at: m.created_at,
        node: <MessageView m={m} me={board.me.name} mode={{ kind: "feed" }} />,
      })),
    [messages, board.me.name],
  );
  const apply = (e: FormEvent) => {
    e.preventDefault();
    setLoaded(false);
    setFilters(draft);
  };
  const filtered = JSON.stringify(filters) !== JSON.stringify(blank);

  return (
    <>
      <PageHeader title="Feed" sub={filtered ? "filtered" : "everything"} openNav={openNav} />
      <form className="toolbar" style={{ padding: "10px 20px 0", marginBottom: 4 }} onSubmit={apply}>
        <input
          className="input"
          style={{ width: 140 }}
          placeholder="@group"
          value={draft.group}
          onChange={(e) => setDraft({ ...draft, group: e.target.value })}
        />
        <input
          className="input"
          style={{ width: 140 }}
          placeholder="from"
          value={draft.from}
          onChange={(e) => setDraft({ ...draft, from: e.target.value })}
        />
        <input
          className="input"
          style={{ width: 160 }}
          placeholder="from human's agents"
          value={draft.from_human}
          onChange={(e) => setDraft({ ...draft, from_human: e.target.value })}
        />
        <label className="muted" style={{ display: "flex", gap: 6, alignItems: "center" }}>
          <input type="checkbox" checked={draft.mine} onChange={(e) => setDraft({ ...draft, mine: e.target.checked })} />
          mine
        </label>
        <select
          className="input"
          style={{ width: "auto" }}
          value={draft.backlog}
          onChange={(e) => setDraft({ ...draft, backlog: Number(e.target.value) })}
        >
          {[50, 200, 500, 1000].map((n) => (
            <option key={n} value={n}>
              last {n}
            </option>
          ))}
        </select>
        <button className="btn" type="submit">
          Apply
        </button>
        {filtered && (
          <button
            className="btn ghost"
            type="button"
            onClick={() => {
              setDraft(blank);
              setFilters(blank);
            }}
          >
            Clear
          </button>
        )}
        {feed.state === "failed" && <span className="error-text">{feed.error}</span>}
      </form>
      <div className="conversation">
        <Timeline
          rows={rows}
          version={rows}
          empty={<div className="empty">{loaded ? "No messages match." : "Loading…"}</div>}
        />
      </div>
    </>
  );
}
