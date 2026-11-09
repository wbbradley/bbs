-- Hub model (schema 27). NOT in db::MIGRATIONS: startup runs it only to
-- create a new database, right after 1-26 in the same transaction, with no
-- records. On existing data only db::migrate_to_hub_model runs it, from schema
-- 26, as an operator cutover.
--
-- The runner, inside one BEGIN IMMEDIATE transaction with foreign_keys
-- disabled beforehand, first creates and fills:
--   TEMP hub_migration_context(namespace TEXT NOT NULL), one row;
--   TEMP hub_native_map(kind, native_id, stable_id), one row per human, agent
--   and group, with the entity ID the runner computed from BBS names.
-- It then runs this file, compares copied row counts, runs
-- PRAGMA foreign_key_check and sets user_version 27 before COMMIT.

CREATE TEMP TABLE hub_migration_assert (ok INTEGER NOT NULL CHECK(ok = 1));
INSERT INTO hub_migration_assert
SELECT CASE WHEN count(*) = 1 AND min(length(namespace)) > 0 THEN 1 ELSE 0 END
FROM hub_migration_context;

-- Every native record has exactly one computed entity ID.
INSERT INTO hub_migration_assert SELECT
  (SELECT count(*) FROM hub_native_map) =
    (SELECT count(*) FROM humans) + (SELECT count(*) FROM agents) + (SELECT count(*) FROM bbs_groups)
  AND NOT EXISTS (SELECT 1 FROM humans h WHERE NOT EXISTS
    (SELECT 1 FROM hub_native_map x WHERE x.kind = 'human' AND x.native_id = h.id))
  AND NOT EXISTS (SELECT 1 FROM agents a WHERE NOT EXISTS
    (SELECT 1 FROM hub_native_map x WHERE x.kind = 'agent' AND x.native_id = a.id))
  AND NOT EXISTS (SELECT 1 FROM bbs_groups g WHERE NOT EXISTS
    (SELECT 1 FROM hub_native_map x WHERE x.kind = 'group' AND x.native_id = g.id));

-- Polymorphic legacy references did not have foreign keys. Fail closed rather
-- than inventing identities, losing rows, or choosing a recipient arbitrarily.
-- The runner's preflight reports these first; these are the backstop.
INSERT INTO hub_migration_assert SELECT NOT EXISTS (
  SELECT 1 FROM messages m WHERE
    (m.from_kind = 'human' AND NOT EXISTS (SELECT 1 FROM humans WHERE id = m.from_id)) OR
    (m.from_kind = 'agent' AND NOT EXISTS (SELECT 1 FROM agents WHERE id = m.from_id)) OR
    (m.to_group_id IS NULL AND (SELECT count(*) FROM deliveries d WHERE d.message_id = m.id) != 1)
);
INSERT INTO hub_migration_assert SELECT NOT EXISTS (
  SELECT 1 FROM deliveries d WHERE
    (d.recipient_kind = 'human' AND NOT EXISTS (SELECT 1 FROM humans WHERE id = d.recipient_id)) OR
    (d.recipient_kind = 'agent' AND NOT EXISTS (SELECT 1 FROM agents WHERE id = d.recipient_id))
);

-- The installation's own namespace: every native entity's, and the origin of
-- every native message. One row.
CREATE TABLE bbs_installation (
  id INTEGER PRIMARY KEY CHECK(id = 1),
  namespace TEXT NOT NULL CHECK(length(namespace) > 0)
);
INSERT INTO bbs_installation(id, namespace) SELECT 1, namespace FROM hub_migration_context;

CREATE TABLE entities (
  id INTEGER PRIMARY KEY,
  namespace TEXT NOT NULL CHECK(length(namespace) > 0),
  entity_id TEXT NOT NULL CHECK(length(entity_id) > 0),
  kind TEXT NOT NULL CHECK(length(kind) > 0),
  display_name TEXT NOT NULL,
  lifecycle TEXT NOT NULL CHECK(lifecycle IN ('active', 'retired')),
  created_at TEXT NOT NULL,
  UNIQUE(namespace, entity_id)
);
CREATE INDEX entities_catalog ON entities(namespace, lifecycle, id);

-- Native accounts/memberships stay in their original tables. Foreign entities
-- have NO row here. Each native record maps to exactly one immutable entity.
CREATE TABLE entity_local_bindings (
  entity_id INTEGER PRIMARY KEY REFERENCES entities(id),
  human_id INTEGER UNIQUE REFERENCES humans(id),
  agent_id INTEGER UNIQUE REFERENCES agents(id),
  group_id INTEGER UNIQUE REFERENCES bbs_groups(id),
  CHECK((human_id IS NOT NULL) + (agent_id IS NOT NULL) + (group_id IS NOT NULL) = 1)
);
CREATE TABLE entity_addresses (
  id INTEGER PRIMARY KEY,
  entity_id INTEGER NOT NULL REFERENCES entities(id),
  address_id TEXT NOT NULL CHECK(length(address_id) > 0),
  label TEXT NOT NULL,
  lifecycle TEXT NOT NULL CHECK(lifecycle IN ('active', 'retired')),
  is_default INTEGER NOT NULL CHECK(is_default IN (0, 1)),
  supports_run_selector INTEGER NOT NULL DEFAULT 0 CHECK(supports_run_selector IN (0, 1)),
  UNIQUE(entity_id, address_id)
);
CREATE UNIQUE INDEX entity_addresses_default ON entity_addresses(entity_id)
  WHERE is_default = 1 AND lifecycle = 'active';
CREATE TABLE bbs_integrations (
  id TEXT PRIMARY KEY NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
  -- D090: one publication switch, not a per-entity/address allowlist.
  publish_entities INTEGER NOT NULL DEFAULT 0 CHECK(publish_entities IN (0, 1)),
  -- The last publication snapshot sent: its revision, which only grows, and a
  -- digest of its entities, so a changed catalog gets a new revision and a
  -- repeat keeps the old one.
  publication_revision INTEGER NOT NULL DEFAULT 0,
  publication_digest TEXT
);
-- One credential per integration, stored hashed like human tokens. Revoking
-- deletes the row; the integration and its history stay.
CREATE TABLE integration_tokens (
  integration_id TEXT PRIMARY KEY NOT NULL REFERENCES bbs_integrations(id),
  token_hash TEXT NOT NULL UNIQUE
);
CREATE TABLE address_delivery_bindings (
  address_id INTEGER PRIMARY KEY REFERENCES entity_addresses(id),
  handler TEXT NOT NULL CHECK(handler IN ('inbox', 'fanout', 'integration')),
  integration_id TEXT REFERENCES bbs_integrations(id),
  CHECK((handler = 'integration') = (integration_id IS NOT NULL))
);

-- Entity IDs come from hub_native_map, keyed by native ID, never by joining on
-- a possibly reused name. Archived groups are retired entities.
INSERT INTO entities(namespace, entity_id, kind, display_name, lifecycle, created_at)
SELECT c.namespace, x.stable_id, 'human', h.name, 'active', h.created_at
FROM humans h JOIN hub_native_map x ON x.kind = 'human' AND x.native_id = h.id
CROSS JOIN hub_migration_context c;
INSERT INTO entities(namespace, entity_id, kind, display_name, lifecycle, created_at)
SELECT c.namespace, x.stable_id, 'agent', a.name,
  CASE WHEN a.retired_at IS NULL THEN 'active' ELSE 'retired' END, a.created_at
FROM agents a JOIN hub_native_map x ON x.kind = 'agent' AND x.native_id = a.id
CROSS JOIN hub_migration_context c;
INSERT INTO entities(namespace, entity_id, kind, display_name, lifecycle, created_at)
SELECT c.namespace, x.stable_id, 'group', g.name,
  CASE WHEN g.archived_at IS NULL THEN 'active' ELSE 'retired' END, g.created_at
FROM bbs_groups g JOIN hub_native_map x ON x.kind = 'group' AND x.native_id = g.id
CROSS JOIN hub_migration_context c;
INSERT INTO entity_local_bindings(entity_id, human_id, agent_id, group_id)
SELECT e.id,
  CASE WHEN x.kind = 'human' THEN x.native_id END,
  CASE WHEN x.kind = 'agent' THEN x.native_id END,
  CASE WHEN x.kind = 'group' THEN x.native_id END
FROM hub_native_map x JOIN entities e ON e.entity_id = x.stable_id
  AND e.namespace = (SELECT namespace FROM hub_migration_context);
INSERT INTO entity_addresses(entity_id, address_id, label, lifecycle, is_default)
SELECT id, 'native', 'BBS', lifecycle, 1 FROM entities;
INSERT INTO address_delivery_bindings(address_id, handler)
SELECT a.id, CASE WHEN b.group_id IS NULL THEN 'inbox' ELSE 'fanout' END
FROM entity_addresses a JOIN entity_local_bindings b ON b.entity_id = a.entity_id;

CREATE TEMP VIEW hub_native_addresses AS
SELECT x.kind, x.native_id, a.id AS address_id
FROM hub_native_map x JOIN entities e ON e.entity_id = x.stable_id
  AND e.namespace = (SELECT namespace FROM hub_migration_context)
JOIN entity_addresses a ON a.entity_id = e.id AND a.address_id = 'native';

CREATE TABLE messages_replacement (
  id INTEGER PRIMARY KEY,
  origin_namespace TEXT NOT NULL,
  origin_message_id TEXT NOT NULL,
  sender_address_id INTEGER NOT NULL REFERENCES entity_addresses(id),
  sender_run_id TEXT NOT NULL DEFAULT '',
  recipient_address_id INTEGER NOT NULL REFERENCES entity_addresses(id),
  recipient_run_id TEXT NOT NULL DEFAULT '',
  body TEXT NOT NULL,
  content_type TEXT NOT NULL DEFAULT 'text/plain; charset=utf-8',
  created_at TEXT NOT NULL, -- local observation/order; legacy exact original
  authored_at TEXT NOT NULL,
  expires_at TEXT, -- no retroactive expiration for legacy messages
  in_reply_to_namespace TEXT,
  in_reply_to_message_id TEXT,
  kind TEXT NOT NULL DEFAULT 'application' CHECK(kind IN ('application', 'delivery_error')),
  -- Exact common Message bytes for imports/exports; NOT a signed envelope.
  encoded_message BLOB,
  -- Specialized native metadata, not federation instructions.
  to_group_id INTEGER REFERENCES bbs_groups(id),
  control TEXT,
  timer_id INTEGER,
  auto_reply INTEGER,
  -- An imported group copy (D127): the message sent to the group and who sent
  -- it, at the exact address replies go to. The copy's own sender is the group.
  group_copy_namespace TEXT,
  group_copy_message_id TEXT,
  group_copy_sender_address_id INTEGER REFERENCES entity_addresses(id),
  group_copy_sender_run_id TEXT,
  UNIQUE(origin_namespace, origin_message_id),
  CHECK((in_reply_to_namespace IS NULL) = (in_reply_to_message_id IS NULL))
);
INSERT INTO messages_replacement(
  id, origin_namespace, origin_message_id, sender_address_id, recipient_address_id,
  body, created_at, authored_at, to_group_id, control, timer_id, auto_reply)
SELECT m.id, c.namespace, 'bbs-legacy-' || m.id, s.address_id,
  CASE WHEN m.to_group_id IS NOT NULL THEN g.address_id ELSE r.address_id END,
  m.body, m.created_at, m.created_at, m.to_group_id, m.control, m.timer_id, m.auto_reply
FROM messages m
JOIN hub_native_addresses s ON s.kind = m.from_kind AND s.native_id = m.from_id
LEFT JOIN hub_native_addresses g ON g.kind = 'group' AND g.native_id = m.to_group_id
LEFT JOIN deliveries d ON d.message_id = m.id AND m.to_group_id IS NULL
LEFT JOIN hub_native_addresses r ON r.kind = d.recipient_kind AND r.native_id = d.recipient_id
CROSS JOIN hub_migration_context c;
INSERT INTO hub_migration_assert SELECT (SELECT count(*) FROM messages_replacement) = (SELECT count(*) FROM messages);

CREATE TABLE deliveries_replacement (
  message_id INTEGER NOT NULL REFERENCES messages(id),
  recipient_address_id INTEGER NOT NULL REFERENCES entity_addresses(id),
  recipient_run_id TEXT NOT NULL DEFAULT '',
  archived_at TEXT,
  direct INTEGER NOT NULL DEFAULT 0 CHECK(direct IN (0, 1)),
  PRIMARY KEY(message_id, recipient_address_id, recipient_run_id)
);
INSERT INTO deliveries_replacement(message_id, recipient_address_id, archived_at, direct)
SELECT d.message_id, a.address_id, d.archived_at, d.direct
FROM deliveries d JOIN hub_native_addresses a
  ON a.kind = d.recipient_kind AND a.native_id = d.recipient_id;
INSERT INTO hub_migration_assert SELECT (SELECT count(*) FROM deliveries_replacement) = (SELECT count(*) FROM deliveries);

-- Do not rename OLD messages: SQLite could rewrite dependent foreign keys.
-- auto_replies, blocker_events and agent_drain_targets keep referencing
-- messages(id) by name, and the IDs are unchanged. Dropping the old tables
-- also drops their indexes (0001, 0008, 0013); each has an equivalent below:
-- messages_sender, messages_group, messages_created, deliveries_inbox and
-- deliveries_recipient keep their names on the new columns, and
-- deliveries_direct replaces deliveries_human_dm.
DROP TABLE deliveries;
DROP TABLE messages;
ALTER TABLE messages_replacement RENAME TO messages;
ALTER TABLE deliveries_replacement RENAME TO deliveries;
CREATE INDEX messages_sender ON messages(sender_address_id, id);
CREATE INDEX messages_recipient ON messages(recipient_address_id, id);
CREATE INDEX messages_sender_run ON messages(sender_address_id, sender_run_id, id);
CREATE INDEX messages_recipient_run ON messages(recipient_address_id, recipient_run_id, id);
CREATE INDEX messages_created ON messages(created_at, id);
CREATE INDEX messages_group ON messages(to_group_id, id);
CREATE INDEX messages_reply ON messages(in_reply_to_namespace, in_reply_to_message_id, id)
  WHERE in_reply_to_message_id IS NOT NULL;
CREATE INDEX deliveries_inbox ON deliveries(recipient_address_id, recipient_run_id, archived_at, message_id);
-- Each recipient's delivered mail in message order (the feed's --mine);
-- Hub's candidate calls this deliveries_history.
CREATE INDEX deliveries_recipient ON deliveries(recipient_address_id, recipient_run_id, message_id);
-- Replaces deliveries_human_dm, which only held direct deliveries to humans.
-- The recipient's kind is now on its entity, not the delivery, so this covers
-- direct deliveries to every recipient.
CREATE INDEX deliveries_direct ON deliveries(recipient_address_id, message_id) WHERE direct = 1;

CREATE TABLE integration_outbox (
  id INTEGER PRIMARY KEY,
  integration_id TEXT NOT NULL REFERENCES bbs_integrations(id),
  message_id INTEGER NOT NULL UNIQUE REFERENCES messages(id),
  handoff_id TEXT NOT NULL,
  -- Mark attempted BEFORE touching stream. Startup never replays attempted.
  state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending', 'attempted', 'sent', 'received', 'rejected', 'unknown')),
  detail TEXT,
  UNIQUE(integration_id, handoff_id)
);
CREATE INDEX integration_outbox_pending ON integration_outbox(integration_id, id) WHERE state = 'pending';

DROP VIEW hub_native_addresses;
DROP TABLE hub_native_map;
DROP TABLE hub_migration_assert;
