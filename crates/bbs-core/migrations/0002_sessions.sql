-- Binds a harness session (e.g. a Claude Code session_id) to the agent it registered.
CREATE TABLE sessions (
    harness     TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    agent_id    INTEGER NOT NULL REFERENCES agents (id),
    bound_at    TEXT NOT NULL,
    PRIMARY KEY (harness, session_id)
);
