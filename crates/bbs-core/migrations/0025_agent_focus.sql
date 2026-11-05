-- When the owning human put the agent in focus mode; NULL when not focused.
-- While focused, only the owner's direct mail and blocker answers reach the
-- agent, and the agent can message only its owner. State, not history: retirement clears it.
ALTER TABLE agents ADD COLUMN focused_at TEXT;
-- Lets a focused agent's inbox find blocker answers, which focus never holds.
CREATE INDEX blocker_events_message ON blocker_events(message_id) WHERE message_id IS NOT NULL;
