//! Which `WaitForMail` streams a call may have given mail, so a write wakes
//! only them rather than every waiter.
//!
//! An update hook on the server's connection notes the rows each call
//! changes. A new delivery wakes the waiter it is addressed to, and a change
//! to an agent's row (its focus or retirement) wakes that agent. A change
//! that can release anyone's held mail, such as a drain, an owner, an address
//! or a blocker answer, wakes every waiter. Other writes, such as status,
//! context or activity, wake no one. Deliveries are only ever archived in
//! place, so an update to one never brings mail back.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::Connection;
use rusqlite::hooks::Action;
use tokio::sync::watch;

use crate::proto::Kind;

/// Tables whose every change may release held mail for any waiter.
const EVERYONE: &[&str] = &[
    "agent_drains",
    "agent_drain_targets",
    "agent_owners",
    "blocker_events",
    "entity_addresses",
    "entity_local_bindings",
];

/// A waiter's inbox, as (kind, id).
pub(super) type Key = (i32, i64);

/// What the hook saw since the last call finished.
#[derive(Default)]
struct Changed {
    deliveries: Vec<i64>,
    agents: HashSet<i64>,
    everyone: bool,
}

#[derive(Default)]
struct Waiters {
    /// Each waiter's native address, the `entity_addresses` row deliveries name.
    by_address: HashMap<i64, Key>,
    woken: HashSet<Key>,
    /// Bumped to wake every waiter.
    everyone: u64,
}

pub(super) struct Mail {
    hooked: AtomicBool,
    changed: Arc<Mutex<Changed>>,
    waiters: Mutex<Waiters>,
    signal: watch::Sender<u64>,
}

impl Default for Mail {
    fn default() -> Mail {
        Mail {
            hooked: AtomicBool::new(false),
            changed: Default::default(),
            waiters: Default::default(),
            signal: watch::Sender::new(0),
        }
    }
}

impl Mail {
    /// Install the hook on the server's connection, once.
    pub(super) fn attach(&self, conn: &Connection) {
        if self.hooked.swap(true, Ordering::AcqRel) {
            return;
        }
        let changed = self.changed.clone();
        let hook = move |action: Action, _db: &str, table: &str, rowid: i64| {
            let mut c = changed.lock().unwrap_or_else(PoisonError::into_inner);
            match table {
                "deliveries" if action == Action::SQLITE_INSERT => c.deliveries.push(rowid),
                "agents" => {
                    c.agents.insert(rowid);
                }
                t if EVERYONE.contains(&t) => c.everyone = true,
                _ => {}
            }
        };
        if let Err(err) = conn.update_hook(Some(hook)) {
            // Without the hook no waiter would wake on mail; fail loudly.
            panic!("installing the mail update hook: {err}");
        }
    }

    /// After a call, while it still holds the connection: wake the waiters
    /// whose mail its changes may have released.
    pub(super) fn after_call(&self, conn: &Connection) {
        let changed =
            std::mem::take(&mut *self.changed.lock().unwrap_or_else(PoisonError::into_inner));
        if changed.deliveries.is_empty() && changed.agents.is_empty() && !changed.everyone {
            return;
        }
        let recipients = if changed.deliveries.is_empty() {
            Ok(Vec::new())
        } else {
            recipients(conn, &changed.deliveries)
        };
        let mut waiters = self.waiters.lock().unwrap_or_else(PoisonError::into_inner);
        let before = (waiters.woken.len(), waiters.everyone);
        match recipients {
            Ok(addresses) => {
                for address in addresses {
                    if let Some(&key) = waiters.by_address.get(&address) {
                        waiters.woken.insert(key);
                    }
                }
            }
            Err(err) => {
                tracing::warn!("finding new mail's recipients: {err:#}; waking every waiter");
                waiters.everyone += 1;
            }
        }
        for id in changed.agents {
            let key = (Kind::Agent as i32, id);
            if waiters.by_address.values().any(|&k| k == key) {
                waiters.woken.insert(key);
            }
        }
        if changed.everyone {
            waiters.everyone += 1;
        }
        if (waiters.woken.len(), waiters.everyone) != before {
            drop(waiters);
            self.signal.send_modify(|v| *v += 1);
        }
    }

    pub(super) fn register(&self, key: Key, address: i64) {
        let mut waiters = self.waiters.lock().unwrap_or_else(PoisonError::into_inner);
        waiters.by_address.insert(address, key);
    }

    pub(super) fn unregister(&self, key: Key) {
        let mut waiters = self.waiters.lock().unwrap_or_else(PoisonError::into_inner);
        waiters.by_address.retain(|_, k| *k != key);
        waiters.woken.remove(&key);
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<u64> {
        self.signal.subscribe()
    }

    /// The current wake-everyone count, to pass to [`Mail::take`].
    pub(super) fn everyone(&self) -> u64 {
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .everyone
    }

    /// Whether `key` was woken since it last asked, clearing the wake.
    pub(super) fn take(&self, key: Key, everyone: &mut u64) -> bool {
        let mut waiters = self.waiters.lock().unwrap_or_else(PoisonError::into_inner);
        let mut woken = waiters.woken.remove(&key);
        if waiters.everyone != *everyone {
            *everyone = waiters.everyone;
            woken = true;
        }
        woken
    }
}

/// The distinct recipient addresses of the deliveries with these rowids.
fn recipients(conn: &Connection, rowids: &[i64]) -> rusqlite::Result<Vec<i64>> {
    let mut out = HashSet::new();
    for chunk in rowids.chunks(500) {
        let ids = chunk
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT recipient_address_id FROM deliveries WHERE rowid IN ({ids})"
        ))?;
        for address in stmt.query_map([], |r| r.get::<_, i64>(0))? {
            out.insert(address?);
        }
    }
    Ok(out.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use bbs_core::entity::EntityKind;

    use super::*;
    use crate::db;
    use crate::proto;
    use crate::service::{self, Caller};

    fn agent(conn: &mut Connection, owner: &Caller, name: &str) -> (Caller, i64) {
        let token = service::register(
            conn,
            owner,
            proto::RegisterRequest {
                name: Some(name.into()),
                ..Default::default()
            },
        )
        .unwrap()
        .token;
        let caller = Caller {
            token: Some(token),
            ..Default::default()
        };
        let id = service::whoami(conn, &caller).unwrap().id;
        (caller, id)
    }

    fn send(conn: &mut Connection, from: &Caller, to: &str) {
        service::send(
            conn,
            from,
            proto::SendRequest {
                to: to.into(),
                body: "hi".into(),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn a_write_wakes_only_the_waiters_it_concerns() {
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        let will = Caller {
            human: Some("will".into()),
            ..Default::default()
        };
        service::ensure_human(
            &mut conn,
            proto::EnsureHumanRequest {
                name: "will".into(),
            },
        )
        .unwrap();
        let (_ana, ana_id) = agent(&mut conn, &will, "Ana");
        let (bo, _) = agent(&mut conn, &will, "Bo");
        let (cy, _) = agent(&mut conn, &will, "Cy");
        let mail = Mail::default();
        mail.attach(&conn);
        let key = (Kind::Agent as i32, ana_id);
        let address = db::native_address_row(&conn, EntityKind::Agent, ana_id).unwrap();
        mail.register(key, address);
        let mut everyone = mail.everyone();
        let signal = mail.subscribe();
        let mut woke = |conn: &mut Connection, f: &dyn Fn(&mut Connection)| {
            f(conn);
            mail.after_call(conn);
            mail.take(key, &mut everyone)
        };

        // Another agent's status, and mail between others, wake no one.
        let status = |conn: &mut Connection| {
            let text = "busy".to_string();
            service::set_status(conn, &bo, proto::SetStatusRequest { text }).unwrap();
        };
        assert!(!woke(&mut conn, &status));
        assert!(!woke(&mut conn, &|c| send(c, &bo, "Cy")));
        assert!(!woke(&mut conn, &|c| send(c, &cy, "will")));
        assert_eq!(*signal.borrow(), 0);

        // Mail to the waiter wakes it.
        assert!(woke(&mut conn, &|c| send(c, &bo, "Ana")));
        // So does a change to its own row, such as its focus.
        let focus = |conn: &mut Connection| {
            let req = proto::SetFocusRequest {
                agent: "Ana".into(),
                focused: true,
            };
            service::set_focus(conn, &will, req).unwrap();
        };
        assert!(woke(&mut conn, &focus));
        // A change that can release anyone's held mail wakes every waiter.
        let owners = |conn: &mut Connection| {
            conn.execute("UPDATE agent_owners SET human_id = human_id", [])
                .unwrap();
        };
        assert!(woke(&mut conn, &owners));
        assert!(!woke(&mut conn, &|_| {}));

        // Once it stops waiting, nothing wakes it.
        mail.unregister(key);
        assert!(!woke(&mut conn, &|c| send(c, &will, "Ana")));
    }
}
