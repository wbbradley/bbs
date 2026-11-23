-- Blockers belong to their creating agent, not a group: its owner answers.
-- Rebuild without group_id; IDs, chains and rows stay intact.
CREATE TABLE blockers_replacement (
    id          INTEGER PRIMARY KEY,
    creator_id  INTEGER NOT NULL REFERENCES agents(id),
    body        TEXT NOT NULL CHECK(length(CAST(body AS BLOB)) BETWEEN 1 AND 8192),
    created_at  TEXT NOT NULL,
    request_key TEXT NOT NULL CHECK(length(CAST(request_key AS BLOB)) BETWEEN 1 AND 128),
    replaces    INTEGER UNIQUE REFERENCES blockers(id),
    root_id     INTEGER REFERENCES blockers(id),
    choices     TEXT,
    UNIQUE(creator_id, request_key),
    CHECK ((replaces IS NULL) = (root_id IS NULL))
);
INSERT INTO blockers_replacement
    (id, creator_id, body, created_at, request_key, replaces, root_id, choices)
SELECT id, creator_id, body, created_at, request_key, replaces, root_id, choices
FROM blockers;
DROP TABLE blockers;
ALTER TABLE blockers_replacement RENAME TO blockers;
CREATE INDEX blockers_chain ON blockers(COALESCE(root_id, id), id);
CREATE INDEX blockers_creator_root ON blockers(creator_id, COALESCE(root_id, id));
CREATE TRIGGER blockers_no_update BEFORE UPDATE ON blockers
BEGIN SELECT RAISE(ABORT, 'blocker versions are immutable'); END;
CREATE TRIGGER blockers_no_delete BEFORE DELETE ON blockers
BEGIN SELECT RAISE(ABORT, 'blocker history is immutable'); END;
