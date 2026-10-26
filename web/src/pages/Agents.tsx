import { useEffect, useState } from "react";
import type { PageProps } from "../App";
import { PageHeader } from "../components/PageHeader";
import { navigate, paths } from "../router";
import { sortAgents, useBoard } from "../store";
import { ago, cap99, same } from "../util";
import { effortLabel } from "../components/Effort";

export function AgentsPage({ openNav }: PageProps) {
  const board = useBoard();
  const [scope, setScope] = useState<"mine" | "all">(() =>
    localStorage.getItem("bbs-agents-scope") === "all" ? "all" : "mine",
  );
  const [filter, setFilter] = useState("");
  const [, tick] = useState(0);
  useEffect(() => {
    const t = window.setInterval(() => tick((n) => n + 1), 30000);
    return () => window.clearInterval(t);
  }, []);
  useEffect(() => {
    try {
      localStorage.setItem("bbs-agents-scope", scope);
    } catch {
      // A convenience only.
    }
  }, [scope]);

  const needle = filter.trim().toLowerCase();
  const agents = sortAgents(
    board.agents.filter(
      (a) =>
        (scope === "all" || same(a.owner, board.me.name)) &&
        (!needle ||
          a.name.toLowerCase().includes(needle) ||
          (a.owner ?? "").toLowerCase().includes(needle) ||
          (a.status ?? "").toLowerCase().includes(needle) ||
          a.groups.some((g) => g.toLowerCase().includes(needle))),
    ),
  );

  return (
    <>
      <PageHeader title="Agents" sub={`${agents.length} live`} openNav={openNav}>
        <div className="segmented">
          <button className={scope === "mine" ? "on" : ""} onClick={() => setScope("mine")}>
            Mine
          </button>
          <button className={scope === "all" ? "on" : ""} onClick={() => setScope("all")}>
            Everyone's
          </button>
        </div>
        <input
          className="input"
          style={{ width: 200 }}
          placeholder="Filter"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
      </PageHeader>
      <div className="page">
        {agents.length === 0 ? (
          <div className="empty">
            {!board.agentsLoaded
              ? "Loading agents…"
              : scope === "mine" && !needle
                ? "You have no live agents. Start one with `bbs headless run NAME`."
                : "No agents match."}
          </div>
        ) : (
          <table className="list">
            <thead>
              <tr>
                <th>Agent</th>
                <th>Owner</th>
                <th title="Open blockers in the agent's groups">Blockers</th>
                <th title="Unread mail to you">✉</th>
                <th className="hide-narrow" title="Unseen activity">•</th>
                <th>Active</th>
                <th className="hide-narrow">Effort</th>
                <th className="hide-narrow">Groups</th>
                <th className="hide-narrow">Status</th>
              </tr>
            </thead>
            <tbody>
              {agents.map((a) => (
                <tr key={a.name} className="row" onClick={() => navigate(paths.agent(a.name))}>
                  <td className="name agent">{a.name}</td>
                  <td className="name human">{a.owner ?? "–"}</td>
                  <td className="num">
                    {a.blocker_count === null ? (
                      "-"
                    ) : a.blocker_count > 0 ? (
                      <span className="badge warn">{a.blocker_count}</span>
                    ) : (
                      ""
                    )}
                  </td>
                  <td className="num">{a.unread > 0 && <span className="badge mail">{cap99(a.unread)}</span>}</td>
                  <td className="num hide-narrow muted">{cap99(a.unseen)}</td>
                  <td className="num muted" title={a.last_sent_at ?? undefined}>
                    {ago(a.last_sent_at)}
                  </td>
                  <td className="hide-narrow mono muted">{effortLabel(a.effort)}</td>
                  <td className="hide-narrow mono group">{a.groups.map((g) => `@${g}`).join(" ")}</td>
                  <td className="hide-narrow">
                    <div className="status" title={`${a.status ?? ""}\n${a.hostname}:${a.pwd}`}>
                      {a.status ?? <span className="faint">{a.hostname}:{a.pwd}</span>}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </>
  );
}
