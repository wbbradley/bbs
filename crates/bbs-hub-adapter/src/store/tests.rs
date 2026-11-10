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
    let unreported = store.unreported(10).unwrap();
    assert_eq!(unreported.len(), 1);
    assert_eq!(unreported[0].handoff_id, "bbs-h1");
    assert_eq!(unreported[0].detail, "no such recipient");
    store.mark_reported(c.id).unwrap();
    assert!(store.unreported(10).unwrap().is_empty());
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
    assert_eq!(dup, Intake::Duplicate);
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
    // A reused BBS handoff ID for another message conflicts too.
    let reused = store
        .intake_outbound("bbs-h1", "h9", &origin("m2"), "hub", b"other", 1_000, 10)
        .unwrap();
    assert_eq!(reused, Intake::Conflict);

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
    assert_eq!(inbound("x", b"one"), Intake::Duplicate);
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
    assert_eq!(store.unreported(10).unwrap()[0].handoff_id, "bbs-h1");
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
    assert!(store.put_catalog(&catalog(3), 1).unwrap());
    assert!(!store.put_catalog(&catalog(3), 1).unwrap());
    assert!(!store.put_catalog(&catalog(2), 1).unwrap());
    assert!(store.put_catalog(&catalog(4), 1).unwrap());
    assert_eq!(store.catalog_revision().unwrap(), Some(4));
}
