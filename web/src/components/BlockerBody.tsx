import { Markdown } from "../Markdown";
import { NameLink, paths } from "../router";
import { Kind, type Blocker } from "../types";
import { ago, fullTime } from "../util";

export function BlockerCreator({ b }: { b: Blocker }) {
  if (!b.creator) return <span className="faint">unknown</span>;
  return <span className={b.creator.kind === Kind.Agent ? "agent" : "human"}>{b.creator.name}</span>;
}

/** The creator, linked to the agent's conversation when an agent asked. */
export function BlockerCreatorLink({ b }: { b: Blocker }) {
  if (b.creator?.kind !== Kind.Agent) return <BlockerCreator b={b} />;
  return (
    <NameLink to={paths.agent(b.creator.name)} className="agent">
      {b.creator.name}
    </NameLink>
  );
}

/** The blocker's group, linked to the group's page. */
export function BlockerGroupLink({ b }: { b: Blocker }) {
  return (
    <NameLink to={paths.group(b.group)} className="group">
      @{b.group}
    </NameLink>
  );
}

export function BlockerBody({ b }: { b: Blocker }) {
  return (
    <>
      <div className="mono muted" style={{ fontSize: 12, marginBottom: 8 }}>
        #{b.id} in <span className="group">@{b.group}</span> from <BlockerCreatorLink b={b} />, {ago(b.created_at)} ·{" "}
        <span title={fullTime(b.created_at)}>{new Date(b.created_at).toLocaleString()}</span>
        {b.replaces && <span> · replaces #{b.replaces}</span>}
      </div>
      <Markdown text={b.body} />
    </>
  );
}
