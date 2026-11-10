use std::os::unix::fs::PermissionsExt as _;

use super::*;

fn origin(id: &str) -> mp::MessageRef {
    mp::MessageRef {
        namespace: "bbs".into(),
        message_id: id.into(),
    }
}

fn open(dir: &tempfile::TempDir) -> Store {
    Store::open(&dir.path().join("adapter.db"), 1).unwrap()
}

fn outbound(store: &Store, n: u32, expires: i64) -> Intake {
    store
        .intake_outbound(
            &format!("bbs-h{n}"),
            &format!("h{n}"),
            &origin(&format!("m{n}")),
            "hub",
            format!("bytes {n}").as_bytes(),
            expires,
            10,
        )
        .unwrap()
}

fn seal(m: &[u8]) -> Vec<u8> {
    [b"env:".as_slice(), m].concat()
}

#[test]
fn the_store_is_owner_only_and_used_by_one_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adapter.db");
    let store = open(&dir);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let err = Store::open(&path, 1).err().unwrap().to_string();
    assert!(err.contains("another adapter"), "{err}");
    drop(store);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = Store::open(&path, 1).err().unwrap().to_string();
    assert!(err.contains("accessible to other users"), "{err}");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    open(&dir);
}

/// A store from an earlier schema is refused with what to do, not migrated.
#[test]
fn an_older_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adapter.db");
    drop(open(&dir));
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 1)
        .unwrap();
    let err = Store::open(&path, 1).err().unwrap().to_string();
    assert!(
        err.contains("schema 1") && err.contains("delete the store"),
        "{err}"
    );
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", VERSION + 1)
        .unwrap();
    let err = Store::open(&path, 1).err().unwrap().to_string();
    assert!(err.contains("this adapter knows"), "{err}");
}

#[test]
fn a_store_belongs_to_one_installation() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let cred = mp::SignedRecord::default();
    store
        .check_identity("bbs", "bbs-1", &[1; 32], &cred, 1)
        .unwrap();
    // A new key for the same installation is fine.
    store
        .check_identity("bbs", "bbs-1", &[2; 32], &cred, 2)
        .unwrap();
    assert!(
        store
            .check_identity("work", "bbs-1", &[2; 32], &cred, 3)
            .is_err()
    );
    assert!(
        store
            .check_identity("bbs", "bbs-2", &[2; 32], &cred, 3)
            .is_err()
    );
}

#[test]
fn a_handoff_is_claimed_before_it_is_sent_and_settles_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    assert_eq!(outbound(&store, 1, 1_000), Intake::New);
    let c = store.claim_outbound("conn-1", 20, seal).unwrap().unwrap();
    assert_eq!(c.bytes, b"env:bytes 1");
    assert_eq!(
        store.state(Direction::Outbound, c.id).unwrap().unwrap().0,
        "attempted"
    );
    assert_eq!(store.claim_outbound("conn-1", 20, seal).unwrap(), None);
    store.mark_sent(Direction::Outbound, c.id, 21).unwrap();
    // Only the stream it was attempted on may answer.
    assert!(
        !store
            .outbound_result("h1", "conn-2", "received", "", "", 22)
            .unwrap()
    );
    assert!(
        store
            .outbound_result(
                "h1",
                "conn-1",
                "rejected",
                "recipient_unknown",
                "no such recipient",
                22
            )
            .unwrap()
    );
    // The first result stands; a late send record never downgrades it.
    assert!(
        !store
            .outbound_result("h1", "conn-1", "received", "", "", 23)
            .unwrap()
    );
    store.mark_sent(Direction::Outbound, c.id, 24).unwrap();
    assert_eq!(
        store.state(Direction::Outbound, c.id).unwrap().unwrap(),
        ("rejected".into(), "recipient_unknown".into())
    );
    let unreported = store.unreported(0, 10).unwrap();
    assert_eq!(unreported.len(), 1);
    assert_eq!(unreported[0].handoff_id, "bbs-h1");
    assert_eq!(unreported[0].detail, "no such recipient");
    // Owed until the transport takes it, however often it was read.
    assert_eq!(store.unreported(0, 10).unwrap(), unreported);
    assert!(store.unreported(unreported[0].id, 10).unwrap().is_empty());
    // Taken, it is kept a while for a new stream, then purged.
    store.mark_reported(unreported[0].id, 30).unwrap();
    store.purge_settled(i64::MIN, 31).unwrap();
    assert_eq!(store.unreported(0, 10).unwrap(), unreported);
    store
        .purge_settled(i64::MIN, 30 + REPORT_KEEP_MICROS)
        .unwrap();
    assert!(store.unreported(0, 10).unwrap().is_empty());
}

/// Theo's repro (C1): a stream's cursor only moves forward, so report IDs
/// must never be reused, even once every report has been purged.
#[test]
fn report_ids_are_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    store
        .owe_report("bbs-h1", "received", "", "", None, 1)
        .unwrap();
    let first = store.unreported(0, 10).unwrap();
    let after = first[0].id;
    store.mark_reported(after, 2).unwrap();
    store
        .purge_settled(i64::MIN, 2 + REPORT_KEEP_MICROS)
        .unwrap();
    assert!(store.unreported(0, 10).unwrap().is_empty());
    store
        .owe_report("bbs-h2", "received", "", "", None, 3)
        .unwrap();
    let next = store.unreported(after, 10).unwrap();
    assert_eq!(next.len(), 1, "the same stream's cursor finds the new one");
    assert_eq!(next[0].handoff_id, "bbs-h2");
}

/// A taken result is kept until its handoff's deadline at the latest.
#[test]
fn a_taken_result_is_kept_no_longer_than_its_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    store
        .owe_report("bbs-h1", "received", "", "", Some(100), 1)
        .unwrap();
    store
        .owe_report("bbs-h2", "received", "", "", None, 1)
        .unwrap();
    let owed = store.unreported(0, 10).unwrap();
    // Untaken past its deadline: still owed.
    store.purge_settled(i64::MIN, 200).unwrap();
    assert_eq!(store.unreported(0, 10).unwrap().len(), 2);
    store.mark_reported(owed[0].id, 50).unwrap();
    store.mark_reported(owed[1].id, 50).unwrap();
    store.purge_settled(i64::MIN, 99).unwrap();
    assert_eq!(store.unreported(0, 10).unwrap().len(), 2);
    store.purge_settled(i64::MIN, 100).unwrap();
    let left = store.unreported(0, 10).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].handoff_id, "bbs-h2");
}

#[test]
fn results_the_adapter_settles_itself_are_owed_too() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    store
        .owe_report("bbs-h9", "rejected", "invalid_message", "bad", None, 1)
        .unwrap();
    store
        .owe_report("bbs-h10", "received", "already_known", "", None, 2)
        .unwrap();
    let owed = store.unreported(0, 10).unwrap();
    assert_eq!(
        owed.iter()
            .map(|o| (o.handoff_id.as_str(), o.state.as_str(), o.reason.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("bbs-h9", "rejected", "invalid_message"),
            ("bbs-h10", "received", "already_known")
        ]
    );
    store.mark_reported(owed[0].id, 3).unwrap();
    assert_eq!(
        store.unreported(owed[0].id, 10).unwrap()[0].handoff_id,
        "bbs-h10"
    );
}

#[test]
fn an_attempted_handoff_is_unknown_after_a_restart_and_never_offered_again() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut store = open(&dir);
        outbound(&store, 1, 1_000);
        outbound(&store, 2, 1_000);
        store.claim_outbound("conn-1", 20, seal).unwrap().unwrap();
        store
            .intake_inbound(
                "in-1",
                "relay-1",
                "conn-1",
                &origin("x1"),
                b"m",
                b"e",
                1_000,
                10,
            )
            .unwrap();
        store.claim_inbound(20).unwrap().unwrap();
    }
    let mut store = open(&dir);
    assert_eq!(
        store.state(Direction::Outbound, 1).unwrap().unwrap(),
        ("unknown".into(), "restart_after_attempt".into())
    );
    assert_eq!(
        store.state(Direction::Inbound, 1).unwrap().unwrap().0,
        "unknown"
    );
    // Only the work known unsent is offered.
    let c = store.claim_outbound("conn-2", 30, seal).unwrap().unwrap();
    assert_eq!(c.handoff_id, "h2");
    assert_eq!(store.claim_outbound("conn-2", 30, seal).unwrap(), None);
    assert_eq!(store.claim_inbound(30).unwrap(), None);
    // An unknown import is offered to BBS again when BBS reconnects (BBS
    // answers already known if it has it); an unknown outbound send never.
    assert_eq!(store.reoffer_inbound(30).unwrap(), 1);
    assert_eq!(store.claim_inbound(31).unwrap().unwrap().handoff_id, "in-1");
    // An unknown handoff can still learn its result, without another try.
    assert!(
        store
            .outbound_result("h1", "conn-1", "received", "", "", 31)
            .unwrap()
    );
}

#[test]
fn only_a_claim_that_never_left_returns_to_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    outbound(&store, 1, 1_000);
    let c = store.claim_outbound("conn-1", 20, seal).unwrap().unwrap();
    store.requeue(Direction::Outbound, c.id, 21).unwrap();
    // Offered again with the envelope signed at the first claim.
    let again = store
        .claim_outbound("conn-2", 22, |_| b"resigned".to_vec())
        .unwrap()
        .unwrap();
    assert_eq!(again.bytes, c.bytes);
    store.mark_sent(Direction::Outbound, c.id, 23).unwrap();
    // Sent work never returns to pending, even by a stray requeue or SQL.
    store.requeue(Direction::Outbound, c.id, 24).unwrap();
    assert_eq!(
        store.state(Direction::Outbound, c.id).unwrap().unwrap().0,
        "sent"
    );
    assert!(
        store
            .conn
            .execute(
                "UPDATE outbound SET state = 'pending' WHERE id = ?1",
                [c.id]
            )
            .is_err()
    );
}

#[test]
fn duplicates_and_conflicts_store_nothing_new() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    assert_eq!(outbound(&store, 1, 1_000), Intake::New);
    // The same origin and bytes under another handoff: already known.
    let dup = store
        .intake_outbound(
            "bbs-other",
            "h9",
            &origin("m1"),
            "hub",
            b"bytes 1",
            1_000,
            10,
        )
        .unwrap();
    let Intake::Duplicate(prior) = dup else {
        panic!("{dup:?}");
    };
    assert_eq!(prior.state, "pending");
    let conflict = store
        .intake_outbound(
            "bbs-other",
            "h9",
            &origin("m1"),
            "hub",
            b"changed",
            1_000,
            10,
        )
        .unwrap();
    assert_eq!(conflict, Intake::Conflict);
    // A reused BBS handoff ID for another message, and the same handoff
    // offered again.
    let reused = store
        .intake_outbound("bbs-h1", "h9", &origin("m2"), "hub", b"other", 1_000, 10)
        .unwrap();
    assert_eq!(reused, Intake::HandoffReused);
    let repeated = store
        .intake_outbound("bbs-h1", "h9", &origin("m1"), "hub", b"bytes 1", 1_000, 10)
        .unwrap();
    assert!(matches!(repeated, Intake::Repeated(p) if p.state == "pending"));

    let inbound = |id: &str, bytes: &[u8]| {
        store
            .intake_inbound(
                &format!("in-{id}"),
                "r",
                "c",
                &origin(id),
                bytes,
                b"env",
                1_000,
                10,
            )
            .unwrap()
    };
    assert_eq!(inbound("x", b"one"), Intake::New);
    assert!(matches!(inbound("x", b"one"), Intake::Duplicate(_)));
    assert_eq!(inbound("x", b"two"), Intake::Conflict);
    assert_eq!(
        store.counts(Direction::Outbound).unwrap(),
        vec![("pending".into(), 1)]
    );
    assert_eq!(
        store.counts(Direction::Inbound).unwrap(),
        vec![("pending".into(), 1)]
    );
}

#[test]
fn pending_work_past_its_deadline_is_rejected_as_expired() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    outbound(&store, 1, 50);
    outbound(&store, 2, 1_000);
    // Expired work is never claimed.
    let c = store.claim_outbound("conn-1", 60, seal).unwrap().unwrap();
    assert_eq!(c.handoff_id, "h2");
    assert_eq!(store.expire(Direction::Outbound, 60).unwrap(), vec![1]);
    assert_eq!(
        store.state(Direction::Outbound, 1).unwrap().unwrap(),
        ("rejected".into(), "expired".into())
    );
    assert_eq!(store.unreported(0, 10).unwrap()[0].handoff_id, "bbs-h1");
    // Claimed work is not expired: it is no longer pending.
    assert!(store.expire(Direction::Outbound, 5_000).unwrap().is_empty());

    store
        .intake_inbound(
            "in-1",
            "relay-1",
            "conn-1",
            &origin("x"),
            b"m",
            b"e",
            50,
            10,
        )
        .unwrap();
    let ids = store.expire(Direction::Inbound, 60).unwrap();
    let o = store.inbound_outcome_of(ids[0]).unwrap().unwrap();
    assert_eq!(
        (o.handoff_id.as_str(), o.connection.as_str()),
        ("relay-1", "conn-1")
    );
    assert_eq!(
        (o.state.as_str(), o.reason.as_str()),
        ("rejected", "expired")
    );
    // Its result is owed to that relay stream only.
    let mut queued = HashSet::new();
    assert!(
        store
            .relay_unreported("conn-2", &mut queued, 10)
            .unwrap()
            .is_empty()
    );
    let owed = store.relay_unreported("conn-1", &mut queued, 10).unwrap();
    assert_eq!(owed.len(), 1);
    store.mark_relay_reported(owed[0].id).unwrap();
    assert!(
        store
            .relay_unreported("conn-1", &mut queued, 10)
            .unwrap()
            .is_empty()
    );
}

fn import(store: &mut Store, n: u32) -> i64 {
    store
        .intake_inbound(
            &format!("in-{n}"),
            &format!("relay-{n}"),
            "conn-1",
            &origin(&format!("x{n}")),
            format!("m{n}").as_bytes(),
            b"e",
            1_000,
            10,
        )
        .unwrap();
    store.claim_inbound(20).unwrap().unwrap().id
}

/// Theo's repro: import results land in any order, not row order. With
/// imports 1 and 2 offered and 2 answered (and queued) first, 1's result
/// must still be found on the same stream; what is queued is not found
/// again, and leaves `queued` once reported.
#[test]
fn import_results_are_found_whatever_order_they_land_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let first = import(&mut store, 1);
    let second = import(&mut store, 2);
    let mut queued = HashSet::new();
    let ids = |owed: Vec<Outcome>| owed.iter().map(|o| o.id).collect::<Vec<_>>();
    store
        .inbound_result("in-2", "received", "", "", 30)
        .unwrap()
        .unwrap();
    let owed = store.relay_unreported("conn-1", &mut queued, 10).unwrap();
    assert_eq!(ids(owed), vec![second]);
    queued.insert(second);
    store
        .inbound_result("in-1", "rejected", "recipient_unknown", "", 31)
        .unwrap()
        .unwrap();
    let owed = store.relay_unreported("conn-1", &mut queued, 10).unwrap();
    assert_eq!(ids(owed), vec![first], "the earlier row's later result");
    queued.insert(first);
    assert_eq!(queued, HashSet::from([first, second]));
    // The transport took the second: it leaves `queued`; the first stays.
    store.mark_relay_reported(second).unwrap();
    assert!(
        store
            .relay_unreported("conn-1", &mut queued, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(queued, HashSet::from([first]));
    // A limit counts only rows not queued.
    let third = import(&mut store, 3);
    store
        .inbound_result("in-3", "received", "", "", 32)
        .unwrap()
        .unwrap();
    let owed = store.relay_unreported("conn-1", &mut queued, 1).unwrap();
    assert_eq!(ids(owed), vec![third]);
}

/// Sent work with no result by its deadline becomes unknown; settled rows
/// go once retention allows.
#[test]
fn sent_work_lapses_and_settled_work_is_purged() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    outbound(&store, 1, 100);
    outbound(&store, 2, 1_000);
    let c = store.claim_outbound("conn-1", 20, seal).unwrap().unwrap();
    store.mark_sent(Direction::Outbound, c.id, 21).unwrap();
    assert_eq!(store.lapse(Direction::Outbound, 99).unwrap(), 0);
    assert_eq!(store.lapse(Direction::Outbound, 100).unwrap(), 1);
    assert_eq!(
        store.state(Direction::Outbound, c.id).unwrap().unwrap(),
        ("unknown".into(), "no_result_by_deadline".into())
    );
    // Settled at 100; the pending row is never purged.
    assert_eq!(store.purge_settled(100, 200).unwrap(), (0, 0));
    assert_eq!(store.purge_settled(101, 200).unwrap(), (1, 0));
    assert_eq!(
        store.counts(Direction::Outbound).unwrap(),
        vec![("pending".into(), 1)]
    );
}

#[test]
fn bbs_results_settle_imports_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store
        .intake_inbound(
            "in-1",
            "relay-1",
            "conn-1",
            &origin("x"),
            b"m",
            b"e",
            1_000,
            10,
        )
        .unwrap();
    // No result before the import is offered.
    assert_eq!(
        store
            .inbound_result("in-1", "received", "", "", 11)
            .unwrap(),
        None
    );
    let c = store.claim_inbound(12).unwrap().unwrap();
    assert_eq!(
        (c.handoff_id.as_str(), c.bytes.as_slice()),
        ("in-1", b"m".as_slice())
    );
    let o = store
        .inbound_result("in-1", "received", "already_known", "", 13)
        .unwrap()
        .unwrap();
    assert_eq!(
        (o.handoff_id.as_str(), o.reason.as_str()),
        ("relay-1", "already_known")
    );
    assert_eq!(
        store
            .inbound_result("in-1", "rejected", "x", "", 14)
            .unwrap(),
        None
    );
}

#[test]
fn bindings_only_move_forward() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let b = |key: u8, revision: u64| Binding {
        namespace: "hub".into(),
        authority_key: vec![key; 32],
        revision,
        disabled: false,
    };
    assert!(store.put_binding(&b(1, 2), 1).unwrap());
    assert!(!store.put_binding(&b(1, 2), 1).unwrap());
    assert!(
        !store.put_binding(&b(9, 1), 1).unwrap(),
        "older revisions are ignored"
    );
    assert!(
        store.put_binding(&b(9, 2), 1).is_err(),
        "same revision, other contents"
    );
    assert_eq!(
        store.binding("hub").unwrap().unwrap().authority_key,
        vec![1; 32]
    );
    assert!(store.put_binding(&b(9, 3), 1).unwrap());
    assert!(store.enrolled("hub").unwrap());
    assert!(
        store
            .put_binding(
                &Binding {
                    disabled: true,
                    ..b(9, 4)
                },
                1
            )
            .unwrap()
    );
    assert!(!store.enrolled("hub").unwrap());
}

#[test]
fn catalogs_only_move_forward() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let catalog = |revision| bbs_core::hub_proto::integration::PublishedCatalog {
        revision,
        entities: Vec::new(),
    };
    assert!(store.put_catalog(&catalog(3), true, 1).unwrap());
    assert!(!store.put_catalog(&catalog(3), false, 1).unwrap());
    assert!(!store.put_catalog(&catalog(2), false, 1).unwrap());
    assert!(store.put_catalog(&catalog(4), false, 1).unwrap());
    assert_eq!(store.catalog_revision().unwrap(), Some(4));
    // A new stream's snapshot is taken whatever its revision: BBS's
    // database may have been reset.
    assert!(store.put_catalog(&catalog(1), true, 2).unwrap());
    assert_eq!(store.catalog_revision().unwrap(), Some(1));
    assert!(store.put_catalog(&catalog(2), false, 2).unwrap());
}

/// An import offered again after its result was lost may already be in
/// BBS: past its deadline it lapses to unknown, owing the relay nothing,
/// never rejected as expired. One never offered is still rejected.
#[test]
fn a_reoffered_import_lapses_to_unknown_not_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let sent = import(&mut store, 1);
    store.mark_sent(Direction::Inbound, sent, 21).unwrap();
    store
        .intake_inbound(
            "in-2",
            "relay-2",
            "conn-1",
            &origin("x2"),
            b"m2",
            b"e",
            1_000,
            10,
        )
        .unwrap();
    // BBS reconnects: the sent import goes back to pending, offered again.
    assert_eq!(store.reoffer_inbound(30).unwrap(), 1);
    assert_eq!(
        store.state(Direction::Inbound, sent).unwrap().unwrap().0,
        "pending"
    );
    // Its deadline passes before BBS takes it again.
    let rejected = store.expire(Direction::Inbound, 1_000).unwrap();
    assert_eq!(rejected.len(), 1, "only the import never offered");
    assert_ne!(rejected[0], sent);
    assert_eq!(store.lapse(Direction::Inbound, 1_000).unwrap(), 1);
    assert_eq!(
        store.state(Direction::Inbound, sent).unwrap().unwrap(),
        ("unknown".into(), "no_result_by_deadline".into())
    );
    // The relay hears only the definite rejection, not the uncertain one.
    let owed = store
        .relay_unreported("conn-1", &mut HashSet::new(), 10)
        .unwrap();
    assert_eq!(
        owed.iter().map(|o| o.id).collect::<Vec<_>>(),
        vec![rejected[0]]
    );
}
