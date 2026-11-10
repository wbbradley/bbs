//! The frames waiting for one stream's transport, with the claims they
//! carry: a handoff, or a stored result. A claim counts as done when the
//! transport takes its frame from here (accepted into the HTTP/2 send path,
//! not delivered or acknowledged: D072's stream-send boundary, as BBS and Hub
//! use). Every move across that boundary happens under one lock: queuing
//! (only while open), taking (only while open) and closing (which drains
//! what is left). So a claim is either taken, and recorded, or drained, and
//! returned because it provably never left; never both.
//!
//! Three queues, taken in this order, as Hub's peer does:
//!
//! - liveness (heartbeats): a few slots, so a full queue never holds one back;
//! - control: results, replies, trust and route requests (Hub's 256 slots);
//! - work: transfers or imports, which wait for room.
//!
//! Lock order: [`Settle`] runs with the queue lock held, so it must take no
//! other lock but its own leaf (it never touches the store; see
//! `adapter::Boundary`). [`holding_queue`] lets the store check that.

use std::cell::Cell;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

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
    /// BBS's result for an import, for the relay stream it came on:
    /// reported when taken; if not, that stream has gone and so has the
    /// result (Hub accepts it on no other).
    RelayResult(i64),
}

/// What happens to a claim at the boundary. Called with the queue lock
/// held: record and return, taking no lock that is ever held while a queue
/// lock is taken.
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

/// Room for liveness frames: a heartbeat waits behind at most a few others.
const PRIORITY: usize = 4;
/// Room for control frames, as Hub's `controlQueue`.
pub const CONTROL: usize = 256;

/// Why a frame could not be queued without waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Full,
    Closed,
}

struct Queue<T> {
    priority: mpsc::Receiver<T>,
    control: mpsc::Receiver<Item<T>>,
    work: mpsc::Receiver<Item<T>>,
    closed: bool,
    /// Since when frames have waited without the transport taking one: it
    /// is blocked (flow control) when this grows old.
    waiting_since: Option<Instant>,
    /// The transport waiting for a frame, woken when the link closes.
    waiter: Option<Waker>,
}

impl<T> Queue<T> {
    fn is_empty(&self) -> bool {
        self.priority.is_empty() && self.control.is_empty() && self.work.is_empty()
    }

    fn queued(&mut self) {
        self.waiting_since.get_or_insert_with(Instant::now);
    }

    fn took(&mut self) {
        self.waiting_since = (!self.is_empty()).then(Instant::now);
    }
}

thread_local! {
    static HOLDING: Cell<usize> = const { Cell::new(0) };
}

/// Whether this thread holds a link's queue lock (the store must never be
/// taken inside one).
pub fn holding_queue() -> bool {
    HOLDING.with(|h| h.get() > 0)
}

struct Locked<'a, T> {
    guard: MutexGuard<'a, Queue<T>>,
}

impl<T> Drop for Locked<'_, T> {
    fn drop(&mut self) {
        HOLDING.with(|h| h.set(h.get() - 1));
    }
}

impl<T> std::ops::Deref for Locked<'_, T> {
    type Target = Queue<T>;
    fn deref(&self) -> &Queue<T> {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for Locked<'_, T> {
    fn deref_mut(&mut self) -> &mut Queue<T> {
        &mut self.guard
    }
}

struct Shared<T> {
    queue: Mutex<Queue<T>>,
    settle: Arc<dyn Settle>,
}

impl<T> Shared<T> {
    fn lock(&self) -> Locked<'_, T> {
        let guard = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        HOLDING.with(|h| h.set(h.get() + 1));
        Locked { guard }
    }

    /// Close once, returning every claim still queued.
    fn close(&self) {
        let mut q = self.lock();
        if q.closed {
            return;
        }
        q.closed = true;
        q.waiting_since = None;
        q.priority.close();
        q.control.close();
        q.work.close();
        while q.priority.try_recv().is_ok() {}
        while let Ok(item) = q.control.try_recv() {
            if let Some(claim) = item.claim {
                self.settle.unsent(claim);
            }
        }
        while let Ok(item) = q.work.try_recv() {
            if let Some(claim) = item.claim {
                self.settle.unsent(claim);
            }
        }
        if let Some(w) = q.waiter.take() {
            w.wake();
        }
    }
}

/// The sending side of a link.
pub struct Sender<T> {
    priority: mpsc::Sender<T>,
    control: mpsc::Sender<Item<T>>,
    work: mpsc::Sender<Item<T>>,
    shared: Arc<Shared<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender {
            priority: self.priority.clone(),
            control: self.control.clone(),
            work: self.work.clone(),
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
        let mut q = self.shared.lock();
        if q.closed {
            return Err(item);
        }
        self.permit.send(item);
        q.queued();
        Ok(())
    }
}

impl<T> Sender<T> {
    /// Wait for room for one work frame; None once the link has closed.
    pub async fn reserve(&self) -> Option<Permit<T>> {
        let permit = self.work.clone().reserve_owned().await.ok()?;
        Some(Permit {
            permit,
            shared: self.shared.clone(),
        })
    }

    /// Wait for room for one control frame; None once the link has closed.
    pub async fn reserve_control(&self) -> Option<Permit<T>> {
        let permit = self.control.clone().reserve_owned().await.ok()?;
        Some(Permit {
            permit,
            shared: self.shared.clone(),
        })
    }

    /// Queue a control frame that carries no claim, if there is room.
    pub fn try_send_control(&self, frame: T) -> Result<(), Refused> {
        let mut q = self.shared.lock();
        if q.closed {
            return Err(Refused::Closed);
        }
        match self.control.try_send(Item { frame, claim: None }) {
            Ok(()) => {
                q.queued();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => Err(Refused::Full),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Refused::Closed),
        }
    }

    /// Queue a liveness frame ahead of everything else; false when its own
    /// small queue is full or the link is closed.
    pub fn try_send_priority(&self, frame: T) -> bool {
        let mut q = self.shared.lock();
        if q.closed || self.priority.try_send(frame).is_err() {
            return false;
        }
        q.queued();
        true
    }

    /// How long queued frames have waited with the transport taking none:
    /// a transport blocked this long has a peer that is not reading.
    pub fn stalled_for(&self) -> Duration {
        self.shared
            .lock()
            .waiting_since
            .map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// Whether nothing is queued.
    pub fn is_idle(&self) -> bool {
        self.shared.lock().is_empty()
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

/// The transport's side: liveness frames, then control, then work, each
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
        // Every queue registers the waker; they are asked in order.
        if let Poll::Ready(Some(frame)) = q.priority.poll_recv(cx) {
            q.took();
            return Poll::Ready(Some(frame));
        }
        let item = match q.control.poll_recv(cx) {
            Poll::Ready(Some(item)) => item,
            _ => match q.work.poll_recv(cx) {
                Poll::Ready(Some(item)) => item,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {
                    q.waiter = Some(cx.waker().clone());
                    return Poll::Pending;
                }
            },
        };
        q.took();
        if let Some(claim) = item.claim {
            shared.settle.taken(claim);
        }
        Poll::Ready(Some(item.frame))
    }
}

impl<T> Drop for Frames<T> {
    fn drop(&mut self) {
        self.shared.close();
    }
}

/// A new link with room for `work` queued work frames.
pub fn channel<T>(work: usize, settle: Arc<dyn Settle>) -> (Sender<T>, Frames<T>) {
    let (priority_tx, priority) = mpsc::channel(PRIORITY);
    let (control_tx, control) = mpsc::channel(CONTROL);
    let (work_tx, work_rx) = mpsc::channel(work);
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            priority,
            control,
            work: work_rx,
            closed: false,
            waiting_since: None,
            waiter: None,
        }),
        settle,
    });
    (
        Sender {
            priority: priority_tx,
            control: control_tx,
            work: work_tx,
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
            assert!(holding_queue());
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
        tx.reserve_control()
            .await
            .unwrap()
            .send(Item {
                frame: 3,
                claim: Some(Claim::Report(30)),
            })
            .ok()
            .unwrap();
        // Control goes first.
        assert_eq!(frames.next().await, Some(3));
        assert_eq!(frames.next().await, Some(1));
        tx.reserve_control()
            .await
            .unwrap()
            .send(Item {
                frame: 4,
                claim: Some(Claim::RelayResult(40)),
            })
            .ok()
            .unwrap();
        tx.close();
        assert_eq!(frames.next().await, None);
        let permit = tx.reserve().await;
        assert!(
            permit.is_none()
                || permit
                    .unwrap()
                    .send(Item {
                        frame: 5,
                        claim: None
                    })
                    .is_err()
        );
        assert_eq!(tx.try_send_control(5), Err(Refused::Closed));
        assert!(!tx.try_send_priority(6));
        assert!(!holding_queue());
        assert_eq!(
            *record.0.lock().unwrap(),
            vec![
                ("taken", Claim::Report(30)),
                ("taken", Claim::Handoff(Direction::Outbound, 1)),
                ("unsent", Claim::RelayResult(40)),
                ("unsent", Claim::Handoff(Direction::Outbound, 2)),
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

    /// Full work and control queues never hold back a heartbeat, and a full
    /// work queue never holds back control frames.
    #[tokio::test]
    async fn liveness_then_control_go_ahead_of_full_queues() {
        let record = Arc::new(Record::default());
        let (tx, mut frames) = channel::<u32>(3, record.clone());
        for n in 1..=3 {
            tx.reserve()
                .await
                .unwrap()
                .send(Item {
                    frame: n,
                    claim: None,
                })
                .ok()
                .unwrap();
        }
        for n in 0..CONTROL as u32 {
            tx.try_send_control(1000 + n).unwrap();
        }
        assert_eq!(tx.try_send_control(9), Err(Refused::Full));
        assert!(tx.try_send_priority(100));
        assert_eq!(frames.next().await, Some(100));
        assert_eq!(frames.next().await, Some(1000));
        assert!(tx.try_send_priority(101));
        assert_eq!(frames.next().await, Some(101));
        for n in 1..CONTROL as u32 {
            assert_eq!(frames.next().await, Some(1000 + n));
        }
        assert_eq!(frames.next().await, Some(1));
    }

    /// Closing the link ends a transport waiting for a frame, even while
    /// senders remain.
    #[tokio::test]
    async fn closing_ends_a_waiting_transport() {
        let record = Arc::new(Record::default());
        let (tx, mut frames) = channel::<u32>(3, record.clone());
        let waiting = tokio::spawn(async move { frames.next().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.close();
        let ended = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the transport ends");
        assert_eq!(ended.unwrap(), None);
    }

    /// Frames waiting with nothing taken show how long the transport has
    /// been blocked; taking one resets it, and an empty link is not stalled.
    #[tokio::test]
    async fn a_blocked_transport_shows_as_stalled() {
        let record = Arc::new(Record::default());
        let (tx, mut frames) = channel::<u32>(3, record.clone());
        assert_eq!(tx.stalled_for(), Duration::ZERO);
        assert!(tx.try_send_priority(1));
        tx.try_send_control(2).unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(tx.stalled_for() >= Duration::from_millis(50));
        assert_eq!(frames.next().await, Some(1));
        assert!(tx.stalled_for() < Duration::from_millis(50));
        assert!(!tx.is_idle());
        assert_eq!(frames.next().await, Some(2));
        assert_eq!(tx.stalled_for(), Duration::ZERO);
        assert!(tx.is_idle());
    }
}
