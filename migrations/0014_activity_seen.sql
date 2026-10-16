-- How far each human has seen each agent's shared activity (agent-summary.v1):
-- the newest activity id they have seen. Rows go when the agent retires.
CREATE TABLE activity_seen (
    human_id INTEGER NOT NULL REFERENCES humans(id),
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    seen_id INTEGER NOT NULL,
    PRIMARY KEY (human_id, agent_id)
) WITHOUT ROWID;
