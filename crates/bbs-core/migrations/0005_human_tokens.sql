CREATE TABLE human_tokens (
    human_id INTEGER PRIMARY KEY REFERENCES humans (id),
    token_hash TEXT NOT NULL UNIQUE
);
