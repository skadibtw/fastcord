//! Native video over the voice transport (SPEC §8.3, milestones 25–26).
//!
//! Video travels as RTP on the same UDP socket and under the same transport
//! AEAD as audio, one encoded frame at a time: the codec's frame is sealed with
//! DAVE first, then packetized; on receipt the packets are repaired and
//! reassembled into the complete sealed frame before DAVE opens it. Nothing
//! here decodes or renders pixels. H.264 sends and receives; AV1 receives only.
//! Payload types, RTX payload types, and the directions offered for a codec
//! come from the caller's [`VideoOptions`], never from browser SDP.
//!
//! - [`h264`]: RFC 6184 packetization (single NAL unit, FU-A) and
//!   depacketization (also STAP-A).
//! - [`av1`]: AV1 aggregation and OBU fragment reassembly.
//! - [`send`]: the pacer, the retransmission ring, the congestion controller,
//!   and the per-connection sender state.
//! - [`receive`]: per-SSRC reordering, NACK scheduling, frame completion
//!   deadlines, and reception statistics for receiver reports.

pub mod av1;
#[cfg(test)]
pub(crate) mod av1_support;
pub(crate) mod h264;
pub(crate) mod receive;
pub(crate) mod send;
#[cfg(test)]
mod vectors;
#[cfg(test)]
mod vectors_av1;

use std::fmt;
use std::time::Duration;

use fastcord_model::Snowflake;

use crate::rtp::OPUS_PAYLOAD_TYPE;

/// Target size of one UDP datagram, RTP header, extensions, AEAD tag, and
/// nonce suffix included (SPEC §8.3).
pub const MAX_DATAGRAM_BYTES: usize = 1200;
/// The largest encoded frame, sealed or not, in either direction.
pub const MAX_FRAME_BYTES: usize = 2 << 20;
/// RTP packets one frame may occupy.
pub(crate) const MAX_PACKETS: usize = 2048;
/// RTP header (12), extension preamble (4), the longest extension block fastcord
/// writes (24), AEAD tag (16), nonce suffix (4), and the RTX original sequence
/// number (2): what a payload shares a datagram with.
pub const MAX_PAYLOAD_BYTES: usize = MAX_DATAGRAM_BYTES - 12 - 4 - 24 - 16 - 4 - 2;

/// The codecs fastcord can carry, advertised separately by direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    Av1,
}

impl VideoCodec {
    /// The name in Select Protocol and Session Description.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Av1 => "AV1",
        }
    }

    pub fn from_wire(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("H264") {
            Some(Self::H264)
        } else if name.eq_ignore_ascii_case("AV1") {
            Some(Self::Av1)
        } else {
            None
        }
    }
}

/// One codec offered in Select Protocol, with the direction fastcord can
/// actually run it. The server picks one send codec per client that every
/// other client can decode; a codec offered `decode` only is never sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecOffer {
    pub codec: VideoCodec,
    /// Dynamic RTP payload type of the media (97–127, never Opus's 120).
    pub payload_type: u8,
    /// Dynamic RTP payload type of its RFC 4588 retransmissions.
    pub rtx_payload_type: u8,
    pub encode: bool,
    pub decode: bool,
}

impl CodecOffer {
    /// H.264 with the payload types of Discord's documented example offer.
    pub const fn h264(encode: bool, decode: bool) -> Self {
        Self {
            codec: VideoCodec::H264,
            payload_type: 103,
            rtx_payload_type: 104,
            encode,
            decode,
        }
    }

    /// AV1 with Discord's documented offer numbers; receive only.
    pub const fn av1() -> Self {
        Self {
            codec: VideoCodec::Av1,
            payload_type: 101,
            rtx_payload_type: 102,
            encode: false,
            decode: true,
        }
    }

    fn validate(self) -> Result<(), VideoError> {
        // 96 is reserved for probe packets; RTCP types (192–223) must stay
        // clear of a payload type with its marker bit (RFC 5761).
        let dynamic = |payload_type: u8| {
            (97..=127).contains(&payload_type) && payload_type != OPUS_PAYLOAD_TYPE
        };
        if !dynamic(self.payload_type)
            || !dynamic(self.rtx_payload_type)
            || self.payload_type == self.rtx_payload_type
            || !(self.encode || self.decode)
            || (self.codec == VideoCodec::Av1 && self.encode)
        {
            return Err(VideoError::Negotiation);
        }
        Ok(())
    }
}

/// The single screen layer an owner sends (SPEC §8.2), announced in Identify
/// and the Video opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendOptions {
    /// The initial and the highest bitrate, bits per second; congestion control
    /// goes below it and returns up to it.
    pub bitrate: u32,
    pub width: u16,
    pub height: u16,
    pub framerate: u8,
}

impl SendOptions {
    /// The SPEC §8.2 baseline: 1280×720 at 30 fps starting at 2.5 Mbit/s.
    pub const SCREEN_720P30: Self = Self {
        bitrate: 2_500_000,
        width: 1280,
        height: 720,
        framerate: 30,
    };

    /// A camera layer (issue #84): 1280×720 at 30 fps starting at 1.5 Mbit/s.
    pub const CAMERA_720P30: Self = Self {
        bitrate: 1_500_000,
        width: 1280,
        height: 720,
        framerate: 30,
    };

    fn validate(self) -> Result<(), VideoError> {
        if !(64_000..=10_000_000).contains(&self.bitrate)
            || !(16..=4096).contains(&self.width)
            || !(16..=4096).contains(&self.height)
            || !(1..=60).contains(&self.framerate)
        {
            return Err(VideoError::Negotiation);
        }
        Ok(())
    }
}

/// What a connection offers for video. Without options a connection is
/// audio-only, exactly as before milestone 25.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoOptions {
    pub codecs: Vec<CodecOffer>,
    /// Owners send one screen layer; watchers offer decode only and no layer.
    pub send: Option<SendOptions>,
    /// A call connection (issue #84): the layer is a camera (`video`, not
    /// `screen`, and no screenshare content type), the connection receives
    /// other participants' cameras as well as sending its own, and starts
    /// wanting no video and sending none until the application says so.
    pub camera: bool,
}

/// Which remote streams a connection asks the server to forward.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum VideoInterest {
    #[default]
    Off,
    /// The best stream of every announced user.
    All,
    /// Only these users' streams (at most [`MAX_INTEREST_USERS`]).
    Users(Vec<Snowflake>),
}

/// Users a [`VideoInterest::Users`] can name: the number of camera streams a
/// connection receives at once.
pub const MAX_INTEREST_USERS: usize = 6;

/// What the application tells a running connection about its video: set
/// through `MediaSender`, applied by the driver.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoControl {
    pub interest: VideoInterest,
    /// Frames may be sent. A camera connection starts with this off and
    /// announces its layer inactive again whenever it turns off.
    pub sending: bool,
}

impl VideoOptions {
    /// A stream viewer: H.264 and AV1 decode, nothing sent.
    pub fn watch() -> Self {
        Self {
            codecs: vec![CodecOffer::h264(false, true), CodecOffer::av1()],
            send: None,
            camera: false,
        }
    }

    /// A call participant that may turn a camera on: H.264 encode/decode and
    /// AV1 decode, one camera layer (issue #84).
    pub fn camera(send: SendOptions) -> Self {
        Self {
            codecs: vec![CodecOffer::h264(true, true), CodecOffer::av1()],
            send: Some(send),
            camera: true,
        }
    }

    /// A call participant that only receives cameras (a platform with no
    /// camera capture yet).
    pub fn call_watch() -> Self {
        Self {
            camera: true,
            ..Self::watch()
        }
    }

    /// A stream owner: H.264 encode/decode and AV1 decode, one screen layer.
    pub fn share(send: SendOptions) -> Self {
        Self {
            codecs: vec![CodecOffer::h264(true, true), CodecOffer::av1()],
            send: Some(send),
            camera: false,
        }
    }

    /// Whether the offer is internally consistent: unique codecs and payload
    /// types, and an H.264 encoder when a layer is to be sent.
    pub fn validate(&self) -> Result<(), VideoError> {
        if self.codecs.is_empty() || self.codecs.len() > 4 {
            return Err(VideoError::Negotiation);
        }
        for (index, offer) in self.codecs.iter().copied().enumerate() {
            offer.validate()?;
            let numbers = [offer.payload_type, offer.rtx_payload_type];
            if self.codecs[..index].iter().any(|other| {
                other.codec == offer.codec
                    || numbers.contains(&other.payload_type)
                    || numbers.contains(&other.rtx_payload_type)
            }) {
                return Err(VideoError::Negotiation);
            }
        }
        if let Some(send) = self.send {
            send.validate()?;
            if self.send_offer(VideoCodec::H264).is_none() {
                return Err(VideoError::Negotiation);
            }
        }
        Ok(())
    }

    /// The offer an incoming payload type belongs to, and whether it names the
    /// RTX stream. Only decodable codecs match; any other payload type is
    /// unnegotiated media.
    pub(crate) fn receive_offer(&self, payload_type: u8) -> Option<(CodecOffer, bool)> {
        self.codecs
            .iter()
            .copied()
            .filter(|offer| offer.decode)
            .find_map(|offer| {
                if payload_type == offer.payload_type {
                    Some((offer, false))
                } else if payload_type == offer.rtx_payload_type {
                    Some((offer, true))
                } else {
                    None
                }
            })
    }

    /// The offer to send with when the server selected `codec`.
    pub(crate) fn send_offer(&self, codec: VideoCodec) -> Option<CodecOffer> {
        self.codecs
            .iter()
            .copied()
            .find(|offer| offer.codec == codec && offer.encode)
    }
}

/// One encoded frame handed to the sender: an Annex B H.264 access unit that
/// has not been through DAVE. `timestamp` is the source frame's presentation
/// time on any monotonic clock; only differences matter.
#[derive(Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub data: Vec<u8>,
    pub timestamp: Duration,
}

impl fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VideoFrame")
            .field("bytes", &self.data.len())
            .field("timestamp", &self.timestamp)
            .finish()
    }
}

/// One complete, DAVE-opened frame from a known sender, in decode order. The
/// first frame after any loss is a keyframe: frames that could not be
/// reassembled or opened are never followed by deltas that depend on them.
#[derive(Clone, PartialEq, Eq)]
pub struct ReceivedVideo {
    pub user_id: Snowflake,
    pub ssrc: u32,
    pub codec: VideoCodec,
    /// The 90 kHz RTP timestamp shared by every packet of the frame.
    pub timestamp: u32,
    /// An IDR picture (H.264) or coded-sequence start (AV1).
    pub keyframe: bool,
    /// H.264 Annex B (long start codes) or AV1 low-overhead OBUs (size fields).
    pub data: Vec<u8>,
}

impl fmt::Debug for ReceivedVideo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceivedVideo")
            .field("user_id", &self.user_id)
            .field("ssrc", &self.ssrc)
            .field("timestamp", &self.timestamp)
            .field("keyframe", &self.keyframe)
            .field("bytes", &self.data.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoError {
    /// Unsupported or inconsistent video negotiation.
    Negotiation,
    Malformed,
    TooLarge,
    Incomplete,
    /// The pacer's 100 ms queue cannot take the frame.
    QueueFull,
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Negotiation => "unsupported or inconsistent video negotiation",
            Self::Malformed => "malformed codec RTP payload",
            Self::TooLarge => "video frame exceeds its byte bound",
            Self::Incomplete => "incomplete video frame",
            Self::QueueFull => "video pacing queue reached its 100 ms bound",
        })
    }
}

impl std::error::Error for VideoError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_and_share_offers_are_valid_and_directional() {
        let watch = VideoOptions::watch();
        watch.validate().unwrap();
        assert!(watch.send_offer(VideoCodec::H264).is_none());
        let share = VideoOptions::share(SendOptions::SCREEN_720P30);
        share.validate().unwrap();
        assert_eq!(
            share.send_offer(VideoCodec::H264),
            Some(CodecOffer::h264(true, true))
        );
        // The offered payload types, and only those, name incoming media.
        let (offer, rtx) = share.receive_offer(103).unwrap();
        assert!(!rtx && offer.codec == VideoCodec::H264);
        assert!(share.receive_offer(104).unwrap().1);
        assert_eq!(share.receive_offer(101), Some((CodecOffer::av1(), false)));
        assert_eq!(share.receive_offer(102), Some((CodecOffer::av1(), true)));
        assert!(share.send_offer(VideoCodec::Av1).is_none());
        for other in [96, 105, 120] {
            assert!(share.receive_offer(other).is_none(), "{other}");
        }
    }

    #[test]
    fn a_call_offers_a_camera_layer_or_only_receives() {
        let camera = VideoOptions::camera(SendOptions::CAMERA_720P30);
        camera.validate().unwrap();
        assert!(camera.camera && camera.send.is_some());
        assert!(camera.send_offer(VideoCodec::H264).is_some());
        let watch = VideoOptions::call_watch();
        watch.validate().unwrap();
        assert!(watch.camera && watch.send.is_none());
        // Sending without an H.264 encoder is as inconsistent as for a share.
        let mut broken = VideoOptions::call_watch();
        broken.send = Some(SendOptions::CAMERA_720P30);
        assert_eq!(broken.validate(), Err(VideoError::Negotiation));
        assert_eq!(VideoControl::default().interest, VideoInterest::Off);
    }

    #[test]
    fn inconsistent_offers_are_refused() {
        let good = CodecOffer::h264(true, true);
        let bad = |change: fn(&mut CodecOffer)| {
            let mut offer = good;
            change(&mut offer);
            VideoOptions {
                codecs: vec![offer],
                send: None,
                camera: false,
            }
            .validate()
        };
        assert_eq!(bad(|o| o.payload_type = 96), Err(VideoError::Negotiation));
        assert_eq!(bad(|o| o.payload_type = 120), Err(VideoError::Negotiation));
        assert_eq!(
            bad(|o| o.rtx_payload_type = 128),
            Err(VideoError::Negotiation)
        );
        assert_eq!(
            bad(|o| o.rtx_payload_type = 103),
            Err(VideoError::Negotiation)
        );
        assert_eq!(
            bad(|o| {
                o.encode = false;
                o.decode = false;
            }),
            Err(VideoError::Negotiation)
        );
        let empty = VideoOptions {
            codecs: Vec::new(),
            send: None,
            camera: false,
        };
        assert_eq!(empty.validate(), Err(VideoError::Negotiation));
        // Sending needs an encoder.
        let mut no_encoder = VideoOptions::watch();
        no_encoder.send = Some(SendOptions::SCREEN_720P30);
        assert_eq!(no_encoder.validate(), Err(VideoError::Negotiation));
        let mut av1_encoder = VideoOptions::watch();
        av1_encoder.codecs[1].encode = true;
        assert_eq!(av1_encoder.validate(), Err(VideoError::Negotiation));
        // Duplicate codecs or payload types.
        let mut duplicate = VideoOptions::watch();
        duplicate.codecs.push(CodecOffer::h264(false, true));
        assert_eq!(duplicate.validate(), Err(VideoError::Negotiation));
        let mut absurd = VideoOptions::share(SendOptions::SCREEN_720P30);
        absurd.send.as_mut().unwrap().bitrate = 10;
        assert_eq!(absurd.validate(), Err(VideoError::Negotiation));
    }

    #[test]
    fn the_codec_name_round_trips_and_debug_output_hides_pixels() {
        assert_eq!(VideoCodec::from_wire("H264"), Some(VideoCodec::H264));
        assert_eq!(VideoCodec::from_wire("h264"), Some(VideoCodec::H264));
        assert_eq!(VideoCodec::from_wire("H265"), None);
        assert_eq!(VideoCodec::H264.wire_name(), "H264");
        assert_eq!(VideoCodec::from_wire("av1"), Some(VideoCodec::Av1));
        assert_eq!(VideoCodec::Av1.wire_name(), "AV1");
        let frame = ReceivedVideo {
            user_id: Snowflake(7),
            ssrc: 9,
            codec: VideoCodec::H264,
            timestamp: 3000,
            keyframe: true,
            data: vec![0xAB; 1000],
        };
        let text = format!("{frame:?}");
        assert!(text.contains("bytes: 1000") && !text.to_lowercase().contains("ab, ab"));
    }
}
