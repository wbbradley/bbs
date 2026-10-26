-- Timestamps are fixed-width RFC3339 UTC text (e.g. 2026-10-08T14:02:11.123456Z),
-- so lexical order is chronological order.

CREATE TABLE humans (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    active_at   TEXT NOT NULL
);
CREATE UNIQUE INDEX humans_name ON humans (lower(name));

CREATE TABLE agents (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    token       TEXT NOT NULL UNIQUE,
    status      TEXT,
    status_at   TEXT,
    pwd         TEXT NOT NULL,
    hostname    TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    active_at   TEXT NOT NULL,
    retired_at  TEXT
);
-- Names are live aliases: unique only among agents that have not retired.
CREATE UNIQUE INDEX agents_live_name ON agents (lower(name)) WHERE retired_at IS NULL;

CREATE TABLE bbs_groups (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL
);

CREATE TABLE group_members (
    group_id    INTEGER NOT NULL REFERENCES bbs_groups (id),
    agent_id    INTEGER NOT NULL REFERENCES agents (id),
    joined_at   TEXT NOT NULL,
    PRIMARY KEY (group_id, agent_id)
);
CREATE INDEX group_members_agent ON group_members (agent_id);

CREATE TABLE messages (
    id           INTEGER PRIMARY KEY,
    from_kind    TEXT NOT NULL CHECK (from_kind IN ('agent', 'human')),
    from_id      INTEGER NOT NULL,
    to_group_id  INTEGER REFERENCES bbs_groups (id),
    body         TEXT NOT NULL,
    created_at   TEXT NOT NULL
);

-- One row per recipient; broadcasts fan out at send time.
CREATE TABLE deliveries (
    message_id      INTEGER NOT NULL REFERENCES messages (id),
    recipient_kind  TEXT NOT NULL CHECK (recipient_kind IN ('agent', 'human')),
    recipient_id    INTEGER NOT NULL,
    archived_at     TEXT,
    PRIMARY KEY (message_id, recipient_kind, recipient_id)
);
CREATE INDEX deliveries_inbox ON deliveries (recipient_kind, recipient_id, archived_at);
