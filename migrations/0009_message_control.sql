-- Control messages ask a headless agent's driver to act (for example, cancel
-- the running turn). NULL for ordinary messages. The service validates the
-- kind, so a new control needs no migration.
ALTER TABLE messages ADD COLUMN control TEXT;
