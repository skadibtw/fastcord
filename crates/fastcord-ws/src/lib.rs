//! WebSocket transport pieces shared by the main Gateway, the remote-auth
//! gateway and the voice Gateway.
//!
//! `tungstenite` 0.30 is locked without a permessage-deflate feature and
//! ignores an unsolicited `Sec-WebSocket-Extensions` response, so this crate
//! owns the opening handshake ([`handshake`]) and an RFC 7692 frame adapter
//! ([`stream`]) that sits under `tungstenite`. The framing, control frames,
//! UTF-8 validation, close handshake and size limits stay `tungstenite`'s; both
//! WebSocket crates use exactly this implementation, and neither depends on the
//! other.

pub mod extension;
pub mod handshake;
pub mod stream;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;

pub use extension::{ExtensionError, OFFER, Offer, PerMessageDeflate};
pub use handshake::{HandshakeError, connect};
pub use stream::{AdapterError, CompressingStream, Limits, adapter_error, classify, is_size_limit};

/// The socket type both callers expose: `tungstenite` framing over the
/// permessage-deflate adapter.
pub type Socket<S> = tokio_tungstenite::WebSocketStream<CompressingStream<S>>;
