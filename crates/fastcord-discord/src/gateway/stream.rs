//! Main Gateway Go Live signal requests (opcodes 18–22).

use std::fmt;

use fastcord_model::StreamCreateRequest;
use tokio::sync::mpsc;

const STREAM_QUEUE_CAPACITY: usize = 64;

/// A main-Gateway stream signal. Payloads contain no credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StreamCommand {
    Create(StreamCreateRequest),
    Delete(String),
    Watch(String),
    Ping(String),
    PauseResume { stream_key: String, paused: bool },
}

/// A stream command could not be queued for the Gateway task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSignalQueueError;

impl fmt::Display for StreamSignalQueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the Gateway stream-signal queue is full or closed")
    }
}

impl std::error::Error for StreamSignalQueueError {}

/// Consumer handle for Go Live signaling. Clones share a bounded FIFO queue;
/// commands are delivered in order after READY or RESUMED.
#[derive(Clone)]
pub struct StreamControl {
    target: mpsc::Sender<StreamCommand>,
}

impl fmt::Debug for StreamControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StreamControl")
    }
}

impl StreamControl {
    pub(crate) fn new() -> (Self, mpsc::Receiver<StreamCommand>) {
        let (target, receiver) = mpsc::channel(STREAM_QUEUE_CAPACITY);
        (Self { target }, receiver)
    }

    fn set(&self, command: StreamCommand) -> Result<(), StreamSignalQueueError> {
        self.target
            .try_send(command)
            .map_err(|_| StreamSignalQueueError)
    }

    /// Start a stream with main Gateway opcode 18.
    pub fn create(&self, request: StreamCreateRequest) -> Result<(), StreamSignalQueueError> {
        self.set(StreamCommand::Create(request))
    }

    /// End or unwatch the selected stream with opcode 19.
    pub fn delete(&self, stream_key: impl Into<String>) -> Result<(), StreamSignalQueueError> {
        self.set(StreamCommand::Delete(stream_key.into()))
    }

    /// Watch the stream identified by the server-advertised key (opcode 20).
    pub fn watch(&self, stream_key: impl Into<String>) -> Result<(), StreamSignalQueueError> {
        self.set(StreamCommand::Watch(stream_key.into()))
    }

    /// Send a stream ping (opcode 21).
    pub fn ping(&self, stream_key: impl Into<String>) -> Result<(), StreamSignalQueueError> {
        self.set(StreamCommand::Ping(stream_key.into()))
    }

    /// Pause or resume the selected stream with opcode 22.
    pub fn pause_resume(
        &self,
        stream_key: impl Into<String>,
        paused: bool,
    ) -> Result<(), StreamSignalQueueError> {
        self.set(StreamCommand::PauseResume {
            stream_key: stream_key.into(),
            paused,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_queue_refuses_without_losing_fifo_order_and_recovers_after_drain() {
        let (control, mut queued) = StreamControl::new();
        for _ in 0..STREAM_QUEUE_CAPACITY {
            control.watch("guild:41:127:1").unwrap();
        }
        assert_eq!(control.ping("guild:41:127:1"), Err(StreamSignalQueueError));
        assert!(matches!(queued.try_recv(), Ok(StreamCommand::Watch(_))));
        control.ping("guild:41:127:1").unwrap();
        for _ in 0..STREAM_QUEUE_CAPACITY - 1 {
            assert!(matches!(queued.try_recv(), Ok(StreamCommand::Watch(_))));
        }
        assert!(matches!(queued.try_recv(), Ok(StreamCommand::Ping(_))));
        assert!(queued.try_recv().is_err());
    }
}
