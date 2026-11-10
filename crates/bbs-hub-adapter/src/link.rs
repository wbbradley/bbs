//! The frames waiting for one stream's transport, with the handoffs they
//! carry. A claimed handoff counts as sent when the transport takes its
//! frame from here (accepted into the HTTP/2 send path, not delivered or
//! acknowledged: D072's stream-send boundary, as BBS and Hub use). Every move
//! across that boundary happens under one lock: queuing (only while open),
//! taking (only while open) and closing (which drains what is left). So a
//! claim is either taken, and recorded sent, or drained, and returned to
//! pending because it provably never left; never both.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use tokio::sync::mpsc;
use tokio_stream::Stream;

use crate::store::Direction;

/// What happens to a claim at the boundary.
pub trait Settle: Send + Sync + 'static {
    /// The transport took the claim's frame: sent.
    fn taken(&self, dir: Direction, id: i64);
    /// The claim never left: back to pending.
    fn unsent(&self, dir: Direction, id: i64);
}

/// One frame and the handoff it carries, if any.
pub struct Item<T> {
    pub frame: T,
    pub claim: Option<(Direction, i64)>,
}

struct Queue<T> {
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

    /// Close once, returning every claim still queued to pending.
    fn close(&self) {
        let mut q = self.lock();
        if q.closed {
            return;
        }
        q.closed = true;
        q.rx.close();
        while let Ok(item) = q.rx.try_recv() {
            if let Some((dir, id)) = item.claim {
                self.settle.unsent(dir, id);
            }
        }
    }
}

/// The sending side of a link.
pub struct Sender<T> {
    tx: mpsc::Sender<Item<T>>,
    shared: Arc<Shared<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender {
            tx: self.tx.clone(),
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

    /// Queue a frame that carries no handoff if there is room; false when
    /// the link is full or closed.
    pub fn try_send(&self, frame: T) -> bool {
        let q = self.shared.lock();
        !q.closed && self.tx.try_send(Item { frame, claim: None }).is_ok()
    }

    /// Close the link: nothing more is queued or taken, and claims still
    /// queued return to pending.
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

/// The transport's side: frames in order, each claim recorded as sent when
/// taken. Dropping it closes the link.
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
        match q.rx.poll_recv(cx) {
            Poll::Ready(Some(item)) => {
                if let Some((dir, id)) = item.claim {
                    shared.settle.taken(dir, id);
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
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue { rx, closed: false }),
        settle,
    });
    (
        Sender {
            tx,
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
    struct Record(Mutex<Vec<(&'static str, i64)>>);

    impl Settle for Record {
        fn taken(&self, _: Direction, id: i64) {
            self.0.lock().unwrap().push(("taken", id));
        }
        fn unsent(&self, _: Direction, id: i64) {
            self.0.lock().unwrap().push(("unsent", id));
        }
    }

    #[tokio::test]
    async fn a_claim_is_taken_or_returned_never_both() {
        let record = Arc::new(Record::default());
        let (tx, mut frames) = channel::<u32>(4, record.clone());
        for id in 1..=3 {
            tx.reserve()
                .await
                .unwrap()
                .send(Item {
                    frame: id as u32,
                    claim: Some((Direction::Outbound, id)),
                })
                .ok()
                .unwrap();
        }
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
        assert_eq!(
            *record.0.lock().unwrap(),
            vec![("taken", 1), ("unsent", 2), ("unsent", 3)]
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
                claim: Some((Direction::Inbound, 7)),
            })
            .ok()
            .unwrap();
        drop(frames);
        assert!(tx.is_closed());
        assert_eq!(*record.0.lock().unwrap(), vec![("unsent", 7)]);
    }
}
