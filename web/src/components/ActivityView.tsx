import { memo, useState } from "react";
import type { ActivityEntry } from "../activity";
import { Markdown } from "../Markdown";
import { clock, fullTime } from "../util";

type Tool = Extract<ActivityEntry, { kind: "tool" }>;

/** A run of tool calls, one line each; a click shows a call's whole command. */
export const ToolRun = memo(function ToolRun({ calls }: { calls: Tool[] }) {
  return (
    <div className="act-tools">
      {calls.map((c) => (
        <ToolLine key={c.id} call={c} />
      ))}
    </div>
  );
});

function ToolLine({ call }: { call: Tool }) {
  const [open, setOpen] = useState(false);
  const mark = call.status === "completed" ? "✓" : call.status === "failed" ? "✗" : call.status === "running" ? "…" : "•";
  const cls = ["completed", "failed", "running"].includes(call.status) ? call.status : "";
  return (
    <div className={`tool-line ${cls}${open ? " open" : ""}`} title={fullTime(call.at)}>
      <span className="mark">{mark}</span>
      <span className="name" onClick={() => setOpen(!open)}>
        {open ? call.name : call.name.replace(/\s+/g, " ")}
      </span>
      <span className="state">{call.status}</span>
      {call.took && <span className="took">({call.took})</span>}
    </div>
  );
}

export const ActivityView = memo(function ActivityView({ entry }: { entry: ActivityEntry }) {
  switch (entry.kind) {
    case "thought":
      return (
        <div className="act act-thought">
          <Markdown text={entry.text} />
        </div>
      );
    case "text":
      return (
        <div className="act act-text">
          <Markdown text={entry.text} />
        </div>
      );
    case "prompt":
      return (
        <>
          <div className="turn-start" title={fullTime(entry.at)}>
            turn {clock(entry.at)}
          </div>
          {entry.text.trim() && <div className="act-prompt">{entry.text.trim()}</div>}
        </>
      );
    case "steer":
      return (
        <>
          <div className="act-event warning">{clock(entry.at)} mail steered into the turn:</div>
          {entry.text.trim() && <div className="act-prompt">{entry.text.trim()}</div>}
        </>
      );
    case "mail":
      return (
        <div className="act-event warning">
          {clock(entry.at)} mail from {entry.from.join(", ")}
          {entry.queued ? " (queued)" : ""}
        </div>
      );
    case "turn_end":
      return (
        <div className="turn-end" title={fullTime(entry.at)}>
          turn end{entry.reason ? ` (${entry.reason})` : ""}
        </div>
      );
    case "event":
      return (
        <div className={`act-event ${entry.level}`} style={{ whiteSpace: "pre-wrap" }}>
          {clock(entry.at)} {entry.text}
        </div>
      );
    case "tool":
      return <ToolRun calls={[entry]} />;
  }
});
