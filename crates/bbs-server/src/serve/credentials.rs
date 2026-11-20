//! Whose credentials a call may have changed, so an open stream re-checks
//! its caller then, rather than on a short timer.
//!
//! The mail update hook also passes each changed row of the tables
//! `service::whoami` authenticates against to [`Credentials::hook`]. A change
//! to an agent's row (its token or retirement) or to its owner concerns that
//! agent; a change to a human's row concerns that human. A change to
//! `human_tokens` can disable a human and every agent they own, so it
//! concerns everyone, as does a commit by another process, such as
//! `bbs admin human revoke`, which the hook cannot see.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::watch;

use crate::proto::Kind;

/// A principal, as (kind, id).
pub(super) type Key = (i32, i64);

/// What the hook saw since the last call finished.
#[derive(Default)]
struct Changed {
    principals: HashSet<Key>,
    everyone: bool,
}

/// When credentials last changed, on one counter: the latest change, the
/// latest that concerned everyone, and the latest for each principal.
#[derive(Default)]
pub(super) struct Revisions {
    latest: u64,
    everyone: u64,
    by_principal: HashMap<Key, u64>,
}

impl Revisions {
    pub(super) fn latest(&self) -> u64 {
        self.latest
    }

    /// Whether `key`'s credentials may have changed after revision `seen`.
    pub(super) fn changed_since(&self, key: Key, seen: u64) -> bool {
        self.everyone > seen || self.by_principal.get(&key).is_some_and(|&r| r > seen)
    }
}

pub(super) struct Credentials {
    changed: Arc<Mutex<Changed>>,
    signal: watch::Sender<Revisions>,
}

impl Default for Credentials {
    fn default() -> Credentials {
        Credentials {
            changed: Default::default(),
            signal: watch::Sender::new(Revisions::default()),
        }
    }
}

impl Credentials {
    /// For the connection's update hook: note a changed row.
    pub(super) fn hook(&self) -> impl FnMut(&str, i64) + Send + 'static {
        let changed = self.changed.clone();
        move |table, rowid| {
            let key = match table {
                // `agent_owners` is keyed by its agent.
                "agents" | "agent_owners" => Some((Kind::Agent as i32, rowid)),
                "humans" => Some((Kind::Human as i32, rowid)),
                "human_tokens" => None,
                _ => return,
            };
            let mut c = changed.lock().unwrap_or_else(PoisonError::into_inner);
            match key {
                Some(key) => {
                    c.principals.insert(key);
                }
                None => c.everyone = true,
            }
        }
    }

    /// After a call: tell the watchers of the credentials it changed.
    pub(super) fn after_call(&self) {
        let changed =
            std::mem::take(&mut *self.changed.lock().unwrap_or_else(PoisonError::into_inner));
        if changed.principals.is_empty() && !changed.everyone {
            return;
        }
        self.signal.send_modify(|r| {
            r.latest += 1;
            for key in changed.principals {
                r.by_principal.insert(key, r.latest);
            }
            if changed.everyone {
                r.everyone = r.latest;
            }
        });
    }

    /// Have every watcher re-check.
    pub(super) fn everyone(&self) {
        self.signal.send_modify(|r| {
            r.latest += 1;
            r.everyone = r.latest;
        });
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<Revisions> {
        self.signal.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::db;
    use crate::proto;
    use crate::service::{self, Caller};

    #[test]
    fn a_write_concerns_only_the_credentials_it_changes() {
        let mut conn = Connection::open_in_memory().unwrap();
        db::migrate(&mut conn).unwrap();
        let human_token = service::admin_issue_token(&mut conn, "will", true).unwrap();
        let will = Caller {
            human_token: Some(human_token),
            require_human_token: true,
            ..Default::default()
        };
        let mut agent = |name: &str| {
            let req = proto::RegisterRequest {
                name: Some(name.into()),
                ..Default::default()
            };
            let token = service::register(&mut conn, &will, req).unwrap().token;
            let caller = Caller {
                token: Some(token),
                require_human_token: true,
                ..Default::default()
            };
            let id = service::whoami(&mut conn, &caller).unwrap().id;
            (caller, (Kind::Agent as i32, id))
        };
        let (bo, bo_key) = agent("Bo");
        let (_ana, ana_key) = agent("Ana");
        let will_key = (
            Kind::Human as i32,
            service::whoami(&mut conn, &will).unwrap().id,
        );
        let credentials = Credentials::default();
        let mut hook = credentials.hook();
        conn.update_hook(Some(move |_, _: &str, table: &str, rowid| {
            hook(table, rowid)
        }))
        .unwrap();
        let revisions = credentials.subscribe();
        let changed = |conn: &mut Connection, f: &dyn Fn(&mut Connection)| {
            let seen = revisions.borrow().latest();
            f(conn);
            credentials.after_call();
            let r = revisions.borrow();
            [ana_key, bo_key, will_key].map(|key| r.changed_since(key, seen))
        };

        // Mail and a whoami that records nothing new concern no one.
        let send = |conn: &mut Connection| {
            let req = proto::SendRequest {
                to: "will".into(),
                body: "hi".into(),
                ..Default::default()
            };
            service::send(conn, &bo, req).unwrap();
        };
        assert_eq!(changed(&mut conn, &send), [false; 3]);
        assert_eq!(
            changed(&mut conn, &|c| drop(service::whoami(c, &bo).unwrap())),
            [false; 3]
        );
        // A change to an agent's row concerns only that agent.
        let status = |conn: &mut Connection| {
            let text = "busy".to_string();
            service::set_status(conn, &bo, proto::SetStatusRequest { text }).unwrap();
        };
        assert_eq!(changed(&mut conn, &status), [false, true, false]);
        let rotate = |conn: &mut Connection| {
            let req = proto::RetireRequest {
                agent: "Ana".into(),
            };
            service::rotate_agent_token(conn, &will, req).unwrap();
        };
        assert_eq!(changed(&mut conn, &rotate), [true, false, false]);
        // So does its ownership.
        let owners = |conn: &mut Connection| {
            conn.execute(
                "UPDATE agent_owners SET human_id = human_id WHERE agent_id = ?1",
                [bo_key.1],
            )
            .unwrap();
        };
        assert_eq!(changed(&mut conn, &owners), [false, true, false]);
        // A human's row concerns that human.
        let human = |conn: &mut Connection| {
            conn.execute("UPDATE humans SET name = name", []).unwrap();
        };
        assert_eq!(changed(&mut conn, &human), [false, false, true]);
        // A human token concerns everyone, as does another process's commit.
        let revoke = |conn: &mut Connection| service::admin_revoke_token(conn, "will").unwrap();
        assert_eq!(changed(&mut conn, &revoke), [true; 3]);
        assert_eq!(changed(&mut conn, &|_| credentials.everyone()), [true; 3]);
    }
}
