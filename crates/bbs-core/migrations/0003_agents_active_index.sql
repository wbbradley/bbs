-- Unread lookups (read, read --wait, hooks) are already covered by
-- deliveries_inbox (recipient_kind, recipient_id, archived_at).

-- Stale-agent sweep at registration scans live agents by last activity.
CREATE INDEX agents_live_active ON agents (active_at) WHERE retired_at IS NULL;
