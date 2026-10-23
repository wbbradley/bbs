-- Private agent notes (agent-notes.v1): one per human and stable agent id.
-- Rows are never deleted; an empty body is a cleared note, kept so
-- incremental sync sees it. `revision` is a per-human change number.
CREATE TABLE agent_notes (
    human_id   INTEGER NOT NULL REFERENCES humans(id),
    agent_id   INTEGER NOT NULL REFERENCES agents(id),
    body       TEXT NOT NULL CHECK(length(CAST(body AS BLOB)) <= 32768),
    revision   INTEGER NOT NULL CHECK(revision > 0),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (human_id, agent_id)
) WITHOUT ROWID;
CREATE UNIQUE INDEX agent_notes_revision ON agent_notes(human_id, revision);
