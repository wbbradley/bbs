-- A cancel event names the notice it sent the creator, if any. Rebuild to
-- relax the CHECK that kept message_id to answers; IDs and rows stay intact.
-- An existing cancel's notice is the one direct message the canceller sent the
-- creator at the cancel's time whose body starts `Blocker #<id> `.
CREATE TABLE blocker_events_replacement (
    id          INTEGER PRIMARY KEY,
    blocker_id  INTEGER NOT NULL REFERENCES blockers(id),
    kind        TEXT NOT NULL CHECK(kind IN ('created', 'answered', 'cancelled', 'superseded')),
    actor_kind  TEXT NOT NULL CHECK(actor_kind IN ('agent', 'human')),
    actor_id    INTEGER NOT NULL,
    created_at  TEXT NOT NULL,
    request_key TEXT CHECK(request_key IS NULL OR length(CAST(request_key AS BLOB)) BETWEEN 1 AND 128),
    answer      TEXT CHECK(answer IS NULL OR length(CAST(answer AS BLOB)) BETWEEN 1 AND 8192),
    message_id  INTEGER REFERENCES messages(id),
    selected    TEXT CHECK(selected IS NULL OR kind = 'answered'),
    CHECK ((kind IN ('answered', 'cancelled')) = (request_key IS NOT NULL)),
    CHECK ((kind = 'answered') = (answer IS NOT NULL)),
    CHECK (kind != 'answered' OR message_id IS NOT NULL),
    CHECK (kind IN ('answered', 'cancelled') OR message_id IS NULL)
);
INSERT INTO blocker_events_replacement
    (id, blocker_id, kind, actor_kind, actor_id, created_at, request_key, answer, message_id, selected)
SELECT e.id, e.blocker_id, e.kind, e.actor_kind, e.actor_id, e.created_at, e.request_key, e.answer,
    COALESCE(e.message_id, (SELECT m.id FROM messages m JOIN blockers b ON b.id = e.blocker_id
        WHERE e.kind = 'cancelled' AND m.created_at = e.created_at
            AND m.sender_address_id IN (SELECT na.id FROM entity_local_bindings nb
                JOIN entity_addresses na ON na.entity_id = nb.entity_id AND na.address_id = 'native'
                WHERE (e.actor_kind = 'human' AND nb.human_id = e.actor_id)
                    OR (e.actor_kind = 'agent' AND nb.agent_id = e.actor_id))
            AND m.recipient_address_id = (SELECT na.id FROM entity_local_bindings nb
                JOIN entity_addresses na ON na.entity_id = nb.entity_id AND na.address_id = 'native'
                WHERE nb.agent_id = b.creator_id)
            AND m.to_group_id IS NULL
            AND substr(m.body, 1, length('Blocker #' || b.id || ' ')) = 'Blocker #' || b.id || ' '
        ORDER BY m.id LIMIT 1)),
    e.selected
FROM blocker_events e;
DROP TABLE blocker_events;
ALTER TABLE blocker_events_replacement RENAME TO blocker_events;
CREATE UNIQUE INDEX blocker_terminal ON blocker_events(blocker_id) WHERE kind != 'created';
CREATE UNIQUE INDEX blocker_event_key ON blocker_events(actor_kind, actor_id, request_key);
CREATE UNIQUE INDEX blocker_created ON blocker_events(blocker_id) WHERE kind = 'created';
CREATE INDEX blocker_events_message ON blocker_events(message_id) WHERE message_id IS NOT NULL;
CREATE TRIGGER blocker_events_no_update BEFORE UPDATE ON blocker_events
BEGIN SELECT RAISE(ABORT, 'blocker history is immutable'); END;
CREATE TRIGGER blocker_events_no_delete BEFORE DELETE ON blocker_events
BEGIN SELECT RAISE(ABORT, 'blocker history is immutable'); END;
