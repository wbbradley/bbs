//! Ported from Hub's scripts/validate_design.py BBS checks, against the real
//! 26-migration chain.

use std::collections::BTreeMap;

use rusqlite::types::Value;

use super::*;
use crate::db::{migrate, migrate_legacy};

const T: &str = "2026-10-08T14:02:11.123456Z";

/// A schema-26 board: owned, ownerless, retired and reused-name agents, a
/// dotted human and agent name, active and archived groups, direct and group
/// mail (one group message with no deliveries), read and unread deliveries,
/// and the native records that reference messages.
fn fixture() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    fill(&mut conn);
    conn
}

/// The same board in a WAL database file, opened as bbs-server opens it.
fn file_fixture(path: &std::path::Path) -> Connection {
    let mut conn = crate::db::open(path).unwrap();
    fill(&mut conn);
    conn
}

fn fill(conn: &mut Connection) {
    migrate_legacy(conn).unwrap();
    let sql = "
        INSERT INTO humans VALUES (1, 'will', 'T', 'T'), (2, 'ann.lee', 'T', 'T');
        INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at, retired_at) VALUES
            (1, 'scout', 'k1', '/w', 'h', 'T', 'T', 'T'),
            (2, 'scout', 'k2', '/w', 'h', 'T', 'T', NULL),
            (3, 'a.b', 'k3', '/w', 'h', 'T', 'T', NULL),
            (4, 'lonely', 'k4', '/w', 'h', 'T', 'T', NULL),
            (5, 'probe', 'k5', '/w', 'h', 'T', 'T', 'T'),
            (6, 'probe-2', 'k6', '/w', 'h', 'T', 'T', NULL),
            (7, 'Probe', 'k7', '/w', 'h', 'T', 'T', NULL);
        INSERT INTO agent_owners VALUES (1, 1), (2, 1), (3, 2), (5, 1), (6, 1), (7, 1);
        INSERT INTO bbs_groups (id, name, created_at, archived_at) VALUES
            (10, 'team', 'T', NULL), (11, 'old', 'T', 'T');
        INSERT INTO group_members VALUES (10, 2, 'T'), (10, 7, 'T');
        INSERT INTO group_writers VALUES (10, 2, 'T');
        INSERT INTO messages (id, from_kind, from_id, to_group_id, body, created_at, control, timer_id, auto_reply) VALUES
            (7, 'human', 1, NULL, 'hello
scout 📨', 'T', NULL, NULL, NULL),
            (19, 'agent', 1, NULL, 'older scout''s answer', 'T', NULL, NULL, 1),
            (21, 'human', 1, 10, 'team announcement', 'T', NULL, NULL, NULL),
            (24, 'agent', 2, 11, 'empty archived group', 'T', NULL, NULL, NULL),
            (31, 'agent', 2, NULL, 'local control', 'T', 'fixture-control', 123, NULL),
            (40, 'human', 2, NULL, 'unread for a.b', 'T', NULL, NULL, NULL),
            (41, 'agent', 4, NULL, 'from lonely', 'T', NULL, NULL, NULL);
        INSERT INTO deliveries VALUES
            (7, 'agent', 1, NULL, 1), (19, 'human', 1, 'T', 1),
            (21, 'agent', 2, 'T', 0), (21, 'agent', 7, NULL, 0), (21, 'human', 1, NULL, 0),
            (31, 'human', 1, NULL, 1), (40, 'agent', 3, NULL, 1), (41, 'human', 2, 'T', 1);
        INSERT INTO sessions VALUES ('fixture', 'session-2', 2, 'T');
        INSERT INTO human_tokens VALUES (1, 'fixture-hash');
        INSERT INTO auto_replies VALUES (1, 'reply-key', 19);
        INSERT INTO activity VALUES (100, 2, 'T', 'mail', '{\"mail_id\":7,\"mail_ids\":[7,19]}');
        INSERT INTO agent_human_conversations VALUES (2, 1, 'T');
        INSERT INTO agent_notes VALUES (1, 2, 'note', 1, 'T');
        INSERT INTO blockers (id, group_id, creator_id, body, created_at, request_key, choices)
            VALUES (1, 10, 2, 'question', 'T', 'q1', '{\"mode\":\"select_one\"}');
        INSERT INTO blocker_events (id, blocker_id, kind, actor_kind, actor_id, created_at,
            request_key, answer, message_id, selected) VALUES
            (1, 1, 'created', 'agent', 2, 'T', NULL, NULL, NULL, NULL),
            (2, 1, 'answered', 'human', 1, 'T', 'a1', 'answer', 7, '[\"go\"]');
        INSERT INTO agent_drains VALUES (1, 31, 'T', 'draining');
        INSERT INTO agent_drain_targets (human_id, agent_id, final_message_id) VALUES (1, 2, 31);
    "
    .replace("'T'", &format!("'{T}'"));
    conn.execute_batch(&sql).unwrap();
}

fn rows(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let n = stmt.column_count();
    let mut out: Vec<String> = stmt
        .query_map([], |r| {
            (0..n)
                .map(|i| r.get::<_, Value>(i))
                .collect::<rusqlite::Result<Vec<_>>>()
                .map(|v| format!("{v:?}"))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    out.sort();
    out
}

fn tables(conn: &Connection) -> Vec<String> {
    strings(
        conn,
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .unwrap()
}

/// The whole database: version, schema and every row.
fn dump(conn: &Connection) -> Vec<String> {
    let mut out = vec![format!("v{}", schema_version(conn).unwrap())];
    out.extend(rows(conn, "SELECT type, name, sql FROM sqlite_schema"));
    for table in tables(conn) {
        out.push(table.clone());
        out.extend(rows(conn, &format!("SELECT * FROM \"{table}\"")));
    }
    out
}

fn foreign_keys_on(conn: &Connection) -> bool {
    conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))
        .unwrap()
}

fn address(conn: &Connection, kind: &str, id: i64) -> i64 {
    conn.query_row(
        &format!(
            "SELECT a.id FROM entity_addresses a JOIN entity_local_bindings b
             ON b.entity_id = a.entity_id WHERE b.{kind}_id = ?1 AND a.address_id = 'native'"
        ),
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

fn entity(conn: &Connection, kind: &str, id: i64) -> (String, String, String, String) {
    conn.query_row(
        &format!(
            "SELECT e.entity_id, e.kind, e.display_name, e.lifecycle FROM entities e
             JOIN entity_local_bindings b ON b.entity_id = e.id WHERE b.{kind}_id = ?1"
        ),
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

#[test]
fn migration_preserves_history_and_maps_names() {
    let mut conn = fixture();
    let old_messages = rows(
        &conn,
        "SELECT id, body, created_at, to_group_id, control, timer_id, auto_reply FROM messages",
    );
    let old_senders: Vec<(i64, String, i64)> = conn
        .prepare("SELECT id, from_kind, from_id FROM messages")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let old_deliveries: Vec<(i64, String, i64, Option<String>, i64)> = conn
        .prepare("SELECT * FROM deliveries")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let untouched: Vec<(String, Vec<String>)> = tables(&conn)
        .into_iter()
        .filter(|t| t != "messages" && t != "deliveries")
        .map(|t| {
            let r = rows(&conn, &format!("SELECT * FROM \"{t}\""));
            (t, r)
        })
        .collect();

    let report = migrate_to_hub_model(&mut conn, "will").unwrap();

    assert_eq!(schema_version(&conn).unwrap(), HUB_MODEL_SCHEMA_VERSION);
    assert!(foreign_keys_on(&conn));
    assert!(
        !conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .exists([])
            .unwrap()
    );
    assert_eq!(strings(&conn, "PRAGMA integrity_check").unwrap(), ["ok"]);
    assert_eq!(
        (
            report.humans,
            report.agents,
            report.groups,
            report.messages,
            report.deliveries
        ),
        (2, 7, 2, 7, 8)
    );

    // IDs, bodies, timestamps and native metadata.
    assert_eq!(
        old_messages,
        rows(
            &conn,
            "SELECT id, body, created_at, to_group_id, control, timer_id, auto_reply FROM messages",
        )
    );
    for (id, kind, native) in old_senders {
        let sender: i64 = conn
            .query_row(
                "SELECT sender_address_id FROM messages WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sender, address(&conn, &kind, native), "message {id}");
        let (origin_ns, origin_id, authored, created): (String, String, String, String) = conn
            .query_row(
                "SELECT origin_namespace, origin_message_id, authored_at, created_at
                 FROM messages WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (origin_ns, origin_id),
            ("will".into(), format!("bbs-legacy-{id}"))
        );
        assert_eq!(authored, created);
    }
    // Read and unread state, one copied row per original.
    for (id, kind, native, archived, direct) in &old_deliveries {
        let copied: (Option<String>, i64) = conn
            .query_row(
                "SELECT archived_at, direct FROM deliveries
                 WHERE message_id = ?1 AND recipient_address_id = ?2 AND recipient_run_id = ''",
                params![id, address(&conn, kind, *native)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(copied, (archived.clone(), *direct));
    }
    assert_eq!(
        count(&conn, "deliveries").unwrap(),
        old_deliveries.len() as i64
    );
    // Intended recipients: groups for broadcasts, the sole delivery otherwise.
    let recipient = |id: i64| -> i64 {
        conn.query_row(
            "SELECT recipient_address_id FROM messages WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(recipient(21), address(&conn, "group", 10));
    assert_eq!(recipient(24), address(&conn, "group", 11));
    assert_eq!(recipient(7), address(&conn, "agent", 1));
    assert_eq!(recipient(41), address(&conn, "human", 2));
    for (table, before) in untouched {
        assert_eq!(
            before,
            rows(&conn, &format!("SELECT * FROM \"{table}\"")),
            "{table}"
        );
    }

    // The mapping.
    let expected = [
        ("human", 1, "will", "human", "will", "active"),
        ("human", 2, "ann-lee", "human", "ann.lee", "active"),
        ("group", 10, "team", "group", "team", "active"),
        ("group", 11, "old", "group", "old", "retired"),
        (
            "agent",
            1,
            "will/scout-retired-2026-10-08",
            "agent",
            "scout",
            "retired",
        ),
        ("agent", 2, "will/scout", "agent", "scout", "active"),
        ("agent", 3, "ann-lee/a-b", "agent", "a.b", "active"),
        ("agent", 4, "lonely", "agent", "lonely", "active"),
        (
            "agent",
            5,
            "will/probe-retired-2026-10-08",
            "agent",
            "probe",
            "retired",
        ),
        ("agent", 6, "will/probe-2", "agent", "probe-2", "active"),
        ("agent", 7, "will/Probe", "agent", "Probe", "active"),
    ];
    for (column, id, entity_id, kind, display, lifecycle) in expected {
        assert_eq!(
            entity(&conn, column, id),
            (
                entity_id.into(),
                kind.into(),
                display.into(),
                lifecycle.into()
            ),
            "{column} {id}"
        );
    }
    assert_eq!(
        rows(&conn, "SELECT DISTINCT namespace FROM entities"),
        [format!("{:?}", [Value::Text("will".into())])]
    );
    // One default `native` address each, with the matching delivery handler.
    assert_eq!(count(&conn, "entities").unwrap(), 11);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM entity_addresses WHERE address_id = 'native' AND is_default = 1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        11
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT handler, count(*) FROM address_delivery_bindings GROUP BY handler"
        ),
        rows(&conn, "SELECT 'fanout', 2 UNION ALL SELECT 'inbox', 9")
    );
    let renamed: Vec<_> = report
        .renamed
        .iter()
        .map(|m| (m.kind, m.native_id, m.entity_id.as_str()))
        .collect();
    assert_eq!(
        renamed,
        [
            (EntityKind::Human, 2, "ann-lee"),
            (EntityKind::Agent, 1, "will/scout-retired-2026-10-08"),
            (EntityKind::Agent, 3, "ann-lee/a-b"),
            (EntityKind::Agent, 5, "will/probe-retired-2026-10-08"),
        ]
    );

    // No integration or outbox work; publication off by default.
    assert_eq!(count(&conn, "integration_outbox").unwrap(), 0);
    assert_eq!(count(&conn, "bbs_integrations").unwrap(), 0);
    conn.execute("INSERT INTO bbs_integrations(id) VALUES ('hub')", [])
        .unwrap();
    let publish: i64 = conn
        .query_row("SELECT publish_entities FROM bbs_integrations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(publish, 0);
    // Temp scaffolding is gone; blocker history is still immutable.
    assert!(
        strings(&conn, "SELECT name FROM temp.sqlite_schema")
            .unwrap()
            .is_empty()
    );
    assert!(
        conn.execute("DELETE FROM blocker_events WHERE id = 2", [])
            .is_err()
    );

    // Startup serves the migrated database, in its namespace, unchanged; a
    // second cutover refuses it.
    assert_eq!(
        rows(&conn, "SELECT namespace FROM bbs_installation"),
        [format!("{:?}", [Value::Text("will".into())])]
    );
    let before = dump(&conn);
    migrate(&mut conn).unwrap();
    crate::db::migrate_in(&mut conn, Some("will")).unwrap();
    assert!(
        crate::db::migrate_in(&mut conn, Some("work"))
            .unwrap_err()
            .to_string()
            .contains("namespace is \"will\", not \"work\"")
    );
    assert_eq!(dump(&conn), before);
    assert!(
        migrate_to_hub_model(&mut conn, "will")
            .unwrap_err()
            .to_string()
            .contains("needs BBS schema v26")
    );
}

/// Every table, index and trigger, with its SQL.
fn schema(conn: &Connection) -> Vec<String> {
    rows(
        conn,
        "SELECT type, name, tbl_name, sql FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
    )
}

/// Both paths run this binary's migration 27, so this cannot catch a
/// database an earlier build of migration 27 made; the next test does.
#[test]
fn cutover_and_new_database_have_the_same_schema() {
    let mut cut = fixture();
    migrate_to_hub_model(&mut cut, "will").unwrap();
    let mut new = Connection::open_in_memory().unwrap();
    crate::db::migrate_in(&mut new, Some("will")).unwrap();
    assert_eq!(schema_version(&cut).unwrap(), crate::db::SCHEMA_VERSION);
    assert_eq!(schema_version(&new).unwrap(), crate::db::SCHEMA_VERSION);
    assert_eq!(schema(&cut), schema(&new));
    // Including the integration stream's tables and columns.
    for conn in [&cut, &new] {
        for sql in [
            "SELECT integration_id, token_hash FROM integration_tokens",
            "SELECT publication_revision, publication_digest FROM bbs_integrations",
            "SELECT group_copy_namespace, group_copy_message_id,
                    group_copy_sender_address_id, group_copy_sender_run_id FROM messages",
        ] {
            conn.prepare(sql).unwrap();
        }
    }
}

#[test]
fn a_schema_27_database_without_the_integration_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bbs.db");
    let mut conn = crate::db::open(&path).unwrap();
    migrate(&mut conn).unwrap();
    // What an earlier build of migration 27 left: no integration tables or
    // group-copy columns.
    conn.execute_batch(
        "DROP TABLE integration_tokens;
         DROP TABLE integration_namespaces;
         ALTER TABLE messages DROP COLUMN group_copy_sender_run_id;",
    )
    .unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    let mut conn = crate::db::open(&path).unwrap();
    let err = migrate(&mut conn).unwrap_err().to_string();
    assert!(err.contains("lacks the integration schema"), "{err}");
    assert!(err.contains("integration_tokens.token_hash"), "{err}");
    assert!(err.contains("messages.group_copy_sender_run_id"), "{err}");
    drop(conn);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn preflight_writes_nothing_and_matches_migration() {
    let conn = fixture();
    let before = dump(&conn);
    let changes = conn.total_changes();
    let report = hub_model_preflight(&conn, "will").unwrap();
    assert_eq!(dump(&conn), before);
    assert_eq!(conn.total_changes(), changes);
    assert!(!report.is_blocked(), "{:?}", report.issues);
    assert_eq!((report.messages, report.deliveries), (7, 8));
    assert_eq!(report.mappings.len(), 11);
    assert_eq!(report.renamed().len(), 4);
}

/// Each blocking case refuses and leaves the database exactly as it was.
fn assert_blocked(setup: &str, expected: &str) {
    let mut conn = fixture();
    conn.execute_batch(&format!(
        "PRAGMA foreign_keys = OFF; {setup}; PRAGMA foreign_keys = ON;"
    ))
    .unwrap();
    let report = hub_model_preflight(&conn, "will").unwrap();
    assert!(
        report.issues.iter().any(|i| i.contains(expected)),
        "{expected}: {:?}",
        report.issues
    );
    let before = dump(&conn);
    let err = migrate_to_hub_model(&mut conn, "will")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(expected) && err.contains("nothing was changed"),
        "{err}"
    );
    assert_eq!(dump(&conn), before);
    assert_eq!(schema_version(&conn).unwrap(), HUB_MODEL_SOURCE_VERSION);
    assert!(foreign_keys_on(&conn));
}

#[test]
fn top_level_clash_between_human_and_group_blocks() {
    assert_blocked(
        "INSERT INTO humans VALUES (3, 'a.b', 't', 't');
         INSERT INTO bbs_groups (id, name, created_at) VALUES (12, 'a-b', 't')",
        "top-level ID `a-b` (case-insensitive) is claimed by human 3 `a.b`, group 12 `a-b`",
    );
    assert_blocked(
        "INSERT INTO bbs_groups (id, name, created_at) VALUES (12, 'Will', 't')",
        "claimed by human 1 `will`, group 12 `Will`",
    );
}

#[test]
fn top_level_name_reuse_blocks() {
    // Not suffixed: only owned agents' reused names are (blocker 1396).
    assert_blocked(
        "UPDATE agents SET retired_at = 't' WHERE id = 4;
         INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at, retired_at)
             VALUES (8, 'lonely', 'k8', '/', 'h', 't', 't', NULL)",
        "claimed by agent 4 `lonely`, agent 8 `lonely`",
    );
    assert_blocked(
        "INSERT INTO bbs_groups (id, name, created_at) VALUES (12, 'old', 't')",
        "claimed by group 11 `old`, group 12 `old`",
    );
}

#[test]
fn dot_and_dash_names_in_one_owner_scope_block() {
    assert_blocked(
        "INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at, retired_at)
             VALUES (8, 'x.y', 'k8', '/', 'h', 't', 't', 't'), (9, 'x-y', 'k9', '/', 'h', 't', 't', NULL);
         INSERT INTO agent_owners VALUES (8, 1), (9, 1)",
        "different agent names map to `will/x-y`: agent 8 `x.y`, agent 9 `x-y`",
    );
}

#[test]
fn invalid_name_after_mapping_blocks() {
    assert_blocked(
        "INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at)
             VALUES (8, 'bad name', 'k8', '/', 'h', 't', 't');
         INSERT INTO agent_owners VALUES (8, 1)",
        "agent 8 `bad name` maps to `bad name`, which is not a valid Hub ID segment",
    );
    assert_blocked(
        "INSERT INTO humans VALUES (3, '', 't', 't')",
        "human 3 `` maps to ``",
    );
}

#[test]
fn malformed_history_blocks() {
    assert_blocked(
        "UPDATE messages SET from_id = 999 WHERE id = 7",
        "1 messages whose sender does not exist: message 7 from human 999",
    );
    assert_blocked(
        "UPDATE deliveries SET recipient_id = 999 WHERE message_id = 40",
        "deliveries whose recipient does not exist: message 40 to agent 999",
    );
    assert_blocked(
        "DELETE FROM deliveries WHERE message_id = 40",
        "direct messages without exactly one delivery: message 40 has 0 deliveries",
    );
    assert_blocked(
        "INSERT INTO deliveries VALUES (40, 'human', 1, NULL, 1)",
        "message 40 has 2 deliveries",
    );
    assert_blocked(
        "INSERT INTO auto_replies VALUES (3, 'orphan', 999)",
        "invalid foreign-key references: auto_replies row",
    );
}

#[test]
fn failure_after_table_replacement_rolls_back() {
    // Bypass preflight to reach the final foreign-key check after the old
    // tables were dropped and replaced.
    let mut conn = fixture();
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF;
         INSERT INTO auto_replies VALUES (3, 'orphan', 999);",
    )
    .unwrap();
    let before = dump(&conn);
    {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let report = preflight(&tx, "will").unwrap();
        let err = apply(&tx, &report).unwrap_err().to_string();
        assert!(err.contains("foreign-key"), "{err}");
    }
    assert_eq!(dump(&conn), before);
    assert_eq!(
        strings(&conn, "SELECT body FROM messages WHERE id = 7").unwrap(),
        ["hello\nscout 📨"]
    );
}

#[test]
fn wrong_source_version_refuses() {
    let mut conn = fixture();
    conn.pragma_update(None, "user_version", 25).unwrap();
    let before = dump(&conn);
    for err in [
        hub_model_preflight(&conn, "will").unwrap_err(),
        migrate_to_hub_model(&mut conn, "will").unwrap_err(),
    ] {
        assert!(
            err.to_string()
                .contains("needs BBS schema v26; this database is at v25"),
            "{err}"
        );
    }
    assert_eq!(dump(&conn), before);
}

#[test]
fn bad_namespace_refuses() {
    let mut conn = fixture();
    let before = dump(&conn);
    let long = "a".repeat(64);
    for namespace in [
        "", "Will", "1will", "-will", "will_x", "will.x", "wïll", &long,
    ] {
        let err = migrate_to_hub_model(&mut conn, namespace).unwrap_err();
        assert!(
            err.to_string().contains("invalid namespace"),
            "{namespace}: {err}"
        );
        assert!(hub_model_preflight(&conn, namespace).is_err());
    }
    assert_eq!(dump(&conn), before);
    for namespace in ["w", "will", "will-2", &"a".repeat(63)] {
        validate_namespace(namespace).unwrap();
    }
}

#[test]
fn startup_refuses_schema_26_unchanged() {
    let mut conn = fixture();
    let before = dump(&conn);
    let err = migrate(&mut conn).unwrap_err().to_string();
    assert!(
        err.contains("at BBS schema v26, before the Hub model")
            && err.contains("bbs-server migrate-hub-model"),
        "{err}"
    );
    // An older schema is refused too, rather than migrated part way.
    conn.pragma_update(None, "user_version", 20).unwrap();
    let older = dump(&conn);
    let err = migrate(&mut conn).unwrap_err().to_string();
    assert!(err.contains("open it once with bbs 0.24"), "{err}");
    assert_eq!(dump(&conn), older);
    conn.pragma_update(None, "user_version", 26).unwrap();
    assert_eq!(dump(&conn), before);
    assert!(foreign_keys_on(&conn));
}

/// Every message/delivery index before the cutover and its equivalent after.
const INDEX_EQUIVALENTS: &[(&str, &str)] = &[
    (
        "sqlite_autoindex_deliveries_1",
        "sqlite_autoindex_deliveries_1",
    ),
    ("messages_group", "messages_group"),
    ("messages_sender", "messages_sender"),
    ("messages_created", "messages_created"),
    ("deliveries_inbox", "deliveries_inbox"),
    ("deliveries_recipient", "deliveries_recipient"),
    ("deliveries_human_dm", "deliveries_direct"),
];

fn indexes(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT name, tbl_name FROM sqlite_schema
             WHERE type = 'index' AND tbl_name IN ('messages', 'deliveries') ORDER BY name",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn plan_uses(conn: &Connection, index: &str, sql: &str) {
    let plan = rows(conn, &format!("EXPLAIN QUERY PLAN {sql}"));
    assert!(
        plan.iter().any(|row| row.contains(index)),
        "{index}: {plan:?}"
    );
}

#[test]
fn every_message_and_delivery_index_has_an_equivalent() {
    let mut conn = fixture();
    let before = indexes(&conn);
    let mut old: Vec<_> = INDEX_EQUIVALENTS
        .iter()
        .map(|(o, _)| o.to_string())
        .collect();
    old.sort();
    assert_eq!(
        before.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        old,
        "a schema-26 index has no listed equivalent"
    );
    migrate_to_hub_model(&mut conn, "will").unwrap();
    let after = indexes(&conn);
    for (old_name, new_name) in INDEX_EQUIVALENTS {
        let table = &before.iter().find(|(n, _)| n == old_name).unwrap().1;
        assert!(
            after.iter().any(|(n, t)| n == new_name && t == table),
            "{old_name} -> {new_name} on {table}: {after:?}"
        );
    }
    // The queries those indexes serve, on the new columns.
    let a = address(&conn, "human", 1);
    plan_uses(
        &conn,
        "deliveries_inbox",
        &format!(
            "SELECT message_id FROM deliveries WHERE recipient_address_id = {a}
             AND recipient_run_id = '' AND archived_at IS NULL ORDER BY message_id LIMIT 50"
        ),
    );
    plan_uses(
        &conn,
        "deliveries_recipient",
        &format!(
            "SELECT message_id FROM deliveries WHERE recipient_address_id = {a}
             AND recipient_run_id = '' AND message_id < 1000 ORDER BY message_id DESC LIMIT 50"
        ),
    );
    plan_uses(
        &conn,
        "deliveries_direct",
        &format!(
            "SELECT message_id FROM deliveries WHERE recipient_address_id = {a}
             AND direct = 1 ORDER BY message_id DESC LIMIT 50"
        ),
    );
    plan_uses(
        &conn,
        "messages_sender",
        &format!(
            "SELECT id FROM messages WHERE sender_address_id = {a} AND id < 1000 ORDER BY id DESC LIMIT 50"
        ),
    );
    plan_uses(
        &conn,
        "messages_recipient",
        &format!(
            "SELECT id FROM messages WHERE recipient_address_id = {a} AND id < 1000 ORDER BY id DESC LIMIT 50"
        ),
    );
    plan_uses(
        &conn,
        "messages_group",
        "SELECT id FROM messages WHERE to_group_id = 10 ORDER BY id DESC LIMIT 50",
    );
    plan_uses(
        &conn,
        "messages_created",
        "SELECT id FROM messages WHERE created_at >= '2026' ORDER BY created_at, id LIMIT 50",
    );
}

#[test]
fn refuses_while_another_connection_uses_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bbs.db");
    drop(file_fixture(&path));
    let snapshot = || dump(&crate::db::open(&path).unwrap());
    let before = snapshot();
    let cases: [(&str, &str); 3] = [
        ("idle reader", "SELECT count(*) FROM messages"),
        (
            "idle writer",
            "UPDATE humans SET active_at = active_at WHERE id = 1",
        ),
        (
            "open read transaction",
            "BEGIN; SELECT count(*) FROM messages",
        ),
    ];
    for (case, setup) in cases {
        let other = crate::db::open(&path).unwrap();
        other.execute_batch(setup).unwrap();
        let mut conn = crate::db::open(&path).unwrap();
        let started = std::time::Instant::now();
        let err = migrate_to_hub_model(&mut conn, "will")
            .unwrap_err()
            .to_string();
        assert!(err.contains("stop bbs-server"), "{case}: {err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{case}"
        );
        assert!(foreign_keys_on(&conn), "{case}");
        // The other connection still works, and nothing changed.
        other
            .query_row("SELECT count(*) FROM humans", [], |r| r.get::<_, i64>(0))
            .unwrap();
        drop((conn, other));
        assert_eq!(snapshot(), before, "{case}");
    }

    // Alone, it migrates; a server opening the database meanwhile cannot read
    // until the migrating connection closes.
    let mut conn = crate::db::open(&path).unwrap();
    migrate_to_hub_model(&mut conn, "will").unwrap();
    let late = Connection::open(&path).unwrap();
    late.busy_timeout(std::time::Duration::ZERO).unwrap();
    assert!(
        late.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .is_err()
    );
    drop(conn);
    assert_eq!(schema_version(&late).unwrap(), HUB_MODEL_SCHEMA_VERSION);
}

/// Preflight one owner's agents, `(id, name, retired_at)`, under human `will`:
/// each agent's entity ID, and the blocking issues.
fn scope(agents: &[(i64, &str, Option<&str>)]) -> (BTreeMap<i64, String>, Vec<String>) {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate_legacy(&mut conn).unwrap();
    conn.execute("INSERT INTO humans VALUES (1, 'will', 't', 't')", [])
        .unwrap();
    for &(id, name, retired_at) in agents {
        conn.execute(
            "INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at, retired_at)
             VALUES (?1, ?2, 'k' || ?1, '/', 'h', 't', 't', ?3)",
            params![id, name, retired_at],
        )
        .unwrap();
        conn.execute("INSERT INTO agent_owners VALUES (?1, 1)", [id])
            .unwrap();
    }
    let report = hub_model_preflight(&conn, "will").unwrap();
    let ids = report
        .mappings
        .iter()
        .filter(|m| m.kind == EntityKind::Agent)
        .map(|m| (m.native_id, m.entity_id.clone()))
        .collect();
    (ids, report.issues)
}

fn ids(expected: &[(i64, &str)]) -> BTreeMap<i64, String> {
    expected.iter().map(|&(id, e)| (id, e.to_owned())).collect()
}

#[test]
fn single_use_names_keep_plain_ids() {
    let (got, issues) = scope(&[
        (1, "solo", None),
        (2, "gone", Some("2026-10-15T10:11:12.000000Z")),
        // Never needs a date, so it need not parse.
        (3, "odd", Some("not a time")),
    ]);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(
        got,
        ids(&[(1, "will/solo"), (2, "will/gone"), (3, "will/odd")])
    );
}

#[test]
fn reused_name_live_agent_keeps_plain_id() {
    let (got, issues) = scope(&[
        (1, "k3", Some("2026-10-15T10:11:12.123456Z")),
        (2, "K3", Some("2026-10-16T08:00:00.000000Z")),
        (3, "k3", None),
    ]);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(
        got,
        ids(&[
            (1, "will/k3-retired-2026-10-15"),
            (2, "will/K3-retired-2026-10-16"),
            (3, "will/k3"),
        ])
    );
}

#[test]
fn reused_name_all_retired_newest_keeps_plain_id() {
    // Newest by retired_at, not by id.
    let (got, issues) = scope(&[
        (1, "k3", Some("2026-10-17T09:00:00.000000Z")),
        (2, "k3", Some("2026-10-15T09:00:00.000000Z")),
    ]);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(
        got,
        ids(&[(1, "will/k3"), (2, "will/k3-retired-2026-10-15")])
    );
    // A tie goes to the highest id. Offsets are read as UTC.
    let (got, issues) = scope(&[
        (1, "k3", Some("2026-10-16T09:00:00.000000Z")),
        (2, "k3", Some("2026-10-15T23:30:00.000000Z")),
        (3, "k3", Some("2026-10-16T11:00:00.000000+02:00")),
    ]);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(
        got,
        ids(&[
            (1, "will/k3-retired-2026-10-16"),
            (2, "will/k3-retired-2026-10-15"),
            (3, "will/k3"),
        ])
    );
}

#[test]
fn dated_id_collisions_fall_back_to_minute_second_then_counter() {
    // Like the sid series: many retired in quick succession.
    let (got, issues) = scope(&[
        (1, "sid", Some("2026-10-15T10:00:00.000000Z")),
        (2, "sid", Some("2026-10-15T11:30:00.000000Z")),
        (3, "sid", Some("2026-10-15T12:00:01.500000Z")),
        (4, "sid", Some("2026-10-15T12:00:59.000000Z")),
        (5, "sid", Some("2026-10-15T12:30:05.900000Z")),
        (6, "sid", Some("2026-10-15T12:30:05.100000Z")),
        (7, "sid", Some("2026-10-15T12:30:05.500000Z")),
        (8, "sid", Some("2026-10-16T09:00:00.000000Z")),
        (9, "sid", None),
    ]);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(
        got,
        ids(&[
            // Same day: minutes.
            (1, "will/sid-retired-2026-10-15-1000"),
            (2, "will/sid-retired-2026-10-15-1130"),
            // Same minute: seconds.
            (3, "will/sid-retired-2026-10-15-120001"),
            (4, "will/sid-retired-2026-10-15-120059"),
            // Same second: counters in id order, not time order.
            (5, "will/sid-retired-2026-10-15-123005-2"),
            (6, "will/sid-retired-2026-10-15-123005-3"),
            (7, "will/sid-retired-2026-10-15-123005-4"),
            (8, "will/sid-retired-2026-10-16"),
            (9, "will/sid"),
        ])
    );
}

#[test]
fn literal_names_equal_to_dated_ids_push_the_fallback() {
    // Resolved, not blocked: the retired agent moves to the next level.
    let (got, issues) = scope(&[
        (1, "k3", Some("2026-10-15T10:11:12.000000Z")),
        (2, "k3", None),
        (3, "K3-Retired-2026-10-15", None),
    ]);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(
        got,
        ids(&[
            (1, "will/k3-retired-2026-10-15-1011"),
            (2, "will/k3"),
            (3, "will/K3-Retired-2026-10-15"),
        ])
    );
    // Every dated form taken: the counter resolves it, skipping taken ones.
    let (got, issues) = scope(&[
        (1, "k3", Some("2026-10-15T10:11:12.000000Z")),
        (2, "k3", None),
        (3, "k3-retired-2026-10-15", None),
        (4, "k3-retired-2026-10-15-1011", None),
        (5, "k3-retired-2026-10-15-101112", None),
        (6, "k3-retired-2026-10-15-101112-2", None),
    ]);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(got[&1], "will/k3-retired-2026-10-15-101112-3");
}

#[test]
fn dated_id_longer_than_128_blocks() {
    // `will/` + name + `-retired-YYYY-MM-DD` (19 characters).
    let fits = "n".repeat(128 - 5 - 19);
    let (got, issues) = scope(&[
        (1, &fits, Some("2026-10-15T10:11:12.000000Z")),
        (2, &fits, None),
    ]);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(got[&1].len(), 128);
    let long = "n".repeat(128 - 5 - 19 + 1);
    let (_, issues) = scope(&[
        (1, &long, Some("2026-10-15T10:11:12.000000Z")),
        (2, &long, None),
    ]);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0].starts_with(&format!(
            "agent 1 `{long}` maps to `will/{long}-retired-2026-10-15`, longer than 128"
        )),
        "{issues:?}"
    );
}

#[test]
fn unparseable_retired_at_blocks_a_dated_id() {
    let (_, issues) = scope(&[(1, "k3", Some("yesterday")), (2, "k3", None)]);
    assert_eq!(
        issues,
        [
            "reused name `will/k3` needs dated IDs, but these retired_at values are not \
             RFC3339 times: agent 1 `k3` retired_at \"yesterday\""
        ]
    );
    // All retired: choosing the newest needs every time.
    let (_, issues) = scope(&[
        (1, "k3", Some("2026-10-15T10:11:12.000000Z")),
        (2, "k3", Some("2026-13-01T00:00:00Z")),
    ]);
    assert!(issues[0].contains("agent 2 `k3` retired_at"), "{issues:?}");
    // And the migration refuses.
    assert_blocked(
        "INSERT INTO agents (id, name, token, pwd, hostname, created_at, active_at, retired_at)
             VALUES (8, 'scout', 'k8', '/', 'h', 't', 't', 'garbage');
         INSERT INTO agent_owners VALUES (8, 1)",
        "agent 8 `scout` retired_at \"garbage\"",
    );
}
