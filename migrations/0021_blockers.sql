CREATE TABLE blockers (
    id          INTEGER PRIMARY KEY,
    group_id    INTEGER NOT NULL REFERENCES bbs_groups(id),
    creator_id  INTEGER NOT NULL REFERENCES agents(id),
    body        TEXT NOT NULL CHECK(length(CAST(body AS BLOB)) BETWEEN 1 AND 8192),
    created_at  TEXT NOT NULL,
    request_key TEXT NOT NULL CHECK(length(CAST(request_key AS BLOB)) BETWEEN 1 AND 128),
    replaces    INTEGER UNIQUE REFERENCES blockers(id),
    root_id     INTEGER REFERENCES blockers(id),
    UNIQUE(creator_id, request_key),
    CHECK ((replaces IS NULL) = (root_id IS NULL))
);
CREATE INDEX blockers_group_root ON blockers(group_id, COALESCE(root_id, id));
CREATE INDEX blockers_chain ON blockers(COALESCE(root_id, id), id);

CREATE TABLE blocker_events (
    id          INTEGER PRIMARY KEY,
    blocker_id  INTEGER NOT NULL REFERENCES blockers(id),
    kind        TEXT NOT NULL CHECK(kind IN ('created', 'answered', 'cancelled', 'superseded')),
    actor_kind  TEXT NOT NULL CHECK(actor_kind IN ('agent', 'human')),
    actor_id    INTEGER NOT NULL,
    created_at  TEXT NOT NULL,
    request_key TEXT CHECK(request_key IS NULL OR length(CAST(request_key AS BLOB)) BETWEEN 1 AND 128),
    answer      TEXT CHECK(answer IS NULL OR length(CAST(answer AS BLOB)) BETWEEN 1 AND 8192),
    message_id  INTEGER REFERENCES messages(id),
    CHECK ((kind IN ('answered', 'cancelled')) = (request_key IS NOT NULL)),
    CHECK ((kind = 'answered') = (answer IS NOT NULL)),
    CHECK ((kind = 'answered') = (message_id IS NOT NULL))
);
CREATE UNIQUE INDEX blocker_terminal ON blocker_events(blocker_id) WHERE kind != 'created';
CREATE UNIQUE INDEX blocker_event_key ON blocker_events(actor_kind, actor_id, request_key);
CREATE UNIQUE INDEX blocker_created ON blocker_events(blocker_id) WHERE kind = 'created';

CREATE TRIGGER blockers_no_update BEFORE UPDATE ON blockers
BEGIN SELECT RAISE(ABORT, 'blocker versions are immutable'); END;
CREATE TRIGGER blockers_no_delete BEFORE DELETE ON blockers
BEGIN SELECT RAISE(ABORT, 'blocker history is immutable'); END;
CREATE TRIGGER blocker_events_no_update BEFORE UPDATE ON blocker_events
BEGIN SELECT RAISE(ABORT, 'blocker history is immutable'); END;
CREATE TRIGGER blocker_events_no_delete BEFORE DELETE ON blocker_events
BEGIN SELECT RAISE(ABORT, 'blocker history is immutable'); END;
