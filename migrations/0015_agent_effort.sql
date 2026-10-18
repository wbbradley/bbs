-- An agent's newest reported effort state (validated JSON) and when the
-- server received it. State, not history: retirement clears both, and neither
-- claims the agent is running now.
ALTER TABLE agents ADD COLUMN effort TEXT;
ALTER TABLE agents ADD COLUMN effort_reported_at TEXT;
