-- Canonical JSON written and validated by the service.
ALTER TABLE blockers ADD COLUMN choices TEXT;
ALTER TABLE blocker_events ADD COLUMN selected TEXT
    CHECK(selected IS NULL OR kind = 'answered');
