import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { rpc } from "./api";
import { Kind, type Message, type SearchHit } from "./types";
import { navigate, paths } from "./router";
import { useBoard } from "./store";
import { useCover } from "./cover";
import { ago, same, senderName } from "./util";

interface Result {
  key: string;
  kind: string;
  label: ReactNode;
  detail?: ReactNode;
  go: () => void;
}

/** Literal match first, then letters in order (fuzzy); null when neither. */
function score(text: string, query: string): number | null {
  const t = text.toLowerCase();
  const q = query.toLowerCase();
  const at = t.indexOf(q);
  if (at >= 0) return at === 0 ? (t === q ? 0 : 1) : 2;
  let i = 0;
  for (const c of t) if (c === q[i]) i++;
  return i === q.length ? 3 : null;
}

function Hit({ text, query }: { text: string; query: string }) {
  const at = text.toLowerCase().indexOf(query.toLowerCase());
  if (!query || at < 0) return <>{text}</>;
  return (
    <>
      {text.slice(0, at)}
      <mark>{text.slice(at, at + query.length)}</mark>
      {text.slice(at + query.length)}
    </>
  );
}

/** Where a found message belongs: its group, the agent side, or the Feed. */
function messagePath(m: Message, me: string): string {
  const q = `?m=${m.id}`;
  if (m.to_group) return paths.group(m.to_group) + q;
  if (m.from_kind === Kind.Agent) return paths.agent(m.from_name) + q;
  if (m.to_name && m.to_owner) return paths.agent(m.to_name) + q;
  if (m.to_name && !same(m.to_name, me)) return paths.feed;
  return paths.feed;
}

/**
 * Jump anywhere: agents (by name or status), groups and your notes match as
 * you type; messages come from the server's search of the whole board.
 */
export function Launcher({ onClose }: { onClose: () => void }) {
  const board = useBoard();
  const [query, setQuery] = useState("");
  const [sel, setSel] = useState(0);
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [searching, setSearching] = useState(false);
  const list = useRef<HTMLDivElement>(null);
  useCover();

  useEffect(() => {
    const q = query.trim();
    if (q.length < 2 || !board.has("message-search.v1")) {
      setHits([]);
      return;
    }
    setSearching(true);
    const t = window.setTimeout(() => {
      rpc<{ hits: SearchHit[] }>("search", { query: q, limit: 30 })
        .then((r) => setHits(r.hits))
        .catch(() => setHits([]))
        .finally(() => setSearching(false));
    }, 200);
    return () => window.clearTimeout(t);
  }, [query, board]);

  const results = useMemo<Result[]>(() => {
    const q = query.trim();
    const go = (to: string) => () => {
      onClose();
      navigate(to);
    };
    const out: { r: Result; s: number; section: number }[] = [];
    for (const a of board.agents) {
      const byName = q ? score(a.name, q) : 1;
      const byStatus = q && a.status ? score(a.status, q) : null;
      const s = byName ?? (byStatus !== null && byStatus < 3 ? byStatus + 0.5 : null);
      if (s === null) continue;
      out.push({
        s,
        section: 0,
        r: {
          key: `a:${a.name}`,
          kind: "agent",
          label: (
            <span className="agent">
              <Hit text={a.name} query={q} />
              {a.owner && <span className="faint">@{a.owner}</span>}
            </span>
          ),
          detail: a.status ? <Hit text={a.status} query={byName === null ? q : ""} /> : undefined,
          go: go(paths.agent(a.name)),
        },
      });
    }
    for (const g of board.groups) {
      const s = q ? score(g.name, q) : 1;
      if (s === null) continue;
      out.push({
        s,
        section: 1,
        r: {
          key: `g:${g.name}`,
          kind: "group",
          label: (
            <span className="group">
              @<Hit text={g.name} query={q} />
            </span>
          ),
          detail: g.description ?? undefined,
          go: go(paths.group(g.name)),
        },
      });
    }
    if (q) {
      for (const n of board.notes.values()) {
        if (n.retired || !n.body) continue;
        const line = n.body.split("\n").find((l) => l.toLowerCase().includes(q.toLowerCase()));
        if (!line) continue;
        out.push({
          s: 2,
          section: 2,
          r: {
            key: `n:${n.agent_id}`,
            kind: "note",
            label: <span className="agent">{n.agent}</span>,
            detail: <Hit text={line.trim()} query={q} />,
            go: go(paths.agent(n.agent)),
          },
        });
      }
    }
    out.sort((a, b) => Math.floor(a.s / 3) - Math.floor(b.s / 3) || a.section - b.section || a.s - b.s);
    const found = out.slice(0, 30).map((o) => o.r);
    for (const h of hits) {
      const m = h.message;
      const line = m.body.split("\n").find((l) => l.toLowerCase().includes(q.toLowerCase())) ?? m.body;
      found.push({
        key: `m:${m.id}`,
        kind: ago(m.created_at).replace(" ago", ""),
        label: (
          <span>
            <span className={m.from_kind === Kind.Agent ? "agent" : "human"}>{senderName(m)}</span>
            <span className="faint"> → </span>
            <span className={m.to_group ? "group" : "agent"}>{m.to_group ? `@${m.to_group}` : m.to_name}</span>
          </span>
        ),
        detail: <Hit text={line.trim().slice(0, 200)} query={q} />,
        go: go(messagePath(m, board.me.name)),
      });
    }
    return found;
  }, [query, board, hits, onClose]);

  useEffect(() => setSel(0), [query]);
  useEffect(() => {
    list.current?.querySelector(".sel")?.scrollIntoView({ block: "nearest" });
  }, [sel]);

  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="dialog launcher">
        <input
          autoFocus
          placeholder="Agents, groups, notes, messages…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") onClose();
            else if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "j")) {
              e.preventDefault();
              setSel((s) => Math.min(s + 1, results.length - 1));
            } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "k")) {
              e.preventDefault();
              setSel((s) => Math.max(s - 1, 0));
            } else if (e.key === "Enter") results[sel]?.go();
          }}
        />
        <div className="launcher-results" ref={list}>
          {results.length === 0 && (
            <div className="empty" style={{ padding: 20 }}>
              {searching ? "Searching…" : "No matches."}
            </div>
          )}
          {results.map((r, i) => (
            <div
              key={r.key}
              className={`launcher-item${i === sel ? " sel" : ""}`}
              onMouseEnter={() => setSel(i)}
              onClick={r.go}
            >
              <span className="kind">{r.kind}</span>
              {r.label}
              {r.detail && <span className="text">{r.detail}</span>}
            </div>
          ))}
          {searching && results.length > 0 && <div className="launcher-section">searching messages…</div>}
        </div>
      </div>
    </div>
  );
}
