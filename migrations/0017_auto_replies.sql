-- Durable dedup for automatic owner replies (auto-reply.v1): one accepted
-- send per (agent registration, delivery key). The body is compared through
-- the referenced message, so no copy is stored here. Keyed by agent id, so a
-- new registration reusing a retired name never collides with old rows.
CREATE TABLE auto_replies (
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    key        TEXT NOT NULL,
    message_id INTEGER NOT NULL REFERENCES messages (id),
    PRIMARY KEY (agent_id, key)
) WITHOUT ROWID;

-- 1: sent via AutoReply; NULL: ordinary send.
ALTER TABLE messages ADD COLUMN auto_reply INTEGER;
