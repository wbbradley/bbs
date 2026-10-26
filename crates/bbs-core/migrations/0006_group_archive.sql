-- Rebuild to replace all-time name uniqueness with active-name uniqueness.
-- The migration runner disables foreign keys during the transaction and checks
-- all references before committing; group IDs and dependent rows stay intact.
CREATE TABLE bbs_groups_replacement (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    archived_at TEXT
);
INSERT INTO bbs_groups_replacement (id, name, created_at)
    SELECT id, name, created_at FROM bbs_groups;
DROP TABLE bbs_groups;
ALTER TABLE bbs_groups_replacement RENAME TO bbs_groups;
CREATE UNIQUE INDEX bbs_groups_active_name ON bbs_groups (name) WHERE archived_at IS NULL;
