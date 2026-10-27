import { useEffect, useState, type ReactNode } from "react";
import { logout } from "./api";
import { Link, navigate, paths, usePath } from "./router";
import { sortAgents, useBoard, type LiveAgent } from "./store";
import { ago, cap99, same } from "./util";

/** How often the agents' ages are drawn again; nothing is asked of the server. */
const AGE_TICK_MS = 30000;

/**
 * The left navigation replaces `bbs connect`'s main menu: the board's pages,
 * then the human's own agents and the groups as direct links, each with the
 * counts that ask for attention.
 */
export function Nav({ onSearch }: { onSearch: () => void }) {
  const board = useBoard();
  const path = usePath();
  const [showOthers, setShowOthers] = useState(false);
  const [, tick] = useState(0);
  useEffect(() => {
    const t = window.setInterval(() => tick((n) => n + 1), AGE_TICK_MS);
    return () => window.clearInterval(t);
  }, []);
  // An agent's flag is the blockers it opened itself, by its stable id, never
  // those of the groups it shares.
  const created = (a: LiveAgent) => (a.id === null ? 0 : (board.createdBlockers?.get(a.id) ?? 0));
  const mine = board.agents
    .filter((a) => same(a.owner, board.me.name))
    .sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }));
  const others = sortAgents(
    board.agents.filter((a) => !same(a.owner, board.me.name)),
    created,
  );
  const blockers = board.blockers?.active_count ?? 0;
  const unread = board.agents.reduce((n, a) => n + a.unread, 0);
  const oldestUnread = board.agents
    .filter((a) => a.unread > 0)
    .sort(
      (a, b) =>
        (a.oldestUnread ?? "").localeCompare(b.oldestUnread ?? "") ||
        a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
    )[0];

  const item = (to: string, glyph: string, label: string, badge?: ReactNode) => (
    <Link to={to} className={`nav-item${path === to ? " active" : ""}`}>
      <span className="nav-glyph">{glyph}</span>
      <span className="label">{label}</span>
      {badge}
    </Link>
  );

  const live = board.summary.state;
  return (
    <nav className="sidebar">
      <div className="brand">
        <span className="brand-logo">BBS</span>
        <span className="brand-version">{board.info?.release?.version}</span>
      </div>
      <button className="nav-search" onClick={onSearch}>
        <span>Search…</span>
        <kbd>⌘K</kbd>
      </button>
      <div className="nav-scroll">
        <div className="nav-section">
          {item(paths.agents, "◆", "Agents", unread > 0 && <span className="badge mail">✉ {cap99(unread)}</span>)}
          {item(paths.groups, "@", "Groups")}
          {item(
            paths.blockers,
            "⚑",
            "Blockers",
            blockers > 0 && <span className="badge warn">{blockers}</span>,
          )}
          {item(paths.feed, "≡", "Feed")}
          {item(paths.settings, "⚙", "Settings")}
          {oldestUnread && (
            <button
              className="btn small"
              style={{ margin: "8px 8px 0", width: "calc(100% - 16px)" }}
              onClick={() => navigate(paths.agent(oldestUnread.name))}
            >
              Next unread: {oldestUnread.name}
            </button>
          )}
        </div>

        <div className="nav-heading">
          <span>Your agents</span>
        </div>
        <div className="nav-section">
          {mine.length === 0 && <div className="nav-item faint">{board.agentsLoaded ? "none live" : "loading…"}</div>}
          {mine.map((a) => (
            <AgentLink key={a.name} agent={a} blockers={created(a)} active={same(path, paths.agent(a.name))} activity />
          ))}
        </div>

        {others.length > 0 && (
          <>
            <div className="nav-heading">
              <span>Others' agents</span>
              <button onClick={() => setShowOthers(!showOthers)}>{showOthers ? "hide" : `show ${others.length}`}</button>
            </div>
            {showOthers && (
              <div className="nav-section">
                {others.map((a) => (
                  <AgentLink
                    key={a.name}
                    agent={a}
                    blockers={created(a)}
                    active={same(path, paths.agent(a.name))}
                    qualify
                  />
                ))}
              </div>
            )}
          </>
        )}

        <div className="nav-heading">
          <span>Groups</span>
        </div>
        <div className="nav-section">
          {board.groups.length === 0 && (
            <div className="nav-item faint">{board.groupsLoaded ? "none" : "loading…"}</div>
          )}
          {board.groups.map((g) => (
            <Link
              key={g.name}
              to={paths.group(g.name)}
              className={`nav-item${same(path, paths.group(g.name)) ? " active" : ""}`}
            >
              <span className="nav-glyph group">@</span>
              <span className="label">{g.name}</span>
              <span className="badge quiet">{g.members.length}</span>
            </Link>
          ))}
        </div>
      </div>
      <div className="nav-footer">
        <span className="who" title={live === "live" ? "Connected" : "Reconnecting"}>
          <span className={`status-dot${live === "live" ? "" : live === "connecting" ? " waiting" : " down"}`} />
          <span className="human">{board.me.name}</span>
        </span>
        <button
          className="btn small ghost"
          onClick={() => logout().finally(() => location.assign("/"))}
        >
          Sign out
        </button>
      </div>
    </nav>
  );
}

/**
 * An agent in the navigation, with the blockers it opened. Your own agents
 * show how long ago each last sent or posted (blank if never), as connect's
 * Agents list does; others' show their mail and activity counts.
 */
function AgentLink({
  agent,
  blockers,
  active,
  qualify,
  activity,
}: {
  agent: LiveAgent;
  blockers: number;
  active: boolean;
  qualify?: boolean;
  activity?: boolean;
}) {
  return (
    <Link to={paths.agent(agent.name)} className={`nav-item${active ? " active" : ""}`} title={agent.status ?? undefined}>
      <span className="nav-glyph agent">◆</span>
      <span className="label">
        <span className="agent">{agent.name}</span>
        {qualify && agent.owner && <span className="faint">@{agent.owner}</span>}
      </span>
      {blockers > 0 && <span className="badge warn">⚑{blockers}</span>}
      {activity ? (
        agent.last_sent_at && (
          <span className="badge quiet" title={agent.last_sent_at}>
            {ago(agent.last_sent_at)}
          </span>
        )
      ) : (
        <>
          {agent.unread > 0 && <span className="badge mail">✉{cap99(agent.unread)}</span>}
          {agent.unseen > 0 && agent.unread === 0 && <span className="badge quiet">•{cap99(agent.unseen)}</span>}
        </>
      )}
    </Link>
  );
}
