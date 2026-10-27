-- Agents may message humans: the feed finds a human's direct messages through
-- their deliveries, whoever sent them, instead of through human senders.
ALTER TABLE deliveries ADD COLUMN direct INTEGER NOT NULL DEFAULT 0;
UPDATE deliveries SET direct = 1
    WHERE message_id IN (SELECT id FROM messages WHERE to_group_id IS NULL);
CREATE INDEX deliveries_human_dm ON deliveries (recipient_id, message_id)
    WHERE recipient_kind = 'human' AND direct = 1;
DROP INDEX messages_human_dm;
