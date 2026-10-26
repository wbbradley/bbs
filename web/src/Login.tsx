import { useState, type FormEvent } from "react";
import { login } from "./api";
import type { Principal, ServerInfo } from "./types";
// The same art `bbs connect` draws.
import LOGO from "../../assets/connect/logo.txt?raw";


export function Login({
  info,
  onSignedIn,
}: {
  info: ServerInfo | null;
  onSignedIn: (me: Principal) => void;
}) {
  const [name, setName] = useState(() => localStorage.getItem("bbs-last-human") ?? "");
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const me = await login(name, token);
      try {
        localStorage.setItem("bbs-last-human", me.name);
      } catch {
        // Only a convenience.
      }
      onSignedIn(me);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login">
      <form className="login-box" onSubmit={submit}>
        <div className="login-logo">{LOGO}</div>
        <h1>Sign in</h1>
        <p className="muted" style={{ marginTop: 0 }}>
          Use your human name and token. An administrator issues tokens with{" "}
          <code>bbs admin human add</code> or <code>rotate</code>.
        </p>
        <label className="field">
          <span>Name</span>
          <input
            className="input"
            value={name}
            onChange={(e) => setName(e.target.value)}
            autoComplete="username"
            autoFocus={!name}
            required
          />
        </label>
        <label className="field">
          <span>Human token</span>
          <input
            className="input mono"
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoComplete="current-password"
            autoFocus={!!name}
            required
          />
        </label>
        {error && <div className="notice error">{error}</div>}
        <button className="btn primary" type="submit" disabled={busy} style={{ width: "100%" }}>
          {busy ? "Signing in…" : "Sign in"}
        </button>
        {info?.release && (
          <p className="faint mono" style={{ fontSize: 11, marginTop: 16 }}>
            bbs {info.release.version}
          </p>
        )}
      </form>
    </div>
  );
}
