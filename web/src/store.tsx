import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { rpc, useWatch, type WatchState } from "./api";
import type {
  Agent,
  AgentNote,
  AgentSummary,
  Group,
  Human,
  ListBlockersResponse,
  Principal,
  ServerInfo,
} from "./types";
import { same, storage } from "./util";

/** An agent as the list gave it, with the summary stream's newer counts. */
export interface LiveAgent extends Agent {
  unread: number;
  unseen: number;
  oldestUnread: string | null;
}

interface Board {
  me: Principal;
  info: ServerInfo | null;
  agents: LiveAgent[];
  humans: Human[];
  agentsLoaded: boolean;
  summary: WatchState;
  groups: Group[];
  groupsLoaded: boolean;
  blockers: ListBlockersResponse | null;
  blockersError: string | null;
  notes: Map<number, AgentNote>;
  /** Every stored note has arrived once; until then none may be written. */
  notesLoaded: boolean;
  notesError: string | null;
  syncNotes: () => void;
  has: (capability: string) => boolean;
  agent: (name: string) => LiveAgent | undefined;
  refreshAgents: () => void;
  refreshGroups: () => void;
  refreshBlockers: () => void;
  setNote: (note: AgentNote) => void;
}

const BoardContext = createContext<Board | null>(null);

export function useBoard(): Board {
  const board = useContext(BoardContext);
  if (!board) throw new Error("useBoard outside BoardProvider");
  return board;
}

const BLOCKER_POLL_MS = 3000;
const AGENT_POLL_MS = 30000;

export function BoardProvider({
  me,
  info,
  children,
}: {
  me: Principal;
  info: ServerInfo | null;
  children: ReactNode;
}) {
  const [listed, setListed] = useState<Agent[]>([]);
  const [humans, setHumans] = useState<Human[]>([]);
  const [agentsLoaded, setAgentsLoaded] = useState(false);
  const [summaries, setSummaries] = useState<Map<string, AgentSummary>>(new Map());
  const [groups, setGroups] = useState<Group[]>([]);
  const [groupsLoaded, setGroupsLoaded] = useState(false);
  const [blockers, setBlockers] = useState<ListBlockersResponse | null>(null);
  const [blockersError, setBlockersError] = useState<string | null>(null);
  const [notes, setNotes] = useState<Map<number, AgentNote>>(new Map());
  const [notesLoaded, setNotesLoaded] = useState(false);
  const [notesError, setNotesError] = useState<string | null>(null);
  const knownNames = useRef("");

  // Until the server says what it offers, nothing that needs a capability is.
  const has = useCallback((c: string) => info?.capabilities.includes(c) ?? false, [info]);

  const refreshAgents = useCallback(() => {
    rpc<{ agents: Agent[]; humans: Human[] }>("list_agents", { all: false })
      .then((r) => {
        setListed(r.agents);
        setHumans(r.humans);
        setAgentsLoaded(true);
      })
      .catch(() => {});
  }, []);

  const refreshGroups = useCallback(() => {
    rpc<{ groups: Group[] }>("group_list")
      .then((r) => {
        setGroups(r.groups.sort((a, b) => a.name.localeCompare(b.name)));
        setGroupsLoaded(true);
      })
      .catch(() => {});
  }, []);

  const blockersBusy = useRef(false);
  const refreshBlockers = useCallback(() => {
    if (blockersBusy.current || !has("blockers-mine.v1")) return;
    blockersBusy.current = true;
    rpc<ListBlockersResponse>("list_blockers", { mine: true, limit: 50 })
      .then((r) => {
        setBlockers(r);
        setBlockersError(null);
      })
      .catch((e) => setBlockersError(String(e.message ?? e)))
      .finally(() => (blockersBusy.current = false));
  }, [has]);

  // Notes arrive in revision order from a cursor; one listing runs at a
  // time, and a newer revision announced meanwhile lists again after it.
  const notesCursor = useRef(0);
  const notesBusy = useRef(false);
  const notesAgain = useRef(false);
  const syncNotes = useCallback(async () => {
    if (notesBusy.current) {
      notesAgain.current = true;
      return;
    }
    notesBusy.current = true;
    try {
      do {
        notesAgain.current = false;
        let cursor = notesCursor.current;
        const found: AgentNote[] = [];
        for (;;) {
          const page = await rpc<{ notes: AgentNote[]; has_more: boolean; next_after_revision: number }>(
            "list_agent_notes",
            { after_revision: cursor, limit: 64 },
          );
          found.push(...page.notes);
          if (page.notes.length > 0) cursor = Math.max(cursor, page.next_after_revision);
          if (!page.has_more) break;
        }
        notesCursor.current = cursor;
        setNotes((old) => mergeNotes(old, found));
        setNotesLoaded(true);
        setNotesError(null);
      } while (notesAgain.current);
    } catch (e) {
      setNotesError(e instanceof Error ? e.message : String(e));
    } finally {
      notesBusy.current = false;
    }
  }, []);

  const setNote = useCallback((note: AgentNote) => {
    setNotes((old) => mergeNotes(old, [note]));
  }, []);

  const notesSupported = has("agent-notes.v1");
  useEffect(() => {
    if (notesSupported) syncNotes();
  }, [notesSupported, syncNotes]);

  const summary = useWatch<{ agents: AgentSummary[]; notes_revision: number | null }>(
    "summary",
    {},
    (snap) => {
      const map = new Map<string, AgentSummary>();
      for (const s of snap.agents) map.set(s.agent.toLowerCase(), s);
      setSummaries(map);
      // A new or retired agent changes the set; list again for its details.
      const names = [...map.keys()].sort().join("\n");
      if (names !== knownNames.current) {
        knownNames.current = names;
        refreshAgents();
      }
      const revision = snap.notes_revision ?? 0;
      if (has("agent-notes.v1") && revision > notesCursor.current) syncNotes();
    },
  );

  useEffect(() => {
    refreshAgents();
    refreshGroups();
    refreshBlockers();
    const agentsTimer = window.setInterval(refreshAgents, AGENT_POLL_MS);
    const groupsTimer = window.setInterval(refreshGroups, AGENT_POLL_MS);
    const blockersTimer = window.setInterval(() => {
      if (document.visibilityState === "visible") refreshBlockers();
    }, BLOCKER_POLL_MS);
    return () => {
      window.clearInterval(agentsTimer);
      window.clearInterval(groupsTimer);
      window.clearInterval(blockersTimer);
    };
  }, [refreshAgents, refreshGroups, refreshBlockers]);

  const agents = useMemo<LiveAgent[]>(
    () =>
      listed.map((a) => {
        const s = summaries.get(a.name.toLowerCase());
        if (!s) return { ...a, unread: 0, unseen: 0, oldestUnread: null };
        return {
          ...a,
          unread: s.unread_messages,
          unseen: s.unseen_stream,
          oldestUnread: s.oldest_unread_message,
          last_sent_at: s.last_sent_at ?? a.last_sent_at,
          effort: s.effort ?? a.effort,
          effort_reported_at: s.effort_reported_at ?? a.effort_reported_at,
          blocker_count: s.blocker_count ?? a.blocker_count,
        };
      }),
    [listed, summaries],
  );

  const agent = useCallback((name: string) => agents.find((a) => same(a.name, name)), [agents]);

  const board: Board = {
    me,
    info,
    agents,
    humans,
    agentsLoaded,
    summary,
    groups,
    groupsLoaded,
    blockers,
    blockersError,
    notes,
    notesLoaded,
    notesError,
    syncNotes,
    has,
    agent,
    refreshAgents,
    refreshGroups,
    refreshBlockers,
    setNote,
  };
  return <BoardContext.Provider value={board}>{children}</BoardContext.Provider>;
}

/** Notes by agent, keeping whichever copy of each is the newer revision. */
export function mergeNotes(old: Map<number, AgentNote>, found: AgentNote[]): Map<number, AgentNote> {
  if (found.length === 0) return old;
  const next = new Map(old);
  for (const n of found) {
    const held = next.get(n.agent_id);
    if (!held || n.revision >= held.revision) next.set(n.agent_id, n);
  }
  return next;
}

/** Agents by open blockers, then most recently active, then name. */
export function sortAgents(agents: LiveAgent[]): LiveAgent[] {
  const time = (a: LiveAgent) => (a.last_sent_at ? Date.parse(a.last_sent_at) : -Infinity);
  return [...agents].sort(
    (a, b) =>
      (b.blocker_count ?? 0) - (a.blocker_count ?? 0) ||
      time(b) - time(a) ||
      a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
  );
}

// ---- Preferences: kept in this browser ----

export interface Prefs {
  theme: "dark" | "light";
  compact: boolean;
  compactHidesThoughts: boolean;
}

const PREFS_KEY = "bbs-prefs";
const defaults: Prefs = { theme: "dark", compact: false, compactHidesThoughts: false };

function loadPrefs(): Prefs {
  try {
    return { ...defaults, ...JSON.parse(storage.get(PREFS_KEY) ?? "{}") };
  } catch {
    return defaults;
  }
}

const PrefsContext = createContext<[Prefs, (p: Partial<Prefs>) => void]>([defaults, () => {}]);

export function PrefsProvider({ children }: { children: ReactNode }) {
  const [prefs, setPrefs] = useState(loadPrefs);
  useEffect(() => {
    document.documentElement.dataset.theme = prefs.theme;
    storage.set(PREFS_KEY, JSON.stringify(prefs));
  }, [prefs]);
  const update = useCallback((p: Partial<Prefs>) => setPrefs((old) => ({ ...old, ...p })), []);
  return <PrefsContext.Provider value={[prefs, update]}>{children}</PrefsContext.Provider>;
}

export function usePrefs() {
  return useContext(PrefsContext);
}
