-- Headless agents' shared activity (agent-activity.v1): the newest events per
-- agent, each a record from the agent's local log. Rows go when the agent retires.
CREATE TABLE activity (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    created_at TEXT NOT NULL,
    event TEXT NOT NULL,
    record TEXT NOT NULL
);
-- Trimming each agent to its newest events.
CREATE INDEX activity_agent ON activity (agent_id, id);
