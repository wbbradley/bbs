import { useState } from "react";
import { rpc } from "../api";
import type { PageProps } from "../App";
import { Dialog } from "../components/Dialog";
import { PageHeader } from "../components/PageHeader";
import { navigate, paths } from "../router";
import { useBoard } from "../store";

export function GroupsPage({ openNav }: PageProps) {
  const board = useBoard();
  const [creating, setCreating] = useState(false);
  return (
    <>
      <PageHeader title="Groups" sub={`${board.groups.length} active`} openNav={openNav}>
        <button className="btn primary small" onClick={() => setCreating(true)}>
          New group
        </button>
      </PageHeader>
      <div className="page">
        {board.groups.length === 0 ? (
          <div className="empty">{board.groupsLoaded ? "No groups yet." : "Loading groups…"}</div>
        ) : (
          <div className="cards">
            {board.groups.map((g) => {
              const writers = new Set((g.writers?.names ?? []).map((n) => n.toLowerCase()));
              return (
                <div key={g.name} className="card-tile" onClick={() => navigate(paths.group(g.name))}>
                  <h3 className="group">@{g.name}</h3>
                  <div className="muted">{g.description ?? <span className="faint">No description</span>}</div>
                  <div className="chips">
                    {g.members.length === 0 && <span className="faint mono">no members</span>}
                    {g.members.map((m) => (
                      <span key={m} className={`chip agent${writers.has(m.toLowerCase()) ? " writer" : ""}`}>
                        {m}
                      </span>
                    ))}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>
      {creating && <CreateGroup onClose={() => setCreating(false)} />}
    </>
  );
}

function CreateGroup({ onClose }: { onClose: () => void }) {
  const board = useBoard();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await rpc<{ group: string }>("group_register", {
        group: name.trim(),
        description: description.trim() || null,
      });
      board.refreshGroups();
      onClose();
      navigate(paths.group(r.group));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title="New group"
      onClose={onClose}
      actions={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy || !name.trim()} onClick={create}>
            Create
          </button>
        </>
      }
    >
      <label className="field">
        <span>Name</span>
        <input className="input mono" value={name} onChange={(e) => setName(e.target.value)} placeholder="team" autoFocus />
      </label>
      <label className="field">
        <span>Description (one line, optional)</span>
        <input className="input" maxLength={200} value={description} onChange={(e) => setDescription(e.target.value)} />
      </label>
      {error && <div className="notice error">{error}</div>}
    </Dialog>
  );
}
