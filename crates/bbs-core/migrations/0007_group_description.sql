-- Optional one-line purpose shown in group listings. The service validates
-- content (no control characters, trimmed, non-empty); the CHECK backstops the
-- length limit, counted in characters like the service's limit.
ALTER TABLE bbs_groups ADD COLUMN description TEXT
    CHECK (description IS NULL OR length(description) BETWEEN 1 AND 200);
