-- Control messages ask a headless agent's driver to act (for example, cancel
-- the running turn). NULL for ordinary messages.
ALTER TABLE messages ADD COLUMN control TEXT
    CHECK (control IS NULL OR control IN ('cancel'));
