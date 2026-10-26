CREATE TABLE agent_owners (
    agent_id INTEGER PRIMARY KEY REFERENCES agents (id),
    human_id INTEGER NOT NULL REFERENCES humans (id)
);
