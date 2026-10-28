-- Barriers persist until the owning human explicitly resumes delivery.
CREATE TABLE agent_drains (
    human_id INTEGER PRIMARY KEY REFERENCES humans(id),
    watermark INTEGER NOT NULL,
    deadline TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('draining', 'ready', 'timed_out'))
);
CREATE TABLE agent_drain_targets (
    human_id INTEGER NOT NULL REFERENCES agent_drains(human_id),
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    final_message_id INTEGER NOT NULL REFERENCES messages(id),
    completed INTEGER NOT NULL DEFAULT 0,
    side_completed_through INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(human_id, agent_id)
);
CREATE INDEX agent_drain_targets_agent ON agent_drain_targets(agent_id);
