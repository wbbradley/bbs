-- Side channels (agent-interactions.v1): structured owner input requests and
-- per-agent side state. Never mail: nothing here touches messages,
-- deliveries, inboxes, history or conversation recency.
CREATE TABLE side_state (
    agent_id    INTEGER PRIMARY KEY REFERENCES agents (id),
    generation  TEXT NOT NULL,
    usage       TEXT,
    reported_at TEXT NOT NULL,
    revision    INTEGER NOT NULL,
    closed_seq  INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE owner_input_requests (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    generation TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    form       TEXT NOT NULL,
    answers    TEXT,
    status     TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX owner_input_agent ON owner_input_requests (agent_id, id);
