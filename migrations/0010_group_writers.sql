-- Agents allowed to broadcast to a group, keyed by owning human and agent name
-- so a grant outlives any one agent identity (new sessions, leaving and
-- rejoining). Groups start with no writers; humans always may broadcast.
CREATE TABLE group_writers (
    group_id    INTEGER NOT NULL REFERENCES bbs_groups (id),
    human_id    INTEGER NOT NULL REFERENCES humans (id),
    name        TEXT NOT NULL,
    granted_at  TEXT NOT NULL
);
CREATE UNIQUE INDEX group_writers_key ON group_writers (group_id, human_id, lower(name));
