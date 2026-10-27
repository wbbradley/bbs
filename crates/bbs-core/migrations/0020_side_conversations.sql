-- Continuing /btw (side-conversations.v1): a follow-up names the question it
-- continues and shares its conversation id, the id of the conversation's
-- first question. `ended` is the stored half of "cannot be continued" (owner
-- end or runner child loss); the rest is derived at read time.
ALTER TABLE side_requests ADD COLUMN conversation INTEGER NOT NULL DEFAULT 0;
UPDATE side_requests SET conversation = id;
ALTER TABLE side_requests ADD COLUMN follows INTEGER;
ALTER TABLE side_requests ADD COLUMN ended INTEGER NOT NULL DEFAULT 0;

ALTER TABLE side_state ADD COLUMN followups INTEGER;
