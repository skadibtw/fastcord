//! Discord voice media transport (SPEC §6): the v8 voice Gateway session, UDP
//! IP discovery and demultiplexing, RTP/RTCP parsing, rtpsize transport
//! encryption (AES-256-GCM and XChaCha20-Poly1305), SSRC-to-user mapping, and
//! join correlation. No audio devices, codecs, or UI live here.

pub mod crypto;
mod dave;
pub mod gateway;
pub mod rtcp;
pub mod rtp;
pub mod session;
pub mod ssrc;
pub mod udp;

pub use crypto::TransportMode;
pub use gateway::{
    CloseReason, MediaSendError, MediaSender, ReceivedAudio, TransportStats, VoiceEvent,
    VoiceSession, VoiceStatus, connect,
};
pub use session::{Correlation, Generation, JoinCorrelator, VoiceCredentials};
