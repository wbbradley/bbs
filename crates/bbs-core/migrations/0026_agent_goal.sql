-- An agent's newest reported goal (validated JSON) and when the server
-- received it. State, not history: retirement clears both, and neither claims
-- the agent is running now.
ALTER TABLE agents ADD COLUMN goal TEXT;
ALTER TABLE agents ADD COLUMN goal_reported_at TEXT;
