//! Discord's UDP control packets and datagram demultiplexing (SPEC §6.2).
//!
//! One UDP socket carries four kinds of traffic: IP discovery, UDP ping,
//! RTP, and RTCP. [`classify`] separates them from the clear bytes before any
//! decryption or media decoding. Discovery and ping are Discord's own formats
//! with a zero RTP version field, so they can never be mistaken for media.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use crate::rtcp;
use crate::rtp::VERSION;

/// Type (2) + length (2) + SSRC (4) + address (64) + port (2).
pub const DISCOVERY_LEN: usize = 74;
const DISCOVERY_REQUEST: u16 = 1;
const DISCOVERY_RESPONSE: u16 = 2;
/// The length field: everything after type and length.
const DISCOVERY_BODY_LEN: u16 = 70;
const ADDRESS_LEN: usize = 64;

pub const PING_LEN: usize = 8;
pub const PING_REQUEST: u32 = 0x1337_CAFE;
pub const PING_RESPONSE: u32 = 0x1337_F00D;

/// The largest datagram fastcord reads; Discord targets 1200-byte media
/// datagrams, so anything near this size is not valid traffic.
pub const MAX_DATAGRAM: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscoveryError {
    /// Not a 74-byte type-2 packet with length 70.
    Malformed,
    /// The answer is for another SSRC.
    WrongSsrc,
    /// The address is not a NUL-terminated textual IP address.
    Address,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed IP discovery response",
            Self::WrongSsrc => "IP discovery response for another SSRC",
            Self::Address => "IP discovery response with an invalid address",
        })
    }
}

impl std::error::Error for DiscoveryError {}

/// The IP discovery request for our `ssrc`.
pub fn discovery_request(ssrc: u32) -> [u8; DISCOVERY_LEN] {
    let mut packet = [0; DISCOVERY_LEN];
    packet[0..2].copy_from_slice(&DISCOVERY_REQUEST.to_be_bytes());
    packet[2..4].copy_from_slice(&DISCOVERY_BODY_LEN.to_be_bytes());
    packet[4..8].copy_from_slice(&ssrc.to_be_bytes());
    packet
}

/// Our external address from a discovery response. The caller has already
/// checked that the datagram came from the voice server's address.
pub fn parse_discovery_response(packet: &[u8], ssrc: u32) -> Result<SocketAddr, DiscoveryError> {
    let packet: &[u8; DISCOVERY_LEN] = packet.try_into().map_err(|_| DiscoveryError::Malformed)?;
    if u16::from_be_bytes([packet[0], packet[1]]) != DISCOVERY_RESPONSE
        || u16::from_be_bytes([packet[2], packet[3]]) != DISCOVERY_BODY_LEN
    {
        return Err(DiscoveryError::Malformed);
    }
    if u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]) != ssrc {
        return Err(DiscoveryError::WrongSsrc);
    }
    let field = &packet[8..8 + ADDRESS_LEN];
    let end = field
        .iter()
        .position(|&b| b == 0)
        .ok_or(DiscoveryError::Address)?;
    let ip: IpAddr = std::str::from_utf8(&field[..end])
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or(DiscoveryError::Address)?;
    let port = u16::from_be_bytes([packet[72], packet[73]]);
    if port == 0 {
        return Err(DiscoveryError::Address);
    }
    Ok(SocketAddr::new(ip, port))
}

pub fn ping_request(sequence: u32) -> [u8; PING_LEN] {
    let mut packet = [0; PING_LEN];
    packet[..4].copy_from_slice(&PING_REQUEST.to_be_bytes());
    packet[4..].copy_from_slice(&sequence.to_be_bytes());
    packet
}

/// What a received datagram is, decided from its clear bytes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Datagram {
    Discovery,
    /// A UDP ping answer with its echoed sequence.
    Ping(u32),
    Rtp,
    Rtcp,
    /// Anything else, including our own request formats echoed back.
    Unknown,
}

pub fn classify(packet: &[u8]) -> Datagram {
    if packet.len() == PING_LEN
        && u32::from_be_bytes([packet[0], packet[1], packet[2], packet[3]]) == PING_RESPONSE
    {
        return Datagram::Ping(u32::from_be_bytes([
            packet[4], packet[5], packet[6], packet[7],
        ]));
    }
    if packet.len() == DISCOVERY_LEN
        && u16::from_be_bytes([packet[0], packet[1]]) == DISCOVERY_RESPONSE
    {
        return Datagram::Discovery;
    }
    if packet.len() < 2 || packet[0] >> 6 != VERSION {
        return Datagram::Unknown;
    }
    if rtcp::is_rtcp(packet) {
        Datagram::Rtcp
    } else {
        Datagram::Rtp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(ssrc: u32, address: &[u8], port: u16) -> [u8; DISCOVERY_LEN] {
        let mut packet = discovery_request(ssrc);
        packet[1] = 2;
        packet[8..8 + address.len()].copy_from_slice(address);
        packet[72..].copy_from_slice(&port.to_be_bytes());
        packet
    }

    #[test]
    fn discovery_request_has_the_documented_layout() {
        let packet = discovery_request(0x0102_0304);
        assert_eq!(packet.len(), 74);
        assert_eq!(&packet[..8], [0, 1, 0, 70, 1, 2, 3, 4]);
        assert!(packet[8..].iter().all(|&b| b == 0));
        // Our own request echoed back is not a response.
        assert_eq!(classify(&packet), Datagram::Unknown);
    }

    #[test]
    fn discovery_responses_yield_the_external_address() {
        let packet = response(12871, b"203.0.113.7", 50_123);
        assert_eq!(classify(&packet), Datagram::Discovery);
        assert_eq!(
            parse_discovery_response(&packet, 12871),
            Ok("203.0.113.7:50123".parse().unwrap())
        );
        let v6 = response(5, b"2001:db8::1", 443);
        assert_eq!(
            parse_discovery_response(&v6, 5),
            Ok("[2001:db8::1]:443".parse().unwrap())
        );
    }

    #[test]
    fn invalid_discovery_responses_are_rejected() {
        let good = response(9, b"198.51.100.2", 4000);
        assert_eq!(
            parse_discovery_response(&good, 10),
            Err(DiscoveryError::WrongSsrc)
        );
        assert_eq!(
            parse_discovery_response(&good[..73], 9),
            Err(DiscoveryError::Malformed)
        );
        let mut request = good;
        request[1] = 1;
        assert_eq!(
            parse_discovery_response(&request, 9),
            Err(DiscoveryError::Malformed)
        );
        let mut length = good;
        length[3] = 71;
        assert_eq!(
            parse_discovery_response(&length, 9),
            Err(DiscoveryError::Malformed)
        );
        for bad in [
            response(9, b"not an address", 4000),
            response(9, b"", 4000),
            response(9, &[0xFF, 0xFE, b'1'], 4000),
            response(9, b"198.51.100.2", 0),
            // 64 bytes with no terminator.
            response(9, &[b'1'; 64], 4000),
        ] {
            assert_eq!(
                parse_discovery_response(&bad, 9),
                Err(DiscoveryError::Address)
            );
        }
    }

    #[test]
    fn datagrams_are_demultiplexed_before_decryption() {
        assert_eq!(ping_request(7), [0x13, 0x37, 0xCA, 0xFE, 0, 0, 0, 7]);
        assert_eq!(
            classify(&[0x13, 0x37, 0xF0, 0x0D, 0, 0, 1, 0]),
            Datagram::Ping(256)
        );
        // Our own ping request is not an answer.
        assert_eq!(classify(&ping_request(7)), Datagram::Unknown);
        assert_eq!(
            classify(&[0x80, 0x78, 0, 1, 0, 0, 3, 0xC0, 0, 0, 0, 1]),
            Datagram::Rtp
        );
        assert_eq!(classify(&[0x81, 201, 0, 1, 0, 0, 0, 1]), Datagram::Rtcp);
        assert_eq!(classify(&[]), Datagram::Unknown);
        assert_eq!(classify(&[0x80]), Datagram::Unknown);
        assert_eq!(classify(&[0x40, 0x78, 0, 0]), Datagram::Unknown);
    }
}
