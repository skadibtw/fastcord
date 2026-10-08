//! RTP (RFC 3550) headers with the boundaries Discord's rtpsize transport
//! modes rely on (SPEC §6.2).
//!
//! An rtpsize packet leaves the fixed header, the CSRC list, and the 4-byte
//! extension preamble in the clear; the extension elements, payload, and
//! padding are encrypted. Parsing is therefore split in two: [`RtpHeader::parse`]
//! reads the clear part of a received datagram (before decryption), and
//! [`RtpHeader::split_body`] splits the decrypted remainder. Neither assumes a
//! 12-byte header, and every length is checked against the buffer, so malformed
//! input is an error rather than a panic or an out-of-bounds read.

use std::fmt;

pub const VERSION: u8 = 2;
pub const FIXED_HEADER_LEN: usize = 12;
/// The documented default Opus payload type on Discord's UDP transport.
pub const OPUS_PAYLOAD_TYPE: u8 = 120;
/// 20 ms of Opus at the 48 kHz RTP clock.
pub const OPUS_FRAME_TICKS: u32 = 960;
/// RFC 8285 one-byte header extension profile.
pub const ONE_BYTE_PROFILE: u16 = 0xBEDE;
/// RFC 8285 two-byte header extension profile (the low 4 bits are app bits).
pub const TWO_BYTE_PROFILE_MASK: u16 = 0xFFF0;
pub const TWO_BYTE_PROFILE: u16 = 0x1000;

const EXTENSION_PREAMBLE_LEN: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtpError {
    /// Shorter than the header it announces.
    Truncated,
    /// Not RTP version 2.
    Version(u8),
    /// The extension block is longer than the decrypted body.
    ExtensionOverrun,
    /// Padding announced but the count is zero or longer than the body.
    Padding,
    /// A header extension element runs past its block or uses a reserved ID.
    ExtensionElement,
}

impl fmt::Display for RtpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("RTP packet shorter than its header"),
            Self::Version(v) => write!(f, "unsupported RTP version {v}"),
            Self::ExtensionOverrun => f.write_str("RTP header extension exceeds the packet"),
            Self::Padding => f.write_str("invalid RTP padding"),
            Self::ExtensionElement => f.write_str("malformed RTP header extension element"),
        }
    }
}

impl std::error::Error for RtpError {}

/// The 4-byte extension preamble: profile and length in 32-bit words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionPreamble {
    pub profile: u16,
    pub words: u16,
}

impl ExtensionPreamble {
    pub const fn body_len(self) -> usize {
        self.words as usize * 4
    }
}

/// The clear part of an RTP packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpHeader {
    pub padding: bool,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub csrc_count: u8,
    pub extension: Option<ExtensionPreamble>,
}

impl RtpHeader {
    /// A header with no CSRCs, padding, or extension.
    pub const fn new(payload_type: u8, sequence: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            padding: false,
            marker: false,
            payload_type,
            sequence,
            timestamp,
            ssrc,
            csrc_count: 0,
            extension: None,
        }
    }

    /// Parses the clear header at the start of `packet`. The returned header's
    /// [`clear_len`](Self::clear_len) is where the encrypted body starts.
    pub fn parse(packet: &[u8]) -> Result<Self, RtpError> {
        if packet.len() < FIXED_HEADER_LEN {
            return Err(RtpError::Truncated);
        }
        let version = packet[0] >> 6;
        if version != VERSION {
            return Err(RtpError::Version(version));
        }
        let header = Self {
            padding: packet[0] & 0x20 != 0,
            marker: packet[1] & 0x80 != 0,
            payload_type: packet[1] & 0x7F,
            sequence: u16::from_be_bytes([packet[2], packet[3]]),
            timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
            csrc_count: packet[0] & 0x0F,
            extension: None,
        };
        let csrc_end = FIXED_HEADER_LEN + header.csrc_count as usize * 4;
        if packet[0] & 0x10 == 0 {
            return if packet.len() < csrc_end {
                Err(RtpError::Truncated)
            } else {
                Ok(header)
            };
        }
        let Some(preamble) = packet.get(csrc_end..csrc_end + EXTENSION_PREAMBLE_LEN) else {
            return Err(RtpError::Truncated);
        };
        Ok(Self {
            extension: Some(ExtensionPreamble {
                profile: u16::from_be_bytes([preamble[0], preamble[1]]),
                words: u16::from_be_bytes([preamble[2], preamble[3]]),
            }),
            ..header
        })
    }

    /// Length of the clear (authenticated, unencrypted) header in rtpsize
    /// modes: fixed header, CSRCs, and the extension preamble.
    pub const fn clear_len(&self) -> usize {
        let preamble = if self.extension.is_some() {
            EXTENSION_PREAMBLE_LEN
        } else {
            0
        };
        FIXED_HEADER_LEN + self.csrc_count as usize * 4 + preamble
    }

    /// The CSRC list of a parsed `packet`.
    pub fn csrcs<'a>(&self, packet: &'a [u8]) -> impl Iterator<Item = u32> + 'a {
        let end = (FIXED_HEADER_LEN + self.csrc_count as usize * 4).min(packet.len());
        packet
            .get(FIXED_HEADER_LEN..end)
            .unwrap_or_default()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_be_bytes(*c))
    }

    /// Splits a decrypted body (everything after the clear header) into the
    /// extension elements and the payload, removing padding.
    pub fn split_body<'a>(&self, body: &'a [u8]) -> Result<RtpBody<'a>, RtpError> {
        let extension_len = self.extension.map_or(0, ExtensionPreamble::body_len);
        if body.len() < extension_len {
            return Err(RtpError::ExtensionOverrun);
        }
        let (extensions, mut payload) = body.split_at(extension_len);
        if self.padding {
            let count = *payload.last().ok_or(RtpError::Padding)? as usize;
            if count == 0 || count > payload.len() {
                return Err(RtpError::Padding);
            }
            payload = &payload[..payload.len() - count];
        }
        Ok(RtpBody {
            extensions: Extensions {
                profile: self.extension.map_or(0, |e| e.profile),
                data: extensions,
            },
            payload,
        })
    }

    /// Writes the clear header of an outgoing packet. fastcord never mixes
    /// streams, so no CSRC list is written and the count is always zero.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut first = VERSION << 6;
        if self.padding {
            first |= 0x20;
        }
        if self.extension.is_some() {
            first |= 0x10;
        }
        out.push(first);
        out.push(u8::from(self.marker) << 7 | (self.payload_type & 0x7F));
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        if let Some(extension) = self.extension {
            out.extend_from_slice(&extension.profile.to_be_bytes());
            out.extend_from_slice(&extension.words.to_be_bytes());
        }
    }
}

/// The decrypted part of an RTP packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpBody<'a> {
    pub extensions: Extensions<'a>,
    pub payload: &'a [u8],
}

/// RFC 8285 header extension elements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extensions<'a> {
    profile: u16,
    data: &'a [u8],
}

/// One header extension element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionElement<'a> {
    pub id: u8,
    pub data: &'a [u8],
}

impl<'a> Extensions<'a> {
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Iterates elements of the one-byte or two-byte profile. Unknown
    /// profiles yield nothing; a malformed element yields one error and ends.
    pub fn elements(&self) -> ExtensionIter<'a> {
        let format = if self.profile == ONE_BYTE_PROFILE {
            Some(false)
        } else if self.profile & TWO_BYTE_PROFILE_MASK == TWO_BYTE_PROFILE {
            Some(true)
        } else {
            None
        };
        ExtensionIter {
            data: if format.is_some() { self.data } else { &[] },
            two_byte: format.unwrap_or(false),
        }
    }

    /// The first element with `id`, ignoring a malformed tail after it.
    pub fn find(&self, id: u8) -> Option<&'a [u8]> {
        self.elements()
            .map_while(Result::ok)
            .find(|e| e.id == id)
            .map(|e| e.data)
    }
}

pub struct ExtensionIter<'a> {
    data: &'a [u8],
    two_byte: bool,
}

impl<'a> Iterator for ExtensionIter<'a> {
    type Item = Result<ExtensionElement<'a>, RtpError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (&first, rest) = self.data.split_first()?;
            // A zero byte is padding between elements in both profiles.
            if first == 0 {
                self.data = rest;
                continue;
            }
            let (id, len, rest) = if self.two_byte {
                let Some((&len, rest)) = rest.split_first() else {
                    self.data = &[];
                    return Some(Err(RtpError::ExtensionElement));
                };
                (first, len as usize, rest)
            } else {
                let id = first >> 4;
                // ID 15 is reserved: stop processing (RFC 8285 §4.2).
                if id == 15 {
                    self.data = &[];
                    return None;
                }
                (id, (first & 0x0F) as usize + 1, rest)
            };
            if rest.len() < len {
                self.data = &[];
                return Some(Err(RtpError::ExtensionElement));
            }
            let (data, rest) = rest.split_at(len);
            self.data = rest;
            return Some(Ok(ExtensionElement { id, data }));
        }
    }
}

/// Builds headers for one outgoing RTP stream: 16-bit sequence numbers and
/// 32-bit timestamps that wrap as RFC 3550 intends. Wrapping here is normal and
/// unrelated to the transport nonce, which must never wrap.
#[derive(Clone, Debug)]
pub struct RtpSender {
    ssrc: u32,
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
}

impl RtpSender {
    /// `sequence`/`timestamp` are the first values to send; RFC 3550 asks for
    /// random initial values.
    pub const fn new(ssrc: u32, payload_type: u8, sequence: u16, timestamp: u32) -> Self {
        Self {
            ssrc,
            payload_type,
            sequence,
            timestamp,
        }
    }

    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The RTP timestamp the next packet will carry.
    pub const fn next_timestamp(&self) -> u32 {
        self.timestamp
    }

    /// The header for the next packet, advancing the stream by `ticks` of the
    /// media clock (960 for a 20 ms Opus frame).
    pub fn next_header(&mut self, ticks: u32) -> RtpHeader {
        let header = RtpHeader::new(self.payload_type, self.sequence, self.timestamp, self.ssrc);
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(ticks);
        header
    }
}

/// Extends received 16-bit sequence numbers across wraps (RFC 3550 A.1), so
/// reordering around 65535 -> 0 is ordered correctly instead of looking like a
/// jump backwards of 65535 packets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SequenceTracker {
    highest: Option<u64>,
}

impl SequenceTracker {
    /// The extended (wrap-counted) value of `sequence`, interpreted as the one
    /// nearest the highest sequence seen so far.
    pub fn extend(&mut self, sequence: u16) -> u64 {
        let Some(highest) = self.highest else {
            // Start one cycle in, so a packet from before the first wrap can
            // still be placed below it.
            let extended = (1 << 16) | u64::from(sequence);
            self.highest = Some(extended);
            return extended;
        };
        let delta = sequence.wrapping_sub(highest as u16) as i16;
        let extended = highest.saturating_add_signed(i64::from(delta));
        if extended > highest {
            self.highest = Some(extended);
        }
        extended
    }

    pub const fn highest(&self) -> Option<u64> {
        self.highest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn minimal_header_parses_and_round_trips() {
        let packet = hex("80780001000003c012345678f8fffe");
        let header = RtpHeader::parse(&packet).unwrap();
        assert_eq!(header, RtpHeader::new(120, 1, 960, 0x1234_5678));
        assert_eq!(header.clear_len(), 12);
        let body = header.split_body(&packet[12..]).unwrap();
        assert!(body.extensions.is_empty());
        assert_eq!(body.payload, [0xf8, 0xff, 0xfe]);
        let mut out = Vec::new();
        header.write(&mut out);
        assert_eq!(out, packet[..12]);
    }

    #[test]
    fn csrcs_and_extension_preamble_are_part_of_the_clear_header() {
        let packet = hex(concat!(
            "92f8ffffffffffff0badf00d",
            "1111111122222222",
            "bede0002",
            "1085900200000000",
            "0102",
        ));
        let header = RtpHeader::parse(&packet).unwrap();
        assert!(header.marker);
        assert_eq!(header.payload_type, 120);
        assert_eq!(header.sequence, 0xFFFF);
        assert_eq!(header.timestamp, 0xFFFF_FFFF);
        assert_eq!(header.csrc_count, 2);
        assert_eq!(
            header.csrcs(&packet).collect::<Vec<_>>(),
            [0x1111_1111, 0x2222_2222]
        );
        assert_eq!(
            header.extension,
            Some(ExtensionPreamble {
                profile: ONE_BYTE_PROFILE,
                words: 2
            })
        );
        assert_eq!(header.clear_len(), 12 + 8 + 4);
        let body = header.split_body(&packet[header.clear_len()..]).unwrap();
        let elements: Vec<_> = body.extensions.elements().collect();
        assert_eq!(
            elements,
            [
                Ok(ExtensionElement {
                    id: 1,
                    data: &[0x85]
                }),
                Ok(ExtensionElement {
                    id: 9,
                    data: &[0x02]
                }),
            ]
        );
        assert_eq!(body.extensions.find(9), Some(&[0x02][..]));
        assert_eq!(body.payload, [1, 2]);

        let mut out = Vec::new();
        RtpHeader {
            csrc_count: 0,
            ..header
        }
        .write(&mut out);
        assert_eq!(out[0], 0x90);
        assert_eq!(&out[12..], &hex("bede0002")[..]);
    }

    #[test]
    fn two_byte_extensions_and_unknown_profiles() {
        let header = RtpHeader {
            extension: Some(ExtensionPreamble {
                profile: 0x1000,
                words: 2,
            }),
            ..RtpHeader::new(120, 0, 0, 1)
        };
        // id 3 with zero length, padding, id 7 with 3 bytes.
        let body = hex("030000070301020300");
        let split = header.split_body(&body).unwrap();
        let elements: Vec<_> = split.extensions.elements().map(Result::unwrap).collect();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0], ExtensionElement { id: 3, data: &[] });
        assert_eq!(
            elements[1],
            ExtensionElement {
                id: 7,
                data: &[1, 2, 3]
            }
        );
        assert_eq!(split.payload, [0]);

        let unknown = RtpHeader {
            extension: Some(ExtensionPreamble {
                profile: 0xABCD,
                words: 1,
            }),
            ..header
        };
        let body = hex("11223344");
        let split = unknown.split_body(&body).unwrap();
        assert_eq!(split.extensions.elements().count(), 0);
    }

    #[test]
    fn padding_is_removed_and_bad_padding_rejected() {
        let header = RtpHeader {
            padding: true,
            ..RtpHeader::new(120, 0, 0, 1)
        };
        assert_eq!(
            header.split_body(&hex("0102030405000003")).unwrap().payload,
            [1, 2, 3, 4, 5]
        );
        // All padding is a valid empty payload.
        assert_eq!(
            header.split_body(&hex("0002")).unwrap().payload,
            [] as [u8; 0]
        );
        assert_eq!(header.split_body(&hex("010200")), Err(RtpError::Padding));
        assert_eq!(header.split_body(&hex("0105")), Err(RtpError::Padding));
        assert_eq!(header.split_body(&[]), Err(RtpError::Padding));
    }

    #[test]
    fn malformed_headers_are_rejected_without_panicking() {
        assert_eq!(RtpHeader::parse(&[]), Err(RtpError::Truncated));
        assert_eq!(
            RtpHeader::parse(&hex("807800010000")),
            Err(RtpError::Truncated)
        );
        assert_eq!(
            RtpHeader::parse(&hex("407800010000000000000001")),
            Err(RtpError::Version(1))
        );
        // CSRC count 15 but no CSRCs.
        assert_eq!(
            RtpHeader::parse(&hex("8f7800010000000000000001")),
            Err(RtpError::Truncated)
        );
        // Extension bit without a preamble.
        assert_eq!(
            RtpHeader::parse(&hex("907800010000000000000001be")),
            Err(RtpError::Truncated)
        );
        // Preamble announcing 0xFFFF words with a short body.
        let header = RtpHeader::parse(&hex("907800010000000000000001beedffff")).unwrap();
        assert_eq!(header.split_body(&[0; 8]), Err(RtpError::ExtensionOverrun));
        // An element running past its block.
        let header = RtpHeader::parse(&hex("907800010000000000000001bede0001")).unwrap();
        let overrun = hex("1f000000");
        let body = header.split_body(&overrun).unwrap();
        assert_eq!(
            body.extensions.elements().collect::<Vec<_>>(),
            [Err(RtpError::ExtensionElement)]
        );
        // ID 15 stops processing.
        let reserved = hex("f0001000");
        let body = header.split_body(&reserved).unwrap();
        assert_eq!(body.extensions.elements().count(), 0);
    }

    #[test]
    fn every_truncation_and_bit_flip_of_a_full_packet_is_handled() {
        let packet = hex(concat!(
            "b2f8ffffffffffff0badf00d",
            "1111111122222222",
            "bede0002",
            "1085900200000000",
            "0102030402",
        ));
        for len in 0..=packet.len() {
            if let Ok(header) = RtpHeader::parse(&packet[..len]) {
                let body = packet[..len].get(header.clear_len()..).unwrap_or_default();
                if let Ok(split) = header.split_body(body) {
                    for element in split.extensions.elements() {
                        let _ = element;
                    }
                }
            }
        }
        for byte in 0..packet.len() {
            for bit in 0..8 {
                let mut corrupt = packet.clone();
                corrupt[byte] ^= 1 << bit;
                if let Ok(header) = RtpHeader::parse(&corrupt)
                    && let Some(body) = corrupt.get(header.clear_len()..)
                {
                    let _ = header
                        .split_body(body)
                        .map(|b| b.extensions.elements().count());
                }
            }
        }
    }

    #[test]
    fn sender_sequence_and_timestamp_wrap_independently() {
        let mut sender = RtpSender::new(7, OPUS_PAYLOAD_TYPE, 0xFFFE, u32::MAX - 959);
        let headers: Vec<_> = (0..3)
            .map(|_| sender.next_header(OPUS_FRAME_TICKS))
            .collect();
        assert_eq!(
            headers.iter().map(|h| h.sequence).collect::<Vec<_>>(),
            [0xFFFE, 0xFFFF, 0]
        );
        assert_eq!(
            headers.iter().map(|h| h.timestamp).collect::<Vec<_>>(),
            [u32::MAX - 959, 0, 960]
        );
        assert!(headers.iter().all(|h| h.ssrc == 7 && h.payload_type == 120));
    }

    #[test]
    fn sequence_tracker_orders_reordering_across_the_wrap() {
        let mut tracker = SequenceTracker::default();
        let first = tracker.extend(65_534);
        let wrapped = tracker.extend(1);
        let late = tracker.extend(65_535);
        let next = tracker.extend(2);
        assert_eq!(wrapped - first, 3);
        assert_eq!(late, first + 1);
        assert_eq!(next, wrapped + 1);
        assert_eq!(tracker.highest(), Some(next));
        // A packet from before the very first one stays below it.
        let mut tracker = SequenceTracker::default();
        let start = tracker.extend(0);
        assert_eq!(tracker.extend(65_535), start - 1);
        assert_eq!(tracker.highest(), Some(start));
    }
}
