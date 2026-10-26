import { useMemo, useState } from "react";
import { rpc, useWatch } from "../api";
import type { PageProps } from "../App";
import { Composer } from "../components/Composer";
import { Dialog } from "../components/Dialog";
import { MessageView } from "../components/MessageView";
import { PageHeader } from "../components/PageHeader";
import { Timeline } from "../components/Timeline";
import { navigate, paths, useSearch } from "../router";
import { useBoard } from "../store";
import type { Message } from "../types";
import { same } from "../util";

const BACKLOG = 300;

export function GroupPage({ group, openNav }: { group: string } & PageProps) {
  const board = useBoard();
  const info = board.groups.find((g) => same(g.name, group));
  const [messages, setMessages] = useState<Message[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [admin, setAdmin] = useState(false);
  const focus = useSearch().get("m");
  const feed = useWatch<{ messages: Message[] }>("feed", { backlog: BACKLOG, group }, (snap) => {
    setMessages(snap.messages);
    setLoaded(true);
  });

  const rows = useMemo(
    () =>
      messages.map((m) => ({
        key: `m${m.id}`,
        at: m.created_at,
        node: <MessageView m={m} me={board.me.name} mode={{ kind: "group" }} highlight={focus === String(m.id)} />,
      })),
    [messages, board.me.name, focus],
  );

  return (
    <>
      <PageHeader
        openNav={openNav}
        title={<span className="group">@{info?.name ?? group}</span>}
        sub={info ? info.description ?? `${info.members.length} members` : board.groupsLoaded ? "archived or unknown" : ""}
      >
        {info && (
          <button className={`btn small${admin ? " primary" : ""}`} onClick={() => setAdmin(!admin)}>
            Members & settings
          </button>
        )}
      </PageHeader>
      <div className="split">
        <div className="conversation">
          <Timeline
            rows={rows}
            version={rows}
            scrollTo={focus ? `msg-${focus}` : null}
            empty={<div className="empty">{loaded ? "No broadcasts yet." : "Loading…"}</div>}
          />
          <Composer
            draftKey={`group:${group.toLowerCase()}`}
            placeholder={`Broadcast to @${group}`}
            disabled={!info && board.groupsLoaded}
            onSubmit={async (body) => {
              await rpc("send", { to: `@${group}`, body });
              return true;
            }}
            hint={feed.state === "retrying" || feed.state === "failed" ? <span className="error-text">{feed.error}</span> : ""}
          />
        </div>
        {admin && info && <GroupAdmin group={info.name} onClose={() => setAdmin(false)} />}
      </div>
    </>
  );
}

/** Group administration: each action maps onto one group RPC, as on the CLI. */
function GroupAdmin({ group, onClose }: { group: string; onClose: () => void }) {
  const board = useBoard();
  const info = board.groups.find((g) => same(g.name, group))!;
  const [description, setDescription] = useState(info.description ?? "");
  const [adding, setAdding] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const writers = new Set((info.writers?.names ?? []).map((n) => n.toLowerCase()));
  const candidates = board.agents.filter((a) => !info.members.some((m) => same(m, a.name)));

  const act = async (f: () => Promise<unknown>, done?: string) => {
    setError(null);
    setNote(null);
    try {
      await f();
      board.refreshGroups();
      if (done) setNote(done);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <aside className="drawer">
      <div className="drawer-head">
        <span className="group">@{info.name}</span>
        <span className="spacer" />
        <button className="btn small" onClick={onClose}>
          Close
        </button>
      </div>
      <div className="drawer-body" style={{ display: "block" }}>
        <label className="field">
          <span>Description</span>
          <div style={{ display: "flex", gap: 6 }}>
            <input
              className="input"
              maxLength={200}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="One line"
            />
            <button
              className="btn small"
              disabled={description.trim() === (info.description ?? "")}
              onClick={() =>
                act(
                  () => rpc("group_describe", { group, description: description.trim() || null }),
                  "Description saved.",
                )
              }
            >
              Save
            </button>
          </div>
        </label>

        <div className="nav-heading" style={{ padding: "8px 0 4px" }}>
          <span>Members</span>
          <span className="faint">★ may write</span>
        </div>
        {info.members.length === 0 && <div className="faint">No members.</div>}
        {info.members.map((m) => {
          const writer = writers.has(m.toLowerCase());
          return (
            <div key={m} style={{ display: "flex", alignItems: "center", gap: 6, padding: "3px 0" }}>
              <span className="agent mono" style={{ flex: 1, cursor: "pointer" }} onClick={() => navigate(paths.agent(m))}>
                {m}
              </span>
              {info.writers && (
                <button
                  className={`btn small${writer ? " primary" : ""}`}
                  title={writer ? "Revoke write" : "Grant write"}
                  onClick={() => act(() => rpc("set_group_writer", { group, agent: m, grant: !writer }))}
                >
                  ★
                </button>
              )}
              <button
                className="btn small danger"
                title="Remove from the group"
                onClick={() =>
                  window.confirm(`Remove ${m} from @${group}?`) &&
                  act(() => rpc("group_remove", { group, agent: m }), `Removed ${m}.`)
                }
              >
                Remove
              </button>
            </div>
          );
        })}

        <div className="nav-heading" style={{ padding: "16px 0 4px" }}>
          <span>Add agents</span>
        </div>
        <select
          className="input"
          multiple
          size={Math.min(8, Math.max(3, candidates.length))}
          value={adding}
          onChange={(e) => setAdding([...e.target.selectedOptions].map((o) => o.value))}
        >
          {candidates.map((a) => (
            <option key={a.name} value={a.name}>
              {a.name}
              {a.owner ? `@${a.owner}` : ""}
            </option>
          ))}
        </select>
        <button
          className="btn small"
          style={{ marginTop: 6 }}
          disabled={adding.length === 0}
          onClick={() =>
            act(async () => {
              const r = await rpc<{ added: string[]; already: string[] }>("group_add", { group, agents: adding });
              setAdding([]);
              return r;
            }, "Added.")
          }
        >
          Add {adding.length || ""}
        </button>

        {note && <div className="notice info">{note}</div>}
        {error && <div className="notice error">{error}</div>}

        <div style={{ marginTop: 24 }}>
          <button className="btn danger" onClick={() => setDeleting(true)}>
            Delete group
          </button>
        </div>
      </div>
      {deleting && (
        <Dialog
          title={`Delete @${group}?`}
          onClose={() => setDeleting(false)}
          actions={
            <>
              <button className="btn" onClick={() => setDeleting(false)}>
                Keep it
              </button>
              <button
                className="btn danger"
                onClick={() =>
                  act(async () => {
                    await rpc("group_delete", { group });
                    navigate(paths.groups);
                  })
                }
              >
                Delete
              </button>
            </>
          }
        >
          The group is archived: members lose access and it leaves listings, but its history stays in the feed.
        </Dialog>
      )}
    </aside>
  );
}
