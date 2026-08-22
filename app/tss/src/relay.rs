//! In-process message relay for one ceremony.
//!
//! sl-dkls23 talks to its peers through `sl_mpc_mate::coord::Relay` — a
//! pull-model pub-sub: a party *asks* for a message id and receives the
//! payload once some party publishes it. Here every party lives in the same
//! process (the user's browser, or a test thread), so the "network" is a
//! `HashMap<MsgId, …>` behind a mutex.
//!
//! The crate's own `SimpleMessageRelay` would do exactly this, but it calls
//! `Instant::now()` (traps on wasm32-unknown-unknown) and `tokio::spawn`
//! (needs a runtime this crate deliberately does not have). This relay has
//! no clock and no runtime: nothing expires — a ceremony either completes
//! while [`run_parties`] drives it, or the whole simulation is dropped and
//! the map with it.

use std::collections::{hash_map::Entry as MapEntry, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_util::{Sink, Stream};
use sl_mpc_mate::coord::{MessageSendError, Relay};
use sl_mpc_mate::message::{MsgHdr, MsgId, MESSAGE_HEADER_SIZE};

/// One message id's state: published, or a list of parties waiting for it.
enum Slot {
    Ready(Vec<u8>),
    Waiters(Vec<usize>),
}

#[derive(Default)]
struct PartyQueue {
    queue: VecDeque<Vec<u8>>,
    waker: Option<Waker>,
}

#[derive(Default)]
struct Shared {
    messages: HashMap<MsgId, Slot>,
    queues: Vec<PartyQueue>,
}

/// The coordinator all parties of one ceremony connect to.
#[derive(Default)]
pub struct LocalCoordinator {
    shared: Arc<Mutex<Shared>>,
}

impl LocalCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// One party's connection.
    pub fn connect(&self) -> LocalRelay {
        let mut s = self.shared.lock().expect("relay lock");
        s.queues.push(PartyQueue::default());
        LocalRelay {
            shared: self.shared.clone(),
            id: s.queues.len() - 1,
        }
    }
}

/// One party's view of the relay: a `Stream` of incoming messages and a
/// `Sink` for asks (header-only) and publishes (header + payload).
pub struct LocalRelay {
    shared: Arc<Mutex<Shared>>,
    id: usize,
}

impl Stream for LocalRelay {
    type Item = Vec<u8>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let mut s = this.shared.lock().expect("relay lock");
        let q = &mut s.queues[this.id];
        if let Some(m) = q.queue.pop_front() {
            Poll::Ready(Some(m))
        } else {
            q.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Sink<Vec<u8>> for LocalRelay {
    type Error = MessageSendError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let hdr: &MsgHdr = item.as_slice().try_into().map_err(|_| MessageSendError)?;
        let id = *hdr.id();
        let mut guard = this.shared.lock().expect("relay lock");
        let s = &mut *guard;
        if item.len() == MESSAGE_HEADER_SIZE {
            // An ASK: deliver immediately if already published, else wait.
            match s.messages.entry(id) {
                MapEntry::Occupied(mut e) => match e.get_mut() {
                    Slot::Ready(msg) => {
                        let m = msg.clone();
                        let q = &mut s.queues[this.id];
                        q.queue.push_back(m);
                        if let Some(w) = q.waker.take() {
                            w.wake();
                        }
                    }
                    Slot::Waiters(w) => w.push(this.id),
                },
                MapEntry::Vacant(v) => {
                    v.insert(Slot::Waiters(vec![this.id]));
                }
            }
        } else {
            // A PUBLISH: store it, hand it to everyone already asking.
            match s.messages.entry(id) {
                MapEntry::Occupied(mut e) => {
                    if let Slot::Waiters(w) = e.get_mut() {
                        let waiters = std::mem::take(w);
                        *e.get_mut() = Slot::Ready(item.clone());
                        for cid in waiters {
                            let q = &mut s.queues[cid];
                            q.queue.push_back(item.clone());
                            if let Some(wk) = q.waker.take() {
                                wk.wake();
                            }
                        }
                    }
                    // Already Ready: a duplicate publish, ignored — the
                    // same choice SimpleMessageRelay makes.
                }
                MapEntry::Vacant(v) => {
                    v.insert(Slot::Ready(item));
                }
            }
        }
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl Relay for LocalRelay {}

/// Drive every party's future to completion on the calling thread.
///
/// All wakes come from [`LocalRelay`]'s own sends, so a single-threaded
/// `block_on(join_all(…))` cannot stall on outside I/O — the same loop runs
/// on a native test thread and inside a web worker.
pub fn run_parties<F, T>(futures: Vec<F>) -> Vec<T>
where
    F: Future<Output = T>,
{
    futures::executor::block_on(futures_util::future::join_all(futures))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use sl_mpc_mate::message::{allocate_message, AskMsg, InstanceId, MessageTag, MsgId};

    fn msg_id(tag: u64) -> MsgId {
        MsgId::new(&InstanceId::from([7; 32]), &[1], None, MessageTag::tag(tag))
    }

    #[test]
    fn publish_then_ask_delivers_and_so_does_ask_then_publish() {
        futures::executor::block_on(async {
            let coordinator = LocalCoordinator::new();
            let mut publisher = coordinator.connect();
            let mut early = coordinator.connect();
            let mut late = coordinator.connect();

            // `early` asks before the message exists; `late` after. Both
            // orders must deliver the same bytes — the protocol's parties
            // interleave arbitrarily.
            let id = msg_id(1);
            early.send(AskMsg::allocate(&id, 10)).await.unwrap();
            let payload = allocate_message(&id, 10, 0, &[42; 5]);
            publisher.send(payload.clone()).await.unwrap();
            late.send(AskMsg::allocate(&id, 10)).await.unwrap();

            assert_eq!(early.next().await.unwrap(), payload);
            assert_eq!(late.next().await.unwrap(), payload);
        });
    }

    #[test]
    fn a_garbled_message_is_a_send_error_not_a_panic() {
        futures::executor::block_on(async {
            let coordinator = LocalCoordinator::new();
            let mut relay = coordinator.connect();
            assert!(relay.send(vec![1, 2, 3]).await.is_err());
        });
    }
}
