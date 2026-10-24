//! Searching the stored history off the UI thread: each query replaces the
//! last, and a query overtaken by a newer one stops early and says nothing.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

use super::store::{Found, Place, Store};

/// Results kept per query, as many as the launcher shows.
const LIMIT: usize = 50;
/// Stored messages read for a conversation, as many as a window holds.
const CONTEXT: usize = 200;

pub struct Searcher {
    queries: mpsc::Sender<(u64, String)>,
    /// The newest query asked for; older ones give way to it, even inside
    /// a scan that finds nothing.
    latest: Arc<AtomicU64>,
}

impl Searcher {
    /// A worker searching the history in `dir`; `done` gets each query's
    /// generation and its results, or why it failed.
    pub fn spawn(
        dir: PathBuf,
        done: impl Fn(u64, Result<Vec<Found>, String>) + Send + 'static,
    ) -> Searcher {
        let (queries, rx) = mpsc::channel::<(u64, String)>();
        let latest = Arc::new(AtomicU64::new(0));
        let newest = latest.clone();
        std::thread::spawn(move || {
            let mut store: Option<Store> = None;
            while let Ok(mut query) = rx.recv() {
                // Only the newest of whatever is waiting matters.
                while let Ok(newer) = rx.try_recv() {
                    query = newer;
                }
                let (generation, text) = query;
                let latest = newest.clone();
                let stale = move || latest.load(Ordering::Acquire) != generation;
                if stale() {
                    continue;
                }
                if let Err(e) = current(&dir, &mut store) {
                    done(generation, Err(e.to_string()));
                    continue;
                }
                let opened = store.as_ref().expect("opened above");
                match opened.search(&text, LIMIT, stale) {
                    Ok(Some(found)) => done(generation, Ok(found)),
                    // Overtaken: the newer query answers instead.
                    Ok(None) => {}
                    Err(e) => {
                        // Reopen next time, in case the file moved on.
                        store = None;
                        done(generation, Err(e.to_string()));
                    }
                }
            }
        });
        Searcher { queries, latest }
    }

    /// Search for `query`; its results arrive with the returned generation.
    /// Whatever query is running stops soon after.
    pub fn search(&self, query: &str) -> u64 {
        let generation = self.latest.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.queries.send((generation, query.to_string()));
        generation
    }
}

/// Open the history in `dir`, or reopen it when sessions moved on to a new
/// generation: an old one's messages never answer a new question.
fn current(
    dir: &std::path::Path,
    store: &mut Option<Store>,
) -> Result<(), super::store::StoreError> {
    if let Some(open) = store
        && open.is_current()?
    {
        return Ok(());
    }
    *store = Some(Store::open(dir)?);
    Ok(())
}

/// Where stored messages are read for, owned so it can cross to a worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Where {
    Agent(String),
    Group(String),
    Feed,
}

/// Reading a conversation's stored messages off the UI thread.
pub struct Contexts {
    jobs: mpsc::Sender<(u64, Where, Option<i64>)>,
    next: AtomicU64,
}

impl Contexts {
    /// A worker reading the history in `dir`; `done` gets each read's
    /// generation and its messages, oldest first, or why it failed.
    pub fn spawn(
        dir: PathBuf,
        done: impl Fn(u64, Result<Vec<crate::proto::Message>, String>) + Send + 'static,
    ) -> Contexts {
        let (jobs, rx) = mpsc::channel::<(u64, Where, Option<i64>)>();
        std::thread::spawn(move || {
            let mut store: Option<Store> = None;
            while let Ok((generation, place, around)) = rx.recv() {
                if let Err(e) = current(&dir, &mut store) {
                    done(generation, Err(e.to_string()));
                    continue;
                }
                let place = match &place {
                    Where::Agent(name) => Place::Agent(name),
                    Where::Group(name) => Place::Group(name),
                    Where::Feed => Place::Feed,
                };
                let opened = store.as_ref().expect("opened above");
                match opened.context(place, around, CONTEXT) {
                    Ok(rows) => done(generation, Ok(rows)),
                    Err(e) => {
                        store = None;
                        done(generation, Err(e.to_string()));
                    }
                }
            }
        });
        Contexts {
            jobs,
            next: AtomicU64::new(0),
        }
    }

    /// Read `place`'s stored messages around `around`, or the newest; they
    /// arrive with the returned generation.
    pub fn fetch(&self, place: Where, around: Option<i64>) -> u64 {
        let generation = self.next.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.jobs.send((generation, place, around));
        generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::history::Batch;
    use crate::proto;
    use std::time::Duration;

    #[test]
    fn each_query_answers_with_its_generation_and_older_ones_give_way() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let messages = (1..=3000)
            .map(|id| proto::Message {
                id,
                from_name: "Zed".into(),
                body: format!("needle number {id}"),
                ..Default::default()
            })
            .collect();
        store
            .apply(
                &Batch {
                    messages,
                    scopes: Vec::new(),
                    caught_up: true,
                },
                None,
            )
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let searcher = Searcher::spawn(dir.path().to_path_buf(), move |generation, result| {
            tx.send((generation, result.map(|f| f.len()))).unwrap();
        });
        // Asked in quick succession, only the last is sure to answer.
        let first = searcher.search("needle");
        let second = searcher.search("needle");
        let last = searcher.search("number 2999");
        let mut answers = Vec::new();
        while let Ok(answer) = rx.recv_timeout(Duration::from_secs(5)) {
            let done = answer.0 == last;
            answers.push(answer);
            if done {
                break;
            }
        }
        assert_eq!(answers.last(), Some(&(last, Ok(1))));
        assert!(
            answers
                .iter()
                .all(|(g, _)| [first, second, last].contains(g))
        );
        assert!(first < second && second < last);
        // A later query still answers.
        let again = searcher.search("needle");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            (again, Ok(50))
        );
    }

    #[test]
    fn workers_answer_from_the_generation_sessions_now_write() {
        let dir = tempfile::tempdir().unwrap();
        let row = |id: i64, body: &str| proto::Message {
            id,
            from_kind: proto::Kind::Agent as i32,
            from_name: "Zed".into(),
            body: body.into(),
            ..Default::default()
        };
        let page = |rows: Vec<proto::Message>| Batch {
            scopes: rows
                .iter()
                .map(|m| (m.id, vec![(7, "Zed".to_string())]))
                .collect(),
            messages: rows,
            caught_up: true,
        };
        let mut old = Store::open(dir.path()).unwrap();
        old.apply(&page(vec![row(1, "old needle")]), None).unwrap();
        let (tx, rx) = mpsc::channel();
        let found = tx.clone();
        let searcher = Searcher::spawn(dir.path().to_path_buf(), move |g, r| {
            found
                .send((
                    g,
                    r.map(|f| f.iter().map(|f| f.message.body.clone()).collect::<Vec<_>>()),
                ))
                .unwrap();
        });
        let contexts = Contexts::spawn(dir.path().to_path_buf(), move |g, r| {
            tx.send((g, r.map(|m| m.iter().map(|m| m.body.clone()).collect())))
                .unwrap();
        });
        let wait = || rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Both workers open generation 1.
        let g = searcher.search("needle");
        assert_eq!(wait(), (g, Ok(vec!["old needle".to_string()])));
        let g = contexts.fetch(Where::Agent("Zed".into()), None);
        assert_eq!(wait(), (g, Ok(vec!["old needle".to_string()])));
        // Another session starts generation 2 with different history.
        let mut new = old.next_generation().unwrap();
        new.apply(&page(vec![row(1, "new needle")]), None).unwrap();
        let g = searcher.search("needle");
        assert_eq!(wait(), (g, Ok(vec!["new needle".to_string()])));
        let g = contexts.fetch(Where::Agent("Zed".into()), None);
        assert_eq!(wait(), (g, Ok(vec!["new needle".to_string()])));
    }
}
