-- Rooms (schema 28, DESIGN.md "Rooms"). A room's log lives on its host;
-- this installation keeps only what its own agents need: follows with one
-- shared fetch cursor per room, the posts it fetched, per-follower
-- deliveries, durable room operation IDs, post outcomes and held tags.

-- A room on another installation (its host), as this installation knows it.
CREATE TABLE rooms (
  id INTEGER PRIMARY KEY,
  host TEXT NOT NULL CHECK(length(host) > 0),
  -- The last path segment: the room's entity ID on its host is rooms/<name>.
  name TEXT NOT NULL CHECK(length(name) > 0),
  archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1)),
  -- The shared fetch (D7): the AFTER cursor and the position it stands at.
  -- NULL until the first successful read after a follow.
  fetch_cursor BLOB,
  fetch_position INTEGER,
  -- Local count of the host log epochs seen: bumped by restore recovery, so
  -- a position from before a restore is never compared with one after it.
  epoch INTEGER NOT NULL DEFAULT 0,
  next_fetch_at TEXT,
  -- Bumped by each request to read at once (a local post, a tag, a new
  -- follow), so one that comes during a read is not lost when the read
  -- schedules the next.
  kicks INTEGER NOT NULL DEFAULT 0,
  -- When the newest fetched post was accepted, for the idle slow-down.
  last_post_at TEXT,
  -- Consecutive reads with no answer (RoomUnavailable): the backoff.
  failures INTEGER NOT NULL DEFAULT 0,
  -- Why the host ended the room's follows (not_found, not_a_room, forbidden).
  ended TEXT,
  created_at TEXT NOT NULL,
  UNIQUE(host, name)
);

-- One agent's (or human's) follow of a room, on this installation only.
CREATE TABLE room_follows (
  id INTEGER PRIMARY KEY,
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  follower_address_id INTEGER NOT NULL REFERENCES entity_addresses(id),
  mode TEXT NOT NULL CHECK(mode IN ('quiet', 'wake')),
  -- Where the follow asked to start: the head at the first read, the oldest
  -- post, or after a cursor.
  start_kind TEXT NOT NULL CHECK(start_kind IN ('head', 'oldest', 'cursor')),
  start_cursor BLOB,
  -- Posts after this position (in `epoch`) come through the shared fetch.
  -- NULL while pending.
  start_position INTEGER,
  epoch INTEGER,
  -- A one-off backfill from the follow's start up to backfill_until, the
  -- fetch position when the follow joined it; NULL when none is due.
  backfill_cursor BLOB,
  backfill_until INTEGER,
  backfill_count INTEGER NOT NULL DEFAULT 0,
  last_wake_at TEXT,
  created_at TEXT NOT NULL,
  UNIQUE(room_id, follower_address_id)
);
CREATE INDEX room_follows_follower ON room_follows(follower_address_id);

-- Posts and notices fetched from a room, once each, whatever followers they
-- went to. The message exactly as the host reported it.
CREATE TABLE room_posts (
  id INTEGER PRIMARY KEY,
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  ref_namespace TEXT NOT NULL,
  ref_message_id TEXT NOT NULL,
  position INTEGER NOT NULL,
  epoch INTEGER NOT NULL,
  cursor BLOB NOT NULL,
  accepted_at TEXT NOT NULL,
  authored_at TEXT NOT NULL,
  -- The author as the host reports it; a notice's author is the room.
  author_namespace TEXT NOT NULL,
  author_entity_id TEXT NOT NULL,
  author_address_id TEXT NOT NULL,
  author_run_id TEXT NOT NULL DEFAULT '',
  in_reply_to_namespace TEXT,
  in_reply_to_message_id TEXT,
  content_type TEXT NOT NULL,
  body TEXT NOT NULL,
  notice INTEGER NOT NULL CHECK(notice IN (0, 1)),
  UNIQUE(room_id, ref_namespace, ref_message_id)
);
CREATE INDEX room_posts_position ON room_posts(room_id, epoch, position);

-- A post due to one follower, deduplicated per follower by post. Pending
-- until delivered to the follower's inbox (at most 50 per room at once; the
-- rest are skipped and counted).
CREATE TABLE room_deliveries (
  id INTEGER PRIMARY KEY,
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  follower_address_id INTEGER NOT NULL REFERENCES entity_addresses(id),
  post_id INTEGER NOT NULL REFERENCES room_posts(id),
  source TEXT NOT NULL CHECK(source IN ('follow', 'backfill', 'recovery', 'tag')),
  -- Set when a tag points at this post: the tag message, merged into this one.
  tag_message_id INTEGER REFERENCES messages(id),
  wake INTEGER NOT NULL DEFAULT 0 CHECK(wake IN (0, 1)),
  state TEXT NOT NULL CHECK(state IN ('pending', 'delivered', 'skipped')),
  -- The inbox message once delivered.
  message_id INTEGER REFERENCES messages(id),
  created_at TEXT NOT NULL,
  UNIQUE(follower_address_id, post_id)
);
CREATE INDEX room_deliveries_pending ON room_deliveries(follower_address_id, room_id)
  WHERE state = 'pending';
CREATE INDEX room_deliveries_message ON room_deliveries(message_id) WHERE message_id IS NOT NULL;

-- Room changes (create, update) by requester, with the operation ID saved
-- before the request goes, reused until a signed answer arrives (D2).
CREATE TABLE room_operations (
  id INTEGER PRIMARY KEY,
  requester_address_id INTEGER NOT NULL REFERENCES entity_addresses(id),
  host TEXT NOT NULL,
  -- A digest of the command's contents: a rerun of the same command reuses
  -- the pending operation.
  fingerprint TEXT NOT NULL,
  operation_id TEXT NOT NULL UNIQUE,
  state TEXT NOT NULL CHECK(state IN ('pending', 'answered')),
  created_at TEXT NOT NULL,
  answered_at TEXT
);
CREATE UNIQUE INDEX room_operations_pending
  ON room_operations(requester_address_id, host, fingerprint) WHERE state = 'pending';

-- What became of each room post this installation sent (D9), independent of
-- follows. The handoff's own state is the outbox's; this adds the read-back.
CREATE TABLE room_post_outcomes (
  message_id INTEGER PRIMARY KEY REFERENCES messages(id),
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  -- NULL until read-back decides: confirmed, rejected or unconfirmed.
  readback TEXT CHECK(readback IN ('confirmed', 'rejected', 'unconfirmed')),
  reason TEXT,
  position INTEGER,
  checks INTEGER NOT NULL DEFAULT 0,
  next_check_at TEXT,
  -- Host-answered "absent" time, counted only between answered checks.
  last_absent_at TEXT,
  absent_micros INTEGER NOT NULL DEFAULT 0,
  unconfirmed_at TEXT,
  -- Set once the poster was told of a final outcome with dropped tags.
  settled_at TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX room_post_outcomes_due ON room_post_outcomes(next_check_at)
  WHERE readback IS NULL OR readback = 'unconfirmed';

-- Tags on a room post (D10), held until the post is confirmed.
CREATE TABLE room_tags (
  id INTEGER PRIMARY KEY,
  post_message_id INTEGER NOT NULL REFERENCES messages(id),
  target TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('held', 'sent', 'dropped')),
  tag_message_id INTEGER REFERENCES messages(id),
  detail TEXT,
  UNIQUE(post_message_id, target)
);

-- A tag this installation received: the tag message is held until the post
-- is read back from the room (then merged into its delivery) or cannot be
-- (then delivered as an unverified claim).
CREATE TABLE room_mentions (
  message_id INTEGER PRIMARY KEY REFERENCES messages(id),
  recipient_address_id INTEGER NOT NULL REFERENCES entity_addresses(id),
  room_id INTEGER NOT NULL REFERENCES rooms(id),
  post_namespace TEXT NOT NULL,
  post_message_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('pending', 'merged', 'unverified')),
  checks INTEGER NOT NULL DEFAULT 0,
  next_check_at TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX room_mentions_pending ON room_mentions(next_check_at) WHERE state = 'pending';
