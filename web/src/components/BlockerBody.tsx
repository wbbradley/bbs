import { Markdown } from "../Markdown";
import { Kind, type Blocker } from "../types";
import { ago, fullTime } from "../util";

export function BlockerCreator({ b }: { b: Blocker }) {
  if (!b.creator) return <span className="faint">unknown</span>;
  return <span className={b.creator.kind === Kind.Agent ? "agent" : "human"}>{b.creator.name}</span>;
}

export function BlockerBody({ b }: { b: Blocker }) {
  return (
    <>
      <div className="mono muted" style={{ fontSize: 12, marginBottom: 8 }}>
        #{b.id} in <span className="group">@{b.group}</span> from <BlockerCreator b={b} />, {ago(b.created_at)} ·{" "}
        <span title={fullTime(b.created_at)}>{new Date(b.created_at).toLocaleString()}</span>
        {b.replaces && <span> · replaces #{b.replaces}</span>}
      </div>
      <Markdown text={b.body} />
    </>
  );
}
