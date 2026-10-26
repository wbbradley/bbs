-- /btw isolated side questions (side-questions.v1): one open request per
-- agent, claimed once by the current runner generation, answered without
-- touching mail, history or conversation recency.
CREATE TABLE side_requests (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    generation TEXT,
    question   TEXT NOT NULL,
    asked_at   TEXT NOT NULL,
    status     TEXT NOT NULL,
    answer     TEXT,
    updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX side_requests_one_open ON side_requests (agent_id) WHERE status = 'open';

ALTER TABLE side_state ADD COLUMN supported INTEGER;
ALTER TABLE side_state ADD COLUMN reason TEXT;
