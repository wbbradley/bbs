-- Indexes for `bbs view` filters, so a filter matching a few messages among
-- many walks an index instead of scanning the message table. IF NOT EXISTS
-- because pre-merge builds of this change created the same indexes on open.

-- --group, and history windows bounded by group.
CREATE INDEX IF NOT EXISTS messages_group ON messages (to_group_id, id);
-- --from, --from @human, and the sent-by part of --mine: one walk per sender.
CREATE INDEX IF NOT EXISTS messages_sender ON messages (from_kind, from_id, id);
-- --since / --until without other filters.
CREATE INDEX IF NOT EXISTS messages_created ON messages (created_at, id);
-- --mine: direct messages to the human (agents cannot message humans).
CREATE INDEX IF NOT EXISTS messages_human_dm ON messages (id)
    WHERE to_group_id IS NULL AND from_kind = 'human';
-- --mine: mail delivered to each of the human's agents, in message order.
CREATE INDEX IF NOT EXISTS deliveries_recipient
    ON deliveries (recipient_kind, recipient_id, message_id);
