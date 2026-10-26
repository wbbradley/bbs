import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { buildActivity, mergeTimeline, type ActivityEntry } from "../activity";
import { rpc, useWatch } from "../api";
import type { PageProps } from "../App";
import { ActivityView, ToolRun } from "../components/ActivityView";
import { AnswerDialog } from "../components/AnswerDialog";
import { Composer, type ComposerHandle } from "../components/Composer";
import { EffortControl } from "../components/Effort";
import { MessageView } from "../components/MessageView";
import { NoteDrawer, type NoteHandle } from "../components/NoteDrawer";
import { PageHeader } from "../components/PageHeader";
import { SidePanel } from "../components/SidePanel";
import { Timeline } from "../components/Timeline";
import { navigate, paths, useSearch } from "../router";
import { useBoard, usePrefs } from "../store";
import {
  Control,
  ContextSource,
  InputStatus,
  Kind,
  SideStatus,
  type Activity,
  type Message,
  type SideReport,
  type SideRequest,
} from "../types";
import { ago, same } from "../util";

const FEED_BACKLOG = 300;
const ACTIVITY_BACKLOG = 200;
/** Activity kept in the page; older records fall off the top. */
const ACTIVITY_KEEP = 3000;
const SIDE_POLL_MS = 1500;
/** Unfinished activity shows as running only this long after it arrives. */
const RUNNING_FRESH_MS = 60000;

type Show = "all" | "messages" | "activity";

export function Conversation({ agent, openNav }: { agent: string } & PageProps) {
  const board = useBoard();
  const [prefs, setPrefs] = usePrefs();
  const info = board.agent(agent);
  const owned = same(info?.owner, board.me.name);
  const me = board.me.name;
  const search = useSearch();
  const focusMessage = search.get("m");

  const [messages, setMessages] = useState<Message[]>([]);
  const [feedLoaded, setFeedLoaded] = useState(false);
  const [records, setRecords] = useState<Activity[]>([]);
  const [show, setShow] = useState<Show>("all");
  const [following, setFollowing] = useState(true);
  const [noteOpen, setNoteOpen] = useState(false);
  const [answering, setAnswering] = useState(false);
  const [sideOpen, setSideOpen] = useState(false);
  const [sideHistory, setSideHistory] = useState<SideRequest[]>([]);
  const [side, setSide] = useState<SideReport | null>(null);
  const [sideError, setSideError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [visible, setVisible] = useState(document.visibilityState === "visible");
  const [, tick] = useState(0);
  const composer = useRef<ComposerHandle>(null);
  const note = useRef<NoteHandle>(null);
  // Closing the note stores a change first, and keeps it open if it cannot.
  const toggleNote = async () => {
    if (!noteOpen) return setNoteOpen(true);
    if (!note.current || (await note.current.requestClose())) setNoteOpen(false);
  };

  useEffect(() => {
    const onVisibility = () => setVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", onVisibility);
    const t = window.setInterval(() => tick((n) => n + 1), 5000);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      window.clearInterval(t);
    };
  }, []);

  const feed = useWatch<{ messages: Message[] }>(
    "feed",
    { backlog: FEED_BACKLOG, with: agent, include_group_deliveries: true },
    (snap) => {
      setMessages(snap.messages);
      setFeedLoaded(true);
    },
  );
  const activity = useWatch<{ events: Activity[] }>(
    "activity",
    board.has("agent-activity.v1") ? { backlog: ACTIVITY_BACKLOG, agent } : null,
    (snap, fresh) =>
      setRecords((old) => {
        if (fresh) return snap.events;
        const seen = new Set(old.map((a) => a.id));
        const next = old.concat(snap.events.filter((a) => !seen.has(a.id)));
        return next.length > ACTIVITY_KEEP ? next.slice(next.length - ACTIVITY_KEEP) : next;
      }),
  );

  // The owner's side state: questions waiting on them, context and asides.
  const sideSupported = owned && board.has("agent-interactions.v1");
  useEffect(() => {
    if (!sideSupported) return;
    let stopped = false;
    let timer: number | undefined;
    const poll = async () => {
      try {
        const r = await rpc<SideReport>("side_report", { agent });
        if (stopped) return;
        setSide(r);
        setSideError(null);
      } catch (e) {
        if (stopped) return;
        const err = e as { status?: number; message?: string };
        // A refusal stops asking; a transient failure is retried.
        if (err.status && err.status < 500 && err.status !== 429) {
          setSideError(err.message ?? "side state unavailable");
          return;
        }
      }
      timer = window.setTimeout(poll, SIDE_POLL_MS);
    };
    poll();
    return () => {
      stopped = true;
      window.clearTimeout(timer);
    };
  }, [agent, sideSupported]);

  // Remember earlier exchanges of this side conversation for the visit.
  useEffect(() => {
    const s = side?.side;
    if (!s || s.status === SideStatus.Open) return;
    setSideHistory((h) => {
      const conversation = s.conversation ?? s.id;
      const kept = h.filter((x) => (x.conversation ?? x.id) === conversation && x.id !== s.id);
      return [...kept, s];
    });
  }, [side?.side]);

  const entries = useMemo(() => buildActivity(records), [records]);

  // Reading: mail to you is read, and activity seen, once the live view shows
  // it to a reader following the newest, as connect does.
  const reading = visible && following && !noteOpen && !focusMessage;
  const markedRead = useRef(0);
  useEffect(() => {
    if (!reading || !feedLoaded) return;
    const unread = messages.filter(
      (m) => !m.read_at && !m.to_group && same(m.to_name, me) && same(m.from_name, agent),
    );
    const newest = Math.max(0, ...unread.map((m) => m.id));
    if (newest > markedRead.current) {
      markedRead.current = newest;
      rpc("mark_read", { from: agent }).catch(() => (markedRead.current = 0));
    }
  }, [reading, feedLoaded, messages, me, agent]);
  const markedSeen = useRef(0);
  useEffect(() => {
    if (!reading || records.length === 0) return;
    const newest = records[records.length - 1].id;
    if (newest > markedSeen.current) {
      markedSeen.current = newest;
      rpc("mark_seen", { agent, through_id: newest }).catch(() => (markedSeen.current = 0));
    }
  }, [reading, records, agent]);

  const compact = prefs.compact;
  const rows = useMemo(() => {
    const items = mergeTimeline(show === "activity" ? [] : messages, show === "messages" ? [] : entries);
    const out: { key: string; at: string; node: ReactNode }[] = [];
    let tools: Extract<ActivityEntry, { kind: "tool" }>[] = [];
    const flush = () => {
      if (tools.length === 0) return;
      const run = tools;
      out.push({ key: `t${run[0].id}`, at: run[0].at, node: <ToolRun calls={run} /> });
      tools = [];
    };
    for (const item of items) {
      if (item.type === "message") {
        const m = item.message;
        const withMe =
          (same(m.from_name, me) && m.from_kind === Kind.Human) || (!m.to_group && same(m.to_name, me));
        // Compact view keeps your mail with the agent and its replies.
        if (compact && !withMe) continue;
        flush();
        out.push({
          key: item.key,
          at: item.at,
          node: (
            <MessageView m={m} me={me} mode={{ kind: "agent", agent }} highlight={focusMessage === String(m.id)} />
          ),
        });
        continue;
      }
      const e = item.entry;
      if (e.kind === "tool") {
        if (compact) continue;
        tools.push(e);
        continue;
      }
      if (compact && e.kind === "thought" && prefs.compactHidesThoughts) continue;
      flush();
      out.push({ key: item.key, at: item.at, node: <ActivityView entry={e} /> });
    }
    flush();
    return out;
  }, [messages, entries, show, compact, prefs.compactHidesThoughts, me, agent, focusMessage]);

  const turn = turnState(entries);
  const pendingInput =
    side?.pending_input && side.pending_input.status === InputStatus.Pending ? side.pending_input : null;

  const send = useCallback(
    async (text: string) => {
      setNotice(null);
      const btw = text.match(/^\/btw(?:\s+([\s\S]*))?$/);
      if (owned && btw) {
        const question = (btw[1] ?? "").trim();
        setSideOpen(true);
        if (!question) return true;
        const report = side?.side;
        const follows =
          report && report.status !== SideStatus.Open && !report.ended && side?.side_followups ? report.id : null;
        await rpc("ask_side", { agent, question, follows });
        return true;
      }
      if (owned && text.trim() === "/answer") {
        if (pendingInput) setAnswering(true);
        else setNotice(`${agent} has no questions waiting.`);
        return true;
      }
      if (owned && text.trim() === "/context") {
        setNotice(contextLabel(side) ?? "Context not reported.");
        return true;
      }
      // `//btw` sends the literal `/btw` text as mail.
      const body = text.startsWith("//") ? text.slice(1) : text;
      await rpc("send", { to: agent, body });
      return true;
    },
    [agent, owned, side, pendingInput],
  );

  const cancelTurn = async () => {
    try {
      await rpc("send", {
        to: agent,
        body: "Cancel your running turn (bbs web).",
        control: Control.Cancel,
      });
      setNotice(`Asked ${agent} to cancel its running turn.`);
    } catch (e) {
      setNotice(e instanceof Error ? e.message : String(e));
    }
  };

  const retire = async () => {
    if (!window.confirm(`Retire ${agent}? Its token stops working and it leaves its groups.`)) return;
    try {
      await rpc("retire", { agent });
      board.refreshAgents();
      navigate(paths.agents);
    } catch (e) {
      setNotice(e instanceof Error ? e.message : String(e));
    }
  };

  const noteId = info?.id ?? null;
  const context = contextLabel(side);
  const streamTrouble =
    feed.state === "retrying" || feed.state === "failed"
      ? `messages: ${feed.error}`
      : activity.state === "retrying" || activity.state === "failed"
        ? `activity: ${activity.error}`
        : null;

  return (
    <>
      <PageHeader
        openNav={openNav}
        title={<span className="agent">{agent}</span>}
        sub={
          info ? (
            <>
              <span className="human">{info.owner ?? "?"}</span>
              {info.groups.length > 0 && <span className="group"> {info.groups.map((g) => `@${g}`).join(" ")}</span>}
              {info.status && <span> · {info.status}</span>}
            </>
          ) : board.agentsLoaded ? (
            "not live"
          ) : undefined
        }
      >
        {info && <EffortControl agent={agent} report={info.effort} owned={owned} />}
        <div className="segmented" title="What the timeline shows">
          {(["all", "messages", "activity"] as Show[]).map((s) => (
            <button key={s} className={show === s ? "on" : ""} onClick={() => setShow(s)}>
              {s === "all" ? "All" : s === "messages" ? "Mail" : "Activity"}
            </button>
          ))}
        </div>
        <button
          className={`btn small${compact ? " primary" : ""}`}
          onClick={() => setPrefs({ compact: !compact })}
          title="Compact view hides tool calls and others' mail"
        >
          Compact
        </button>
        {noteId !== null && board.has("agent-notes.v1") && (
          <button className={`btn small${noteOpen ? " primary" : ""}`} onClick={toggleNote}>
            Note{board.notes.get(noteId)?.body ? " •" : ""}
          </button>
        )}
        {owned && board.has("side-questions.v1") && (
          <button className="btn small" onClick={() => setSideOpen(true)} title="/btw: ask on the side">
            Aside
          </button>
        )}
        {owned && turn.running && (
          <button className="btn small danger" onClick={cancelTurn}>
            Cancel turn
          </button>
        )}
        {owned && (
          <button className="btn small ghost danger" onClick={retire} title="Retire this agent">
            Retire
          </button>
        )}
      </PageHeader>
      <div className="split">
        <div className="conversation">
          <Timeline
            rows={rows}
            version={rows}
            onFollowing={setFollowing}
            scrollTo={focusMessage ? `msg-${focusMessage}` : null}
            empty={<div className="empty">{feedLoaded ? `No conversation with ${agent} yet.` : "Loading…"}</div>}
          />
          <Composer
            ref={composer}
            draftKey={`agent:${agent.toLowerCase()}`}
            placeholder={`Message ${agent}`}
            disabled={!info && board.agentsLoaded}
            onSubmit={send}
            hint={
              notice ??
              (owned ? "/btw asks on the side · /answer · /context" : info ? `${agent} belongs to ${info.owner}` : "")
            }
            above={
              <>
                {pendingInput && (
                  <div className="attention">
                    <span>
                      <span className="agent">{agent}</span>{" "}
                      {pendingInput.is_blocking ? "asks" : "has"}{" "}
                      {pendingInput.questions.length === 1 ? "a question" : `${pendingInput.questions.length} questions`}
                      {pendingInput.is_blocking ? "" : " available"}
                    </span>
                    <span className="spacer" />
                    <button className="btn small primary" onClick={() => setAnswering(true)}>
                      Answer
                    </button>
                  </div>
                )}
                {sideOpen && (
                  <SidePanel agent={agent} report={side} history={sideHistory} onClose={() => setSideOpen(false)} />
                )}
              </>
            }
          />
          <div className="statusbar">
            <span>
              <TurnMark turn={turn} /> {turn.label}
            </span>
            {info && <span>active {ago(info.last_sent_at)}</span>}
            {context && <span>{context}</span>}
            {compact && <span>compact</span>}
            {!following && <span>paused</span>}
            <span className="spacer" />
            {sideError && <span className="warning-text">{sideError}</span>}
            {streamTrouble && <span className="error-text">{streamTrouble}</span>}
            {!board.info && <span>checking what the server offers…</span>}
            {board.info && !board.has("agent-activity.v1") && <span>activity needs a newer server</span>}
          </div>
        </div>
        {noteOpen && noteId !== null && (
          <NoteDrawer ref={note} agentId={noteId} agent={agent} onClose={() => setNoteOpen(false)} />
        )}
      </div>
      {answering && pendingInput && (
        // A new request is a new form: nothing chosen for another carries over.
        <AnswerDialog
          key={pendingInput.request_id}
          agent={agent}
          input={pendingInput}
          onClose={() => setAnswering(false)}
        />
      )}
    </>
  );
}

interface Turn {
  running: boolean;
  label: string;
  mark: "spin" | "done" | "unknown";
}

/** Turn state from the newest activity: running, finished, or unknown. */
function turnState(entries: ActivityEntry[]): Turn {
  for (let i = entries.length - 1; i >= 0; i--) {
    const e = entries[i];
    if (e.kind === "turn_end") return { running: false, label: "idle", mark: "done" };
    if (e.kind === "event" && /^(turn_error|cancel|stopped)/.test(e.text))
      return { running: false, label: "stopped", mark: "unknown" };
    if (["prompt", "steer", "tool", "thought", "text"].includes(e.kind)) {
      const fresh = Date.now() - Date.parse(entries[entries.length - 1].at) < RUNNING_FRESH_MS;
      return fresh
        ? { running: true, label: "working", mark: "spin" }
        : { running: true, label: "quiet", mark: "unknown" };
    }
  }
  return { running: false, label: entries.length ? "idle" : "no activity", mark: "unknown" };
}

function TurnMark({ turn }: { turn: Turn }) {
  const frames = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
  const [frame, setFrame] = useState(0);
  useEffect(() => {
    if (turn.mark !== "spin") return;
    const t = window.setInterval(() => setFrame((f) => (f + 1) % frames.length), 120);
    return () => window.clearInterval(t);
  }, [turn.mark]);
  if (turn.mark === "spin") return <span className="agent">{frames[frame]}</span>;
  if (turn.mark === "done") return <span className="success-text">✓</span>;
  return <span>·</span>;
}

function contextLabel(side: SideReport | null): string | null {
  const usage = side?.usage;
  if (!usage) return null;
  const what = usage.source === ContextSource.LastRequest ? "last request" : "context";
  const k = (n: number) => (n >= 1000 ? `${Math.round(n / 1000)}k` : String(n));
  if (usage.used_tokens === null) return `${what} not reported`;
  if (!usage.limit_tokens) return `${what} ${k(usage.used_tokens)}`;
  const pct = Math.round((usage.used_tokens / usage.limit_tokens) * 100);
  return `${what} ${k(usage.used_tokens)}/${k(usage.limit_tokens)} (${pct}%)`;
}
