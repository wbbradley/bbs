import { memo, type ReactNode } from "react";
import { Markdown } from "../Markdown";
import { NameLink, paths } from "../router";
import { Kind, type Message } from "../types";
import { clock, fullTime, isRemoteName, same } from "../util";

export type MessageMode =
  /** An agent conversation: mail with the viewer is unadorned. */
  | { kind: "agent"; agent: string }
  /** A group thread: the viewer's posts as replies, others as cards. */
  | { kind: "group" }
  /** The board feed: every message as a card. */
  | { kind: "feed" };

/** Agents link to their conversation; humans have no page. */
function Name({ name, kind }: { name: string; kind: number }) {
  // Another installation's entity has no page here.
  if (kind === Kind.Remote) return <span className="remote">{name}</span>;
  if (kind !== Kind.Agent) return <span className="human">{name}</span>;
  return (
    <NameLink to={paths.agent(name)} className="agent">
      {name}
    </NameLink>
  );
}

function Recipient({ m }: { m: Message }) {
  if (m.to_group) {
    return (
      <NameLink to={paths.group(m.to_group)} className="group">
        @{m.to_group}
      </NameLink>
    );
  }
  if (!m.to_name) return null;
  const kind = isRemoteName(m.to_name) ? Kind.Remote : m.to_owner ? Kind.Agent : Kind.Human;
  return <Name name={m.to_name} kind={kind} />;
}

export const MessageView = memo(function MessageView({
  m,
  me,
  mode,
  highlight,
}: {
  m: Message;
  me: string;
  mode: MessageMode;
  highlight?: boolean;
}) {
  const fromMe = m.from_kind === Kind.Human && same(m.from_name, me);
  const hl = highlight ? " highlight" : "";
  const id = `msg-${m.id}`;

  if (mode.kind === "agent") {
    const toAgent = !m.to_group && same(m.to_name, mode.agent);
    const fromAgent = m.from_kind === Kind.Agent && same(m.from_name, mode.agent);
    if (fromMe && toAgent) {
      return (
        <div className={`msg mine${hl}`} id={id} title={fullTime(m.created_at)}>
          <div className="msg-body">
            <Markdown text={m.body} />
          </div>
          {!m.read_at && <span className="unread">unread</span>}
        </div>
      );
    }
    if (fromAgent && !m.to_group && same(m.to_name, me)) {
      return (
        <div className={`msg reply${hl}`} id={id} title={fullTime(m.created_at)}>
          <div className="msg-body">
            <Markdown text={m.body} />
          </div>
        </div>
      );
    }
  }

  if (mode.kind === "group" && fromMe) {
    return (
      <div className={`msg mine${hl}`} id={id} title={fullTime(m.created_at)}>
        <div className="msg-body">
          <Markdown text={m.body} />
        </div>
      </div>
    );
  }

  // Cards as connect and bbs view draw them: group mail on the group's
  // colours, other mail on the side-card surface, and on the feed your own
  // on yours. Names are bare: the card says who, not whose.
  const own = mode.kind === "feed" && fromMe;
  const cls = own ? "side own" : m.to_group ? "groupmail" : "side";
  const showTo = mode.kind !== "group";
  // Group mail in a conversation shows no time, as in connect.
  const showTime = !(mode.kind === "agent" && m.to_group);
  // The feed marks direct mail its one reader has not read yet.
  const unread = mode.kind === "feed" && !m.to_group && !m.read_at;
  return (
    <div className={`msg ${cls}${unread ? " unread-mail" : ""}${hl}`} id={id}>
      <div className="msg-head">
        <span className="names">
          <span className="faint">• </span>
          <Name name={m.from_name} kind={m.from_kind} />
          {showTo && (
            <>
              <span className="faint"> → </span>
              <Recipient m={m} />
            </>
          )}
          {m.auto_reply && <span className="faint"> (auto)</span>}
        </span>
        {showTime && (
          <span className="when" title={fullTime(m.created_at)}>
            {clock(m.created_at)}
          </span>
        )}
      </div>
      <div className="msg-body">
        <Markdown text={m.body} />
      </div>
      {unread && <span className="unread">unread</span>}
    </div>
  );
});

/** Rows of a message thread, with a mark where messages may be missing. */
export function messageRows(
  held: { rows: Message[]; gaps: Set<number> },
  me: string,
  mode: MessageMode,
  focus: number | null,
): { key: string; at: string; node: ReactNode }[] {
  const out: { key: string; at: string; node: ReactNode }[] = [];
  for (const m of held.rows) {
    if (held.gaps.has(m.id)) {
      out.push({ key: `g${m.id}`, at: m.created_at, node: <div className="gap">⋯ messages may be missing here</div> });
    }
    out.push({
      key: `m${m.id}`,
      at: m.created_at,
      node: <MessageView m={m} me={me} mode={mode} highlight={focus === m.id} />,
    });
  }
  return out;
}

