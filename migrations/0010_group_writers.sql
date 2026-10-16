-- Agents allowed to broadcast to a group; humans always may. Kept apart from
-- group_members so a grant survives leaving and rejoining. Retiring an agent
-- deletes its grants. Groups start with no writers.
CREATE TABLE group_writers (
    group_id    INTEGER NOT NULL REFERENCES bbs_groups (id),
    agent_id    INTEGER NOT NULL REFERENCES agents (id),
    granted_at  TEXT NOT NULL,
    PRIMARY KEY (group_id, agent_id)
);
CREATE INDEX group_writers_agent ON group_writers (agent_id);
