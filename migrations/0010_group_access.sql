-- Per-member group access. Only writers may broadcast to the group; members
-- without read receive nothing addressed to it, including notices. Members
-- read by default and write only once a human grants it.
ALTER TABLE group_members ADD COLUMN can_read INTEGER NOT NULL DEFAULT 1
    CHECK (can_read IN (0, 1));
ALTER TABLE group_members ADD COLUMN can_write INTEGER NOT NULL DEFAULT 0
    CHECK (can_write IN (0, 1));
