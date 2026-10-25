//! Discord voice media transport (SPEC §6, §8.3): the v8 voice Gateway session,
//! UDP IP discovery and demultiplexing, RTP/RTCP parsing, rtpsize transport
//! encryption (AES-256-GCM and XChaCha20-Poly1305), SSRC-to-user mapping, join
//! correlation, and H.264 video RTP (packetization, reassembly, NACK/RTX/PLI
//! feedback, pacing) around DAVE's frame boundary. No audio devices, codecs,
//! decoders, or UI live here.

pub mod crypto;
mod dave;
pub mod gateway;
pub mod rtcp;
pub mod rtp;
pub mod session;
pub mod ssrc;
pub mod stream;
pub mod udp;
pub mod video;

pub use crypto::TransportMode;
pub use gateway::{
    CloseReason, MediaSendError, MediaSender, ReceivedAudio, TransportStats, VoiceEvent,
    VoiceMessage, VoiceSession, VoiceStatus, connect, connect_with_video, set_handshake_identity,
};
pub use session::{Correlation, Generation, JoinCorrelator, VoiceCredentials};
pub use stream::{StreamCorrelation, StreamCorrelator, StreamCredentials};
