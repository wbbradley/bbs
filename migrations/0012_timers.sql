-- Timers (timers.v1): messages the server sends to an agent later, once or
-- on a schedule. Rows go when the target agent retires.
CREATE TABLE timers (
    id INTEGER PRIMARY KEY,
    target_id INTEGER NOT NULL REFERENCES agents(id),
    creator_kind TEXT NOT NULL CHECK (creator_kind IN ('agent', 'human')),
    creator_id INTEGER NOT NULL,
    body TEXT NOT NULL,
    -- NULL for a one-time timer; else "every <duration>" or a cron expression.
    repeat TEXT,
    next_run TEXT NOT NULL,
    created_at TEXT NOT NULL
);
-- The scheduler's walk to the next due timer.
CREATE INDEX timers_next_run ON timers (next_run);
CREATE INDEX timers_target ON timers (target_id);
-- The timer that sent a message; NULL for every other message.
ALTER TABLE messages ADD COLUMN timer_id INTEGER;
