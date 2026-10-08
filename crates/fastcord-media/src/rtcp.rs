//! RTCP (RFC 3550 §6, RFC 4585) with its own header/body boundaries.
//!
//! RTCP is not RTP: its 4-byte common header carries a report count or
//! feedback format, a packet type, and the packet length in 32-bit words minus
//! one, and several packets may be stacked in one compound datagram. On
//! Discord's rtpsize transport the first 8 bytes (common header and sender
//! SSRC, [`RTCP_CLEAR_LEN`](crate::crypto::RTCP_CLEAR_LEN)) stay clear and the
//! rest of the compound is encrypted; parsing happens on the reassembled
//! plaintext. Every length is checked, so a malformed datagram is an error,
//! never a panic or an out-of-bounds read.

use std::fmt;

use crate::rtp::VERSION;

pub const HEADER_LEN: usize = 4;
pub const SENDER_REPORT: u8 = 200;
pub const RECEIVER_REPORT: u8 = 201;
pub const SOURCE_DESCRIPTION: u8 = 202;
pub const GOODBYE: u8 = 203;
pub const TRANSPORT_FEEDBACK: u8 = 205;
pub const PAYLOAD_FEEDBACK: u8 = 206;
/// RTPFB format 1 (RFC 4585 §6.2.1).
pub const GENERIC_NACK: u8 = 1;
/// PSFB format 1 (RFC 4585 §6.3.1).
pub const PICTURE_LOSS: u8 = 1;

const REPORT_BLOCK_LEN: usize = 24;
const SENDER_INFO_LEN: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcpError {
    /// Shorter than the header or the length it announces.
    Truncated,
    /// Not version 2.
    Version(u8),
    /// The report count needs more bytes than the packet has.
    ReportOverrun,
    /// Padding is malformed, or present on a packet that is not the last.
    Padding,
    /// A feedback packet without its two SSRCs.
    Feedback,
}

impl fmt::Display for RtcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("RTCP packet shorter than its header"),
            Self::Version(v) => write!(f, "unsupported RTCP version {v}"),
            Self::ReportOverrun => f.write_str("RTCP report count exceeds the packet"),
            Self::Padding => f.write_str("invalid RTCP padding"),
            Self::Feedback => f.write_str("malformed RTCP feedback packet"),
        }
    }
}

impl std::error::Error for RtcpError {}

/// Whether a version-2 datagram is RTCP rather than RTP: RTCP packet types
/// occupy 192–223 (RFC 5761 §4), which no RTP payload type Discord uses can
/// produce, with or without the marker bit.
pub fn is_rtcp(packet: &[u8]) -> bool {
    packet.len() >= HEADER_LEN && packet[0] >> 6 == VERSION && (192..=223).contains(&packet[1])
}

/// The packets of one compound RTCP datagram, in order. A malformed packet
/// yields one error and ends the iteration.
#[derive(Clone, Debug)]
pub struct Compound<'a> {
    rest: &'a [u8],
}

impl<'a> Compound<'a> {
    pub const fn new(datagram: &'a [u8]) -> Self {
        Self { rest: datagram }
    }
}

impl<'a> Iterator for Compound<'a> {
    type Item = Result<RtcpPacket<'a>, RtcpError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let result = split_packet(self.rest);
        match result {
            Ok((packet, rest)) => {
                self.rest = rest;
                Some(Ok(packet))
            }
            Err(error) => {
                self.rest = &[];
                Some(Err(error))
            }
        }
    }
}

fn split_packet(data: &[u8]) -> Result<(RtcpPacket<'_>, &[u8]), RtcpError> {
    if data.len() < HEADER_LEN {
        return Err(RtcpError::Truncated);
    }
    let version = data[0] >> 6;
    if version != VERSION {
        return Err(RtcpError::Version(version));
    }
    let padded = data[0] & 0x20 != 0;
    let count = data[0] & 0x1F;
    let packet_type = data[1];
    let len = (usize::from(u16::from_be_bytes([data[2], data[3]])) + 1) * 4;
    let Some((packet, rest)) = data.split_at_checked(len) else {
        return Err(RtcpError::Truncated);
    };
    let mut body = &packet[HEADER_LEN..];
    if padded {
        // Only the last packet of a compound may be padded (RFC 3550 §6.4.1).
        if !rest.is_empty() {
            return Err(RtcpError::Padding);
        }
        let pad = usize::from(*body.last().ok_or(RtcpError::Padding)?);
        if pad == 0 || pad > body.len() {
            return Err(RtcpError::Padding);
        }
        body = &body[..body.len() - pad];
    }
    let parsed = match packet_type {
        SENDER_REPORT => {
            let (ssrc, rest) = read_u32(body).ok_or(RtcpError::Truncated)?;
            let info = rest.get(..SENDER_INFO_LEN).ok_or(RtcpError::Truncated)?;
            let info = SenderInfo {
                ntp_timestamp: u64::from_be_bytes(info[..8].try_into().unwrap_or_default()),
                rtp_timestamp: be_u32(&info[8..12]),
                packet_count: be_u32(&info[12..16]),
                octet_count: be_u32(&info[16..20]),
            };
            RtcpPacket::SenderReport(SenderReport {
                ssrc,
                info,
                reports: report_blocks(&rest[SENDER_INFO_LEN..], count)?,
            })
        }
        RECEIVER_REPORT => {
            let (ssrc, rest) = read_u32(body).ok_or(RtcpError::Truncated)?;
            RtcpPacket::ReceiverReport(ReceiverReport {
                ssrc,
                reports: report_blocks(rest, count)?,
            })
        }
        TRANSPORT_FEEDBACK | PAYLOAD_FEEDBACK => {
            let (sender_ssrc, rest) = read_u32(body).ok_or(RtcpError::Feedback)?;
            let (media_ssrc, fci) = read_u32(rest).ok_or(RtcpError::Feedback)?;
            RtcpPacket::Feedback(Feedback {
                kind: if packet_type == TRANSPORT_FEEDBACK {
                    FeedbackKind::Transport
                } else {
                    FeedbackKind::PayloadSpecific
                },
                format: count,
                sender_ssrc,
                media_ssrc,
                fci,
            })
        }
        _ => RtcpPacket::Other {
            packet_type,
            count,
            body,
        },
    };
    Ok((parsed, rest))
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u32(data: &[u8]) -> Option<(u32, &[u8])> {
    let (value, rest) = data.split_at_checked(4)?;
    Some((be_u32(value), rest))
}

fn report_blocks(data: &[u8], count: u8) -> Result<ReportBlocks<'_>, RtcpError> {
    // Anything after the blocks is a profile-specific extension; ignored.
    data.get(..usize::from(count) * REPORT_BLOCK_LEN)
        .map(ReportBlocks)
        .ok_or(RtcpError::ReportOverrun)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcpPacket<'a> {
    SenderReport(SenderReport<'a>),
    ReceiverReport(ReceiverReport<'a>),
    Feedback(Feedback<'a>),
    /// SDES, BYE, APP, and anything newer: header fields and the unpadded body.
    Other {
        packet_type: u8,
        count: u8,
        body: &'a [u8],
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SenderInfo {
    /// 64-bit NTP time (seconds since 1900 in the high half).
    pub ntp_timestamp: u64,
    pub rtp_timestamp: u32,
    pub packet_count: u32,
    pub octet_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SenderReport<'a> {
    pub ssrc: u32,
    pub info: SenderInfo,
    pub reports: ReportBlocks<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverReport<'a> {
    pub ssrc: u32,
    pub reports: ReportBlocks<'a>,
}

/// Reception report blocks, already checked to fit the packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportBlocks<'a>(&'a [u8]);

impl ReportBlocks<'_> {
    pub fn len(&self) -> usize {
        self.0.len() / REPORT_BLOCK_LEN
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = ReportBlock> + '_ {
        let (blocks, _) = self.0.as_chunks::<REPORT_BLOCK_LEN>();
        blocks.iter().map(|b| ReportBlock {
            ssrc: be_u32(&b[0..4]),
            fraction_lost: b[4],
            // 24-bit two's complement: duplicates can make it negative.
            cumulative_lost: i32::from_be_bytes([b[5], b[6], b[7], 0]) >> 8,
            highest_sequence: be_u32(&b[8..12]),
            jitter: be_u32(&b[12..16]),
            last_sender_report: be_u32(&b[16..20]),
            delay_since_last_sender_report: be_u32(&b[20..24]),
        })
    }
}

/// One reception report (RFC 3550 §6.4.1) about a source we send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportBlock {
    pub ssrc: u32,
    /// Fraction of packets lost since the previous report, in 1/256 units.
    pub fraction_lost: u8,
    pub cumulative_lost: i32,
    /// Extended highest sequence number received (cycles in the high half).
    pub highest_sequence: u32,
    /// Interarrival jitter in RTP timestamp units.
    pub jitter: u32,
    pub last_sender_report: u32,
    /// In units of 1/65536 s.
    pub delay_since_last_sender_report: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackKind {
    /// RTPFB (205).
    Transport,
    /// PSFB (206).
    PayloadSpecific,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feedback<'a> {
    pub kind: FeedbackKind,
    pub format: u8,
    pub sender_ssrc: u32,
    pub media_ssrc: u32,
    /// Feedback control information, format-specific.
    pub fci: &'a [u8],
}

impl Feedback<'_> {
    /// Sequence numbers a Generic NACK asks for (packet ID plus the 16-bit
    /// bitmask of following losses); nothing for other feedback or a trailing
    /// partial entry.
    pub fn nacked_sequences(&self) -> impl Iterator<Item = u16> + '_ {
        let entries = if self.kind == FeedbackKind::Transport && self.format == GENERIC_NACK {
            self.fci
        } else {
            &[]
        };
        let (entries, _) = entries.as_chunks::<4>();
        entries.iter().flat_map(|entry| {
            let pid = u16::from_be_bytes([entry[0], entry[1]]);
            let mask = u16::from_be_bytes([entry[2], entry[3]]);
            std::iter::once(pid).chain(
                (0..16u16)
                    .filter(move |bit| mask & (1 << bit) != 0)
                    .map(move |bit| pid.wrapping_add(bit + 1)),
            )
        })
    }

    pub fn is_picture_loss(&self) -> bool {
        self.kind == FeedbackKind::PayloadSpecific && self.format == PICTURE_LOSS
    }
}

/// Writes a Sender Report without report blocks (fastcord reports reception
/// only when the server asks for it) into `out`, cleared first.
pub fn write_sender_report(ssrc: u32, info: SenderInfo, out: &mut Vec<u8>) {
    out.clear();
    let words = (HEADER_LEN + 4 + SENDER_INFO_LEN) / 4 - 1;
    out.push(VERSION << 6);
    out.push(SENDER_REPORT);
    out.extend_from_slice(&(words as u16).to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(&info.ntp_timestamp.to_be_bytes());
    out.extend_from_slice(&info.rtp_timestamp.to_be_bytes());
    out.extend_from_slice(&info.packet_count.to_be_bytes());
    out.extend_from_slice(&info.octet_count.to_be_bytes());
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
    fn receiver_report_blocks_decode_including_negative_loss() {
        let packet = hex(concat!(
            "81c90007deadbeef",
            "0badf00d0a00002a0001ffff000000101122334400010000",
        ));
        assert!(is_rtcp(&packet));
        let packets: Vec<_> = Compound::new(&packet).collect();
        let [Ok(RtcpPacket::ReceiverReport(report))] = packets.as_slice() else {
            panic!("{packets:?}");
        };
        assert_eq!(report.ssrc, 0xDEAD_BEEF);
        assert_eq!(report.reports.len(), 1);
        let block = report.reports.iter().next().unwrap();
        assert_eq!(
            block,
            ReportBlock {
                ssrc: 0x0BAD_F00D,
                fraction_lost: 10,
                cumulative_lost: 42,
                highest_sequence: 0x0001_FFFF,
                jitter: 16,
                last_sender_report: 0x1122_3344,
                delay_since_last_sender_report: 0x0001_0000,
            }
        );

        let mut negative = packet.clone();
        negative[13..16].copy_from_slice(&[0xFF, 0xFF, 0xFE]);
        let Some(Ok(RtcpPacket::ReceiverReport(report))) = Compound::new(&negative).next() else {
            panic!();
        };
        assert_eq!(report.reports.iter().next().unwrap().cumulative_lost, -2);
    }

    #[test]
    fn sender_reports_round_trip_and_compounds_split_on_length() {
        let info = SenderInfo {
            ntp_timestamp: 0xE6A1_0000_8000_0000,
            rtp_timestamp: 48_000,
            packet_count: 50,
            octet_count: 3_000,
        };
        let mut datagram = Vec::new();
        write_sender_report(0x0102_0304, info, &mut datagram);
        assert_eq!(datagram.len(), 28);
        assert_eq!(&datagram[..4], [0x80, 200, 0, 6]);
        // Append an SDES (one chunk, END item) and a BYE with four bytes of
        // padding to make a compound.
        datagram.extend_from_slice(&hex("81ca00020102030400000000"));
        datagram.extend_from_slice(&hex("a1cb00020102030400000004"));
        let packets: Vec<_> = Compound::new(&datagram).collect::<Result<_, _>>().unwrap();
        assert_eq!(packets.len(), 3, "{packets:?}");
        let RtcpPacket::SenderReport(report) = packets[0] else {
            panic!();
        };
        assert_eq!(report.ssrc, 0x0102_0304);
        assert_eq!(report.info, info);
        assert!(report.reports.is_empty());
        assert!(matches!(
            packets[1],
            RtcpPacket::Other {
                packet_type: SOURCE_DESCRIPTION,
                count: 1,
                ..
            }
        ));
        let RtcpPacket::Other {
            packet_type: GOODBYE,
            count: 1,
            body,
        } = packets[2]
        else {
            panic!();
        };
        // Four bytes of padding removed: just the SSRC remains.
        assert_eq!(body, [1, 2, 3, 4]);
    }

    #[test]
    fn generic_nack_expands_its_bitmask_across_the_sequence_wrap() {
        let packet = hex("81cd0004deadbeef0badf00dfffe0005000a0000");
        let Some(Ok(RtcpPacket::Feedback(feedback))) = Compound::new(&packet).next() else {
            panic!();
        };
        assert_eq!(feedback.kind, FeedbackKind::Transport);
        assert_eq!(feedback.media_ssrc, 0x0BAD_F00D);
        assert!(!feedback.is_picture_loss());
        assert_eq!(
            feedback.nacked_sequences().collect::<Vec<_>>(),
            [0xFFFE, 0xFFFF, 1, 10]
        );

        let pli = hex("81ce0002deadbeef0badf00d");
        let Some(Ok(RtcpPacket::Feedback(feedback))) = Compound::new(&pli).next() else {
            panic!();
        };
        assert!(feedback.is_picture_loss());
        assert_eq!(feedback.nacked_sequences().count(), 0);
    }

    #[test]
    fn malformed_compounds_are_rejected_without_panicking() {
        let cases: &[(&str, RtcpError)] = &[
            ("81c9", RtcpError::Truncated),
            // Length says 8 words, only 2 present.
            ("81c90007deadbeef", RtcpError::Truncated),
            ("41c90001deadbeef", RtcpError::Version(1)),
            // Two report blocks announced, none present.
            ("82c90001deadbeef", RtcpError::ReportOverrun),
            // SR without sender info.
            ("80c80001deadbeef", RtcpError::Truncated),
            // Padding count larger than the body.
            ("a0c90001deadbe09", RtcpError::Padding),
            ("a0c90001deadbe00", RtcpError::Padding),
            // Feedback missing the media SSRC.
            ("81cd0001deadbeef", RtcpError::Feedback),
        ];
        for (input, error) in cases {
            let packet = hex(input);
            assert_eq!(
                Compound::new(&packet).collect::<Vec<_>>(),
                [Err(*error)],
                "{input}"
            );
        }
        // Padding on a packet that is not last.
        let packet = hex("a0c90001deadbe0480c90001deadbeef");
        assert_eq!(
            Compound::new(&packet).collect::<Vec<_>>(),
            [Err(RtcpError::Padding)]
        );
        // A valid packet followed by garbage yields the packet, then one error.
        let packet = hex("80c90001deadbeef01");
        let items: Vec<_> = Compound::new(&packet).collect();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert_eq!(items[1], Err(RtcpError::Truncated));

        // Every truncation of a valid compound is handled.
        let mut full = Vec::new();
        write_sender_report(
            7,
            SenderInfo {
                ntp_timestamp: 1,
                rtp_timestamp: 2,
                packet_count: 3,
                octet_count: 4,
            },
            &mut full,
        );
        for len in 0..full.len() {
            let _ = Compound::new(&full[..len]).count();
        }
    }

    #[test]
    fn rtp_and_control_traffic_are_not_mistaken_for_rtcp() {
        // Opus RTP, with and without the marker bit.
        assert!(!is_rtcp(&hex("80780001000003c012345678")));
        assert!(!is_rtcp(&hex("80f80001000003c012345678")));
        // Video payload types with marker (101 | 0x80 = 229).
        assert!(!is_rtcp(&hex("80e50001000003c012345678")));
        // IP discovery and UDP ping are version 0.
        assert!(!is_rtcp(&hex("00020046")));
        assert!(!is_rtcp(&hex("1337f00d00000001")));
        assert!(is_rtcp(&hex("80c8000600000000")));
        assert!(is_rtcp(&hex("81ce0002")));
    }
}
