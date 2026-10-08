//! Opcode 4 (Update Voice State) on the main Gateway.
//!
//! The consumer states the voice state it wants as one [`VoiceStateRequest`];
//! the connection task sends it only while the session is ready (after READY
//! or RESUMED, never while Identifying or Resuming), and only when it differs
//! from what this session last sent. Requests made in quick succession
//! coalesce: the newest wins. The voice connection itself (VOICE_SERVER_UPDATE
//! onwards) is owned by the voice layer, not by the Gateway.

use std::fmt;
use std::sync::Arc;

use fastcord_model::VoiceStateRequest;
use tokio::sync::watch;

/// A consumer's handle to the voice state of one [`Gateway`](super::Gateway).
/// Clones share one target.
#[derive(Clone)]
pub struct VoiceStateControl {
    target: Arc<watch::Sender<Option<VoiceStateRequest>>>,
}

impl fmt::Debug for VoiceStateControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VoiceStateControl")
    }
}

impl VoiceStateControl {
    pub(crate) fn new() -> (Self, watch::Receiver<Option<VoiceStateRequest>>) {
        let (sender, receiver) = watch::channel(None);
        (
            Self {
                target: Arc::new(sender),
            },
            receiver,
        )
    }

    /// Makes `request` the wanted voice state. It is sent once the session is
    /// ready, unless this session already sent exactly it; a request equal to
    /// the current one changes nothing.
    pub fn request(&self, request: VoiceStateRequest) {
        self.target.send_if_modified(|target| {
            let changed = *target != Some(request);
            *target = Some(request);
            changed
        });
    }

    /// The wanted voice state, `None` if nothing was ever requested.
    pub fn current(&self) -> Option<VoiceStateRequest> {
        *self.target.borrow()
    }
}

/// Whether `target` still has to be sent, given what this session last sent.
/// A leave is satisfied when nothing was sent in this session: the server
/// holds no voice state of ours to leave.
pub(crate) fn needs_send(
    target: Option<VoiceStateRequest>,
    sent: Option<VoiceStateRequest>,
) -> bool {
    match target {
        None => false,
        Some(target) if target.channel_id.is_none() && sent.is_none() => false,
        Some(target) => sent != Some(target),
    }
}

#[cfg(test)]
mod tests {
    use fastcord_model::Snowflake;

    use super::*;

    fn join(channel: u64) -> VoiceStateRequest {
        VoiceStateRequest::join(Some(Snowflake(1)), Snowflake(channel), false, false)
    }

    #[test]
    fn only_differing_targets_need_a_send_and_a_leave_needs_something_to_leave() {
        let leave = VoiceStateRequest::leave(Some(Snowflake(1)));
        assert!(!needs_send(None, None));
        assert!(!needs_send(None, Some(join(2))));
        assert!(needs_send(Some(join(2)), None));
        assert!(!needs_send(Some(join(2)), Some(join(2))));
        assert!(needs_send(Some(join(3)), Some(join(2))));
        assert!(!needs_send(Some(leave), None));
        assert!(needs_send(Some(leave), Some(join(2))));
        assert!(!needs_send(Some(leave), Some(leave)));
    }

    #[test]
    fn equal_requests_do_not_notify_the_connection() {
        let (control, mut wanted) = VoiceStateControl::new();
        assert_eq!(control.current(), None);
        control.request(join(2));
        assert!(wanted.has_changed().unwrap());
        assert_eq!(*wanted.borrow_and_update(), Some(join(2)));
        control.clone().request(join(2));
        assert!(!wanted.has_changed().unwrap());
        control.request(join(3));
        assert_eq!(control.current(), Some(join(3)));
        assert_eq!(format!("{control:?}"), "VoiceStateControl");
    }
}
