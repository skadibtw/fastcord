//! A small facade over the UI-independent client core (reducer worker,
//! history coordinator, outbox) for integration tests against `fastcord-fake`
//! (#176). It drives the same `gateway::status_stream` the application polls
//! and reads the same published snapshots; nothing here is used by the app.
//! Everything else in the crate stays private.

use std::sync::Arc;

use fastcord_discord::{RestClient, UserToken};
use fastcord_model::Snowflake;

use crate::gateway::{GatewayStatus, NavigationBridge, status_stream};
use crate::outbox::OutboxState;

/// What the worker last published for the open conversation.
#[derive(Debug, Default)]
pub struct View {
    /// The Gateway reached READY.
    pub ready: bool,
    pub channel: Option<Snowflake>,
    /// Retained messages of `channel`, oldest first, as (id, content).
    pub messages: Vec<(Snowflake, String)>,
    /// Sends not yet confirmed by Discord.
    pub unconfirmed: usize,
    /// An unconfirmed send ended in `Failed` or `Uncertain`.
    pub send_trouble: bool,
}

/// One account's worker, started on the caller's Tokio runtime.
pub struct Core {
    bridge: NavigationBridge,
    task: tokio::task::JoinHandle<()>,
    ready: std::sync::atomic::AtomicBool,
}

impl Core {
    pub fn start(rest: RestClient, token: Arc<UserToken>) -> Self {
        use futures_util::StreamExt;
        let bridge = NavigationBridge::new();
        let stream = status_stream(rest, token, bridge.clone());
        let task = tokio::spawn(async move {
            futures_util::pin_mut!(stream);
            while stream.next().await.is_some() {}
        });
        bridge.set_viewing(true);
        Self {
            bridge,
            task,
            ready: false.into(),
        }
    }

    pub fn open_channel(&self, channel: Snowflake) {
        self.bridge.open_channel(channel);
    }

    /// Queues a text message; `false` when the outbox refused it.
    pub fn send(&self, channel: Snowflake, content: &str) -> bool {
        self.bridge
            .send_message_with_files(channel, content.to_owned(), Vec::new(), false, None)
    }

    pub fn edit(&self, channel: Snowflake, message: Snowflake, content: &str) -> bool {
        self.bridge
            .edit_message(channel, message, content.to_owned(), None)
    }

    pub fn delete(&self, channel: Snowflake, message: Snowflake) -> bool {
        self.bridge.delete_message(channel, message)
    }

    /// The latest published state.
    pub fn view(&self) -> View {
        use std::sync::atomic::Ordering::Relaxed;
        let consumed = self.bridge.consume();
        if matches!(consumed.status, Some(GatewayStatus::Ready(_))) {
            self.ready.store(true, Relaxed);
        }
        let timeline = &consumed.timeline;
        View {
            ready: self.ready.load(Relaxed),
            channel: timeline.channel_id,
            messages: timeline
                .rows
                .iter()
                .map(|row| (row.message.id, row.message.content.clone()))
                .collect(),
            unconfirmed: timeline.outbox.len(),
            send_trouble: timeline
                .outbox
                .iter()
                .any(|item| !matches!(item.state, OutboxState::Sending)),
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.task.abort();
    }
}
