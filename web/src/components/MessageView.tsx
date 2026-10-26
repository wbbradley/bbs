import { memo } from "react";
import { Markdown } from "../Markdown";
import { Kind, type Message } from "../types";
import { clock, fullTime, same } from "../util";

export type MessageMode =
  /** An agent conversation: mail with the viewer is unadorned. */
  | { kind: "agent"; agent: string }
  /** A group thread: the viewer's posts as replies, others as cards. */
  | { kind: "group" }
  /** The board feed: every message as a card. */
  | { kind: "feed" };

function Name({ name, kind, owner }: { name: string; kind: number; owner?: string | null }) {
  if (kind === Kind.Agent)
    return (
      <>
        <span className="agent">{name}</span>
        {owner && <span className="faint">@{owner}</span>}
      </>
    );
  return <span className="human">{name}</span>;
}

function Recipient({ m, owners }: { m: Message; owners: boolean }) {
  if (m.to_group) return <span className="group">@{m.to_group}</span>;
  if (!m.to_name) return null;
  return <Name name={m.to_name} kind={m.to_owner ? Kind.Agent : Kind.Human} owner={owners ? m.to_owner : null} />;
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

  const cls = mode.kind === "agent" ? (m.to_group ? "groupmail" : "side") : "card";
  const showTo = mode.kind !== "group";
  // Conversation side cards name participants without owner suffixes.
  const owners = mode.kind !== "agent";
  // Group mail in a conversation shows no time, as in connect.
  const showTime = !(mode.kind === "agent" && m.to_group);
  return (
    <div className={`msg ${cls}${hl}`} id={id}>
      <div className="msg-head">
        <span className="names">
          <span className="faint">• </span>
          <Name name={m.from_name} kind={m.from_kind} owner={owners ? m.from_owner : null} />
          {showTo && (
            <>
              <span className="faint"> → </span>
              <Recipient m={m} owners={owners} />
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
    </div>
  );
});

