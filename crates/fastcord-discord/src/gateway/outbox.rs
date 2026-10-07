//! The byte-bounded queue between the Gateway task and its single consumer.
//!
//! SPEC §4.5 budgets queued Gateway deltas at 2 MiB. Every event reserves its
//! estimated size from a shared byte budget before it is queued and returns it
//! when the consumer takes the event, so a slow consumer applies backpressure
//! (the task stops reading the socket) instead of growing memory, and no
//! ordered event is ever dropped. One event larger than the whole budget (a
//! big READY) takes all of it, i.e. is queued alone.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::event::GatewayEvent;

pub(crate) const QUEUE_BYTES: u32 = 2 * 1024 * 1024;
const QUEUE_EVENTS: usize = 256;

struct Delivery {
    event: GatewayEvent,
    /// Returns the event's bytes to the budget when the consumer drops it.
    _permit: OwnedSemaphorePermit,
}

/// The consumer side.
pub(crate) struct EventReceiver {
    events: mpsc::Receiver<Delivery>,
}

impl EventReceiver {
    pub(crate) async fn recv(&mut self) -> Option<GatewayEvent> {
        let Delivery { event, .. } = self.events.recv().await?;
        Some(event)
    }
}

/// The consumer has gone away; the task should stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConsumerGone;

struct Queued {
    event: GatewayEvent,
    cost: u32,
}

/// Events produced but not yet accepted by the bounded channel. Kept as a
/// field of the connection loop so delivery can be one `select!` branch next
/// to the heartbeat timer: a stalled consumer must never stop heartbeats.
pub(crate) struct Outbox {
    pending: VecDeque<Queued>,
    sender: mpsc::Sender<Delivery>,
    budget: Arc<Semaphore>,
    budget_bytes: u32,
}

pub(crate) fn channel() -> (Outbox, EventReceiver) {
    channel_with_budget(QUEUE_BYTES)
}

pub(crate) fn channel_with_budget(budget_bytes: u32) -> (Outbox, EventReceiver) {
    let (sender, events) = mpsc::channel(QUEUE_EVENTS);
    (
        Outbox {
            pending: VecDeque::new(),
            sender,
            budget: Arc::new(Semaphore::new(budget_bytes as usize)),
            budget_bytes,
        },
        EventReceiver { events },
    )
}

impl Outbox {
    /// Queues an event costing about `cost` bytes of consumer-side memory.
    pub(crate) fn push(&mut self, event: GatewayEvent, cost: usize) {
        let cost = cost.clamp(1, self.budget_bytes as usize) as u32;
        self.pending.push_back(Queued { event, cost });
    }

    pub(crate) fn len(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Moves the oldest pending event into the channel once both the byte
    /// budget and a channel slot are available. Cancel-safe: nothing is taken
    /// from the queue until both reservations succeeded.
    pub(crate) async fn deliver_one(&mut self) -> Result<(), ConsumerGone> {
        let Some(cost) = self.pending.front().map(|queued| queued.cost) else {
            return Ok(());
        };
        let permit = Arc::clone(&self.budget)
            .acquire_many_owned(cost)
            .await
            .map_err(|_| ConsumerGone)?;
        let slot = self.sender.reserve().await.map_err(|_| ConsumerGone)?;
        if let Some(queued) = self.pending.pop_front() {
            slot.send(Delivery {
                event: queued.event,
                _permit: permit,
            });
        }
        Ok(())
    }

    /// Delivers everything pending, in order.
    pub(crate) async fn flush(&mut self) -> Result<(), ConsumerGone> {
        while !self.pending.is_empty() {
            self.deliver_one().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::gateway::event::{ConnectionState, Dispatch};

    fn event(sequence: u64) -> GatewayEvent {
        GatewayEvent::Dispatch {
            sequence,
            event: Dispatch::Resumed,
        }
    }

    fn poll_once<F: Future>(future: &mut std::pin::Pin<&mut F>) -> Poll<F::Output> {
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
    }

    #[tokio::test]
    async fn events_are_delivered_in_order_and_return_their_bytes() {
        let (mut outbox, mut events) = channel_with_budget(1000);
        for sequence in 1..=3 {
            outbox.push(event(sequence), 100);
        }
        outbox.flush().await.unwrap();
        assert!(outbox.is_empty());
        assert_eq!(outbox.budget.available_permits(), 700);
        for sequence in 1..=3 {
            assert_eq!(events.recv().await, Some(event(sequence)));
        }
        assert_eq!(outbox.budget.available_permits(), 1000);
    }

    #[tokio::test]
    async fn queued_bytes_never_exceed_the_budget_and_the_producer_waits() {
        let (mut outbox, mut events) = channel_with_budget(1000);
        for sequence in 1..=4 {
            outbox.push(event(sequence), 400);
        }
        // Two events fit (800 of 1000); the third must wait for the consumer.
        outbox.deliver_one().await.unwrap();
        outbox.deliver_one().await.unwrap();
        {
            let mut third = std::pin::pin!(outbox.deliver_one());
            assert!(poll_once(&mut third).is_pending());
            // The abandoned attempt took nothing from the queue.
        }
        assert_eq!(outbox.len(), 2);
        assert_eq!(events.recv().await, Some(event(1)));
        outbox.deliver_one().await.unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(events.recv().await, Some(event(2)));
        assert_eq!(events.recv().await, Some(event(3)));
        outbox.flush().await.unwrap();
        assert_eq!(events.recv().await, Some(event(4)));
    }

    #[tokio::test]
    async fn an_event_larger_than_the_budget_is_queued_alone() {
        let (mut outbox, mut events) = channel_with_budget(1000);
        outbox.push(event(1), 10_000_000);
        outbox.push(GatewayEvent::State(ConnectionState::Ready), 64);
        outbox.deliver_one().await.unwrap();
        assert_eq!(outbox.budget.available_permits(), 0);
        {
            let mut next = std::pin::pin!(outbox.deliver_one());
            assert!(poll_once(&mut next).is_pending());
        }
        assert_eq!(events.recv().await, Some(event(1)));
        outbox.flush().await.unwrap();
        assert_eq!(
            events.recv().await,
            Some(GatewayEvent::State(ConnectionState::Ready))
        );
    }

    #[tokio::test]
    async fn a_dropped_consumer_is_reported() {
        let (mut outbox, events) = channel_with_budget(1000);
        drop(events);
        outbox.push(event(1), 10);
        assert_eq!(outbox.flush().await, Err(ConsumerGone));
    }
}
