ALTER TABLE humans ADD COLUMN blockers_enabled INTEGER NOT NULL DEFAULT 0 CHECK (blockers_enabled IN (0, 1));
