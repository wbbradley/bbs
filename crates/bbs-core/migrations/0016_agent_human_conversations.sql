-- The newest direct message between each agent and human pair, in either
-- direction, for ordering agents by how recently a human talked with them.
-- Sparse: only pairs that have talked have a row. Keyed by ids, so a new agent
-- that reuses a retired one's name starts with none. Controls, timer messages
-- and broadcasts are not conversation and never count.
CREATE TABLE agent_human_conversations (
    agent_id  INTEGER NOT NULL REFERENCES agents (id),
    human_id  INTEGER NOT NULL REFERENCES humans (id),
    last_at   TEXT NOT NULL,
    PRIMARY KEY (agent_id, human_id)
) WITHOUT ROWID;

INSERT INTO agent_human_conversations (agent_id, human_id, last_at)
SELECT agent_id, human_id, max(created_at) FROM (
    SELECT d.recipient_id AS agent_id, m.from_id AS human_id, m.created_at
    FROM deliveries d JOIN messages m ON m.id = d.message_id
    WHERE d.recipient_kind = 'agent' AND d.direct = 1 AND m.from_kind = 'human'
      AND m.control IS NULL AND m.timer_id IS NULL
    UNION ALL
    SELECT m.from_id, d.recipient_id, m.created_at
    FROM deliveries d JOIN messages m ON m.id = d.message_id
    WHERE d.recipient_kind = 'human' AND d.direct = 1 AND m.from_kind = 'agent'
      AND m.control IS NULL AND m.timer_id IS NULL
) c
WHERE c.agent_id IN (SELECT id FROM agents) AND c.human_id IN (SELECT id FROM humans)
GROUP BY agent_id, human_id;
