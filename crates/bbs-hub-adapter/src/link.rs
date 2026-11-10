//! The frames waiting for one stream's transport, with the claims they
//! carry: a handoff, or a stored result for BBS. A claim counts as done when
//! the transport takes its frame from here (accepted into the HTTP/2 send
//! path, not delivered or acknowledged: D072's stream-send boundary, as BBS
//! and Hub use). Every move across that boundary happens under one lock:
//! queuing (only while open), taking (only while open) and closing (which
//! drains what is left). So a claim is either taken, and recorded, or
//! drained, and returned because it provably never left; never both.
//!
//! Liveness frames (heartbeats) have a small queue of their own, taken
//! before anything else, so a queue full of transfers never holds them back.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use tokio::sync::mpsc;
use tokio_stream::Stream;

use crate::store::Direction;

/// What a frame carries across the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// A claimed handoff: sent when taken, back to pending if not.
    Handoff(Direction, i64),
    /// A stored result for BBS: reported when taken, reported again later
    /// if not.
    Report(i64),
}

/// What happens to a claim at the boundary.
pub trait Settle: Send + Sync + 'static {
    /// The transport took the claim's frame.
    fn taken(&self, claim: Claim);
    /// The claim never left.
    fn unsent(&self, claim: Claim);
}

/// One frame and what it carries, if anything.
pub struct Item<T> {
    pub frame: T,
    pub claim: Option<Claim>,
}

/// Room for liveness frames: one or two are enough, as in Hub.
const PRIORITY: usize = 4;

struct Queue<T> {
    priority: mpsc::Receiver<T>,
    rx: mpsc::Receiver<Item<T>>,
    closed: bool,
}

struct Shared<T> {
    queue: Mutex<Queue<T>>,
    settle: Arc<dyn Settle>,
}

impl<T> Shared<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue<T>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Close once, returning every claim still queued.
    fn close(&self) {
        let mut q = self.lock();
        if q.closed {
            return;
        }
        q.closed = true;
        q.rx.close();
        q.priority.close();
        while let Ok(item) = q.rx.try_recv() {
            if let Some(claim) = item.claim {
                self.settle.unsent(claim);
            }
        }
    }
}

/// The sending side of a link.
pub struct Sender<T> {
    tx: mpsc::Sender<Item<T>>,
    priority: mpsc::Sender<T>,
    shared: Arc<Shared<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender {
            tx: self.tx.clone(),
            priority: self.priority.clone(),
            shared: self.shared.clone(),
        }
    }
}

/// Room reserved for one frame.
pub struct Permit<T> {
    permit: mpsc::OwnedPermit<Item<T>>,
    shared: Arc<Shared<T>>,
}

impl<T> Permit<T> {
    /// Queue `item` in the reserved room, unless the link has closed, in
    /// which case it is handed back: it was never queued.
    pub fn send(self, item: Item<T>) -> Result<(), Item<T>> {
        let q = self.shared.lock();
        if q.closed {
            return Err(item);
        }
        self.permit.send(item);
        Ok(())
    }
}

impl<T> Sender<T> {
    /// Wait for room for one frame; None once the link has closed.
    pub async fn reserve(&self) -> Option<Permit<T>> {
        let permit = self.tx.clone().reserve_owned().await.ok()?;
        Some(Permit {
            permit,
            shared: self.shared.clone(),
        })
    }

    /// Queue a frame that carries no claim if there is room; false when the
    /// link is full or closed.
    pub fn try_send(&self, frame: T) -> bool {
        let q = self.shared.lock();
        !q.closed && self.tx.try_send(Item { frame, claim: None }).is_ok()
    }

    /// Queue a liveness frame ahead of everything else; false when its own
    /// small queue is full (one is already waiting) or the link is closed.
    pub fn try_send_priority(&self, frame: T) -> bool {
        let q = self.shared.lock();
        !q.closed && self.priority.try_send(frame).is_ok()
    }

    /// Close the link: nothing more is queued or taken, and claims still
    /// queued are returned.
    pub fn close(&self) {
        self.shared.close();
    }

    /// Whether `other` is a sender for the same link.
    pub fn same(&self, other: &Sender<T>) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    pub fn is_closed(&self) -> bool {
        self.shared.lock().closed
    }
}

/// The transport's side: liveness frames first, then frames in order, each
/// claim recorded when taken. Dropping it closes the link.
pub struct Frames<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Stream for Frames<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let shared = &self.shared;
        let mut q = shared.lock();
        if q.closed {
            return Poll::Ready(None);
        }
        // Both queues register the waker; the priority one is asked first.
        if let Poll::Ready(Some(frame)) = q.priority.poll_recv(cx) {
            return Poll::Ready(Some(frame));
        }
        match q.rx.poll_recv(cx) {
            Poll::Ready(Some(item)) => {
                if let Some(claim) = item.claim {
                    shared.settle.taken(claim);
                }
                Poll::Ready(Some(item.frame))
            }
            other => other.map(|_| None),
        }
    }
}

impl<T> Drop for Frames<T> {
    fn drop(&mut self) {
        self.shared.close();
    }
}

/// A new link with room for `capacity` queued frames.
pub fn channel<T>(capacity: usize, settle: Arc<dyn Settle>) -> (Sender<T>, Frames<T>) {
    let (tx, rx) = mpsc::channel(capacity);
    let (priority_tx, priority) = mpsc::channel(PRIORITY);
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            priority,
            rx,
            closed: false,
        }),
        settle,
    });
    (
        Sender {
            tx,
            priority: priority_tx,
            shared: shared.clone(),
        },
        Frames { shared },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt as _;

    #[derive(Default)]
    struct Record(Mutex<Vec<(&'static str, Claim)>>);

    impl Settle for Record {
        fn taken(&self, claim: Claim) {
            self.0.lock().unwrap().push(("taken", claim));
        }
        fn unsent(&self, claim: Claim) {
            self.0.lock().unwrap().push(("unsent", claim));
        }
    }

    fn handoff(id: i64) -> Option<Claim> {
        Some(Claim::Handoff(Direction::Outbound, id))
    }

    #[tokio::test]
    async fn a_claim_is_taken_or_returned_never_both() {
        let record = Arc::new(Record::default());
        let (tx, mut frames) = channel::<u32>(4, record.clone());
        for id in 1..=2 {
            tx.reserve()
                .await
                .unwrap()
                .send(Item {
                    frame: id as u32,
                    claim: handoff(id),
                })
                .ok()
                .unwrap();
        }
        tx.reserve()
            .await
            .unwrap()
            .send(Item {
                frame: 3,
                claim: Some(Claim::Report(30)),
            })
            .ok()
            .unwrap();
        assert!(tx.try_send(9));
        assert_eq!(frames.next().await, Some(1));
        tx.close();
        assert_eq!(frames.next().await, None);
        let permit = tx.reserve().await;
        assert!(
            permit.is_none()
                || permit
                    .unwrap()
                    .send(Item {
                        frame: 4,
                        claim: None
                    })
                    .is_err()
        );
        assert!(!tx.try_send(5));
        assert!(!tx.try_send_priority(6));
        assert_eq!(
            *record.0.lock().unwrap(),
            vec![
                ("taken", Claim::Handoff(Direction::Outbound, 1)),
                ("unsent", Claim::Handoff(Direction::Outbound, 2)),
                ("unsent", Claim::Report(30)),
            ]
        );
    }

    #[tokio::test]
    async fn dropping_the_transport_side_returns_queued_claims() {
        let record = Arc::new(Record::default());
        let (tx, frames) = channel::<u32>(2, record.clone());
        tx.reserve()
            .await
            .unwrap()
            .send(Item {
                frame: 1,
                claim: Some(Claim::Handoff(Direction::Inbound, 7)),
            })
            .ok()
            .unwrap();
        drop(frames);
        assert!(tx.is_closed());
        assert_eq!(
            *record.0.lock().unwrap(),
            vec![("unsent", Claim::Handoff(Direction::Inbound, 7))]
        );
    }

    /// A full queue never holds back a heartbeat: it is taken next.
    #[tokio::test]
    async fn liveness_frames_go_ahead_of_a_full_queue() {
        let record = Arc::new(Record::default());
        let (tx, mut frames) = channel::<u32>(3, record.clone());
        for n in 1..=3 {
            assert!(tx.try_send(n));
        }
        assert!(!tx.try_send(4), "the queue is full");
        assert!(tx.try_send_priority(100));
        assert_eq!(frames.next().await, Some(100));
        assert_eq!(frames.next().await, Some(1));
        assert!(tx.try_send_priority(101));
        assert_eq!(frames.next().await, Some(101));
        assert_eq!(frames.next().await, Some(2));
    }
}
