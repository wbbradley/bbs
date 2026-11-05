-- When the owning human put the agent in focus mode; NULL when not focused.
-- While focused, only the owner's direct mail reaches the agent and the agent
-- can message only its owner. State, not history: retirement clears it.
ALTER TABLE agents ADD COLUMN focused_at TEXT;
