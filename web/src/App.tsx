import { useEffect, useState } from "react";
import { ApiError, onSignedOut, rpc, serverInfo } from "./api";
import { Launcher } from "./Launcher";
import { Login } from "./Login";
import { Nav } from "./Nav";
import { AgentsPage } from "./pages/Agents";
import { BlockersPage } from "./pages/Blockers";
import { Conversation } from "./pages/Conversation";
import { FeedPage } from "./pages/Feed";
import { GroupPage } from "./pages/Group";
import { GroupsPage } from "./pages/Groups";
import { SettingsPage } from "./pages/Settings";
import { navigate, paths, usePath } from "./router";
import { BoardProvider } from "./store";
import type { Principal, ServerInfo } from "./types";
import { setDraftOwner } from "./util";

type Session = { state: "loading" } | { state: "out" } | { state: "in"; me: Principal };

export function App() {
  const [session, setSession] = useState<Session>({ state: "loading" });
  const [info, setInfo] = useState<ServerInfo | null>(null);

  // Drafts and kept blocker writes are the signed-in human's alone.
  const signIn = (me: Principal) => {
    setDraftOwner(me.name);
    setSession({ state: "in", me });
  };
  const signOut = () => {
    setDraftOwner(null);
    setSession({ state: "out" });
  };

  useEffect(() => {
    rpc<Principal>("whoami")
      .then(signIn)
      .catch((e) => {
        // Anything but a refused sign-in (a restart, say) still shows sign-in,
        // which says what went wrong when it is tried.
        if (!(e instanceof ApiError) || e.status !== 401) console.warn(e);
        signOut();
      });
    return onSignedOut(signOut);
  }, []);

  // What the server offers is asked until it answers; meanwhile nothing
  // that needs a capability is offered.
  useEffect(() => {
    let stopped = false;
    let timer: number | undefined;
    let delay = 1000;
    const ask = async () => {
      const found = await serverInfo();
      if (stopped) return;
      if (found) {
        setInfo(found);
        return;
      }
      timer = window.setTimeout(ask, delay);
      delay = Math.min(delay * 2, 30000);
    };
    ask();
    return () => {
      stopped = true;
      window.clearTimeout(timer);
    };
  }, []);

  if (session.state === "loading") return null;
  if (session.state === "out") {
    return <Login info={info} onSignedIn={signIn} />;
  }
  return (
    <BoardProvider me={session.me} info={info}>
      <Shell />
    </BoardProvider>
  );
}

function Shell() {
  const path = usePath();
  const [navOpen, setNavOpen] = useState(false);
  const [launcher, setLauncher] = useState(false);

  useEffect(() => setNavOpen(false), [path]);
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setLauncher((open) => !open);
      }
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, []);

  return (
    <div className={`app${navOpen ? " nav-open" : ""}`}>
      <Nav onSearch={() => setLauncher(true)} />
      <main className="main" onClick={() => navOpen && setNavOpen(false)}>
        <Route path={path} openNav={() => setNavOpen(true)} />
      </main>
      {launcher && <Launcher onClose={() => setLauncher(false)} />}
    </div>
  );
}

/** Each page gets a way to open the navigation on narrow screens. */
export interface PageProps {
  openNav: () => void;
}

function Route({ path, openNav }: { path: string } & PageProps) {
  const [, section, name] = path.split("/").map((p) => decodeURIComponent(p));
  switch (section) {
    case "agents":
      return name ? <Conversation key={name} agent={name} openNav={openNav} /> : <AgentsPage openNav={openNav} />;
    case "groups":
      return name ? <GroupPage key={name} group={name} openNav={openNav} /> : <GroupsPage openNav={openNav} />;
    case "blockers":
      return <BlockersPage openNav={openNav} />;
    case "feed":
      return <FeedPage openNav={openNav} />;
    case "settings":
      return <SettingsPage openNav={openNav} />;
    default:
      return <Redirect to={paths.agents} />;
  }
}

function Redirect({ to }: { to: string }) {
  useEffect(() => {
    navigate(to, true);
  }, [to]);
  return null;
}
