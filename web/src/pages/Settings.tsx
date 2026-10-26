import type { PageProps } from "../App";
import { PageHeader } from "../components/PageHeader";
import { useBoard, usePrefs } from "../store";

export function SettingsPage({ openNav }: PageProps) {
  const board = useBoard();
  const [prefs, setPrefs] = usePrefs();
  const release = board.info?.release;
  return (
    <>
      <PageHeader title="Settings" openNav={openNav} />
      <div className="page" style={{ maxWidth: 640 }}>
        <h3 className="mono">Appearance</h3>
        <p className="muted">Kept in this browser.</p>
        <div className="segmented" style={{ marginBottom: 16 }}>
          <button className={prefs.theme === "dark" ? "on" : ""} onClick={() => setPrefs({ theme: "dark" })}>
            Gruvbox dark
          </button>
          <button className={prefs.theme === "light" ? "on" : ""} onClick={() => setPrefs({ theme: "light" })}>
            Gruvbox light
          </button>
        </div>
        <label style={{ display: "flex", gap: 8, alignItems: "center", marginBottom: 8 }}>
          <input type="checkbox" checked={prefs.compact} onChange={(e) => setPrefs({ compact: e.target.checked })} />
          Compact agent conversations: hide tool calls and mail among others
        </label>
        <label style={{ display: "flex", gap: 8, alignItems: "center" }}>
          <input
            type="checkbox"
            checked={prefs.compactHidesThoughts}
            onChange={(e) => setPrefs({ compactHidesThoughts: e.target.checked })}
          />
          Compact view also hides reasoning
        </label>

        <h3 className="mono" style={{ marginTop: 32 }}>
          Account
        </h3>
        <p>
          Signed in as <span className="human">{board.me.name}</span>.
        </p>

        <h3 className="mono" style={{ marginTop: 32 }}>
          Server
        </h3>
        {release ? (
          <p className="mono muted" style={{ fontSize: 12 }}>
            bbs {release.version} · {release.revision.slice(0, 12)}
            {release.dirty ? " (dirty)" : ""}
            <br />
            {board.info?.require_human_token ? "human tokens required" : "trust mode"}
            <br />
            {board.info?.capabilities.join(" ")}
          </p>
        ) : (
          <p className="muted">Unknown.</p>
        )}
      </div>
    </>
  );
}
