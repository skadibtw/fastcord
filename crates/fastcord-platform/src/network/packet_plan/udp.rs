//! Existing discovery-only UDP profile layouts; checksum completion on the
//! original and fake packets stays with the native caller.
use super::{io, unavailable};

const QUIC: &[u8; 1200] = include_bytes!("../../../native/zapret2/quic_initial_www_google_com.bin");
const REPEATS: usize = 6;

pub(in crate::network) fn headers(packet: &[u8]) -> Option<(usize, usize)> {
    let (udp, length) = match packet.first()? >> 4 {
        4 if packet.get(9) == Some(&17)
            && u16::from_be_bytes(packet.get(6..8)?.try_into().ok()?) & 0x3fff == 0 =>
        {
            let ihl = usize::from(packet[0] & 15) * 4;
            if ihl < 20 || ihl > packet.len() {
                return None;
            }
            (
                ihl,
                usize::from(u16::from_be_bytes(packet.get(2..4)?.try_into().ok()?)),
            )
        }
        6 if packet.get(6) == Some(&17) => (
            40,
            40 + usize::from(u16::from_be_bytes(packet.get(4..6)?.try_into().ok()?)),
        ),
        _ => return None,
    };
    if packet.len() != length
        || udp + 8 > length
        || usize::from(u16::from_be_bytes(
            packet.get(udp + 4..udp + 6)?.try_into().ok()?,
        )) != length - udp
    {
        return None;
    }
    Some((udp, udp + 8))
}

/// Six upstream QUIC fakes, then the exact original discovery datagram, or
/// IPv4 fragments which reassemble to that datagram. No media bytes change.
pub(in crate::network) fn packet_plan(
    raw: &[u8],
    request: &[u8],
    variant: u8,
    id: u16,
) -> io::Result<Vec<(Vec<u8>, bool)>> {
    let (udp, payload) = headers(raw).ok_or_else(unavailable)?;
    if !(1..=3).contains(&variant)
        || !fastcord_net::udp::is_discovery_request(request)
        || raw[payload..] != *request
    {
        return Err(unavailable());
    }
    let mut fake = raw[..payload].to_vec();
    fake.extend_from_slice(QUIC);
    let udp_length = u16::try_from(fake.len() - udp).map_err(|_| unavailable())?;
    fake[udp + 4..udp + 6].copy_from_slice(&udp_length.to_be_bytes());
    if raw[0] >> 4 == 4 {
        let length = u16::try_from(fake.len()).map_err(|_| unavailable())?;
        fake[2..4].copy_from_slice(&length.to_be_bytes());
    } else {
        fake[4..6].copy_from_slice(&udp_length.to_be_bytes());
    }
    let mut plan = Vec::with_capacity(REPEATS + 2);
    for _ in 0..REPEATS {
        plan.push((fake.clone(), true));
    }
    if variant == 1 || raw[0] >> 4 == 6 {
        plan.push((raw.to_vec(), false));
    } else {
        if udp != 20 {
            return Err(unavailable());
        }
        let split = if variant == 2 { 8 } else { 16 };
        let body = &raw[udp..];
        if split >= body.len() {
            return Err(unavailable());
        }
        for (offset, more, bytes) in [(0, true, &body[..split]), (split, false, &body[split..])] {
            let mut fragment = raw[..udp].to_vec();
            fragment.extend_from_slice(bytes);
            let length = u16::try_from(fragment.len()).map_err(|_| unavailable())?;
            fragment[2..4].copy_from_slice(&length.to_be_bytes());
            fragment[4..6].copy_from_slice(&id.to_be_bytes());
            let flags = (u16::try_from(offset / 8).map_err(|_| unavailable())?)
                | if more { 0x2000 } else { 0 };
            fragment[6..8].copy_from_slice(&flags.to_be_bytes());
            fragment[10..12].fill(0);
            let checksum = ip_checksum(&fragment[..udp]);
            fragment[10..12].copy_from_slice(&checksum.to_be_bytes());
            plan.push((fragment, false));
        }
    }
    Ok(plan)
}

fn ip_checksum(header: &[u8]) -> u16 {
    let mut sum = header.as_chunks::<2>().0.iter().fold(0u32, |sum, bytes| {
        sum + u32::from(u16::from_be_bytes([bytes[0], bytes[1]]))
    });
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ipv4_checksum_matches_the_rfc_1071_vector() {
        assert_eq!(
            ip_checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]),
            0x220d
        );
    }

    fn request() -> [u8; 74] {
        let mut request = [0; 74];
        request[..8].copy_from_slice(&[0, 1, 0, 70, 0x12, 0x34, 0x56, 0x78]);
        request
    }
    fn packet(ipv6: bool) -> Vec<u8> {
        let header = if ipv6 { 40 } else { 20 };
        let mut packet = vec![0; header + 8];
        if ipv6 {
            packet[0] = 0x60;
            packet[4..6].copy_from_slice(&82u16.to_be_bytes());
            packet[6] = 17;
            packet[7] = 64;
        } else {
            packet[0] = 0x45;
            packet[2..4].copy_from_slice(&102u16.to_be_bytes());
            packet[6] = 0x40;
            packet[8] = 64;
            packet[9] = 17;
        }
        packet[header..header + 2].copy_from_slice(&50_123u16.to_be_bytes());
        packet[header + 2..header + 4].copy_from_slice(&50_000u16.to_be_bytes());
        packet[header + 4..header + 6].copy_from_slice(&82u16.to_be_bytes());
        packet[header + 6..header + 8].copy_from_slice(&0xabcdu16.to_be_bytes());
        packet.extend_from_slice(&request());
        packet
    }

    #[test]
    fn fakes_preserve_endpoints_and_original_discovery() {
        for ipv6 in [false, true] {
            let raw = packet(ipv6);
            let (udp, payload) = headers(&raw).unwrap();
            let plan = packet_plan(&raw, &request(), 1, 7).unwrap();
            assert_eq!(plan.len(), 7);
            for (fake, calculate) in &plan[..6] {
                assert!(*calculate);
                assert_eq!(&fake[udp..udp + 4], &raw[udp..udp + 4]);
                assert_eq!(&fake[payload..], QUIC);
                assert_eq!(headers(fake), Some((udp, payload)));
            }
            assert_eq!(plan[6], (raw, false));
        }
    }
    #[test]
    fn fragments_reassemble_exact_udp_bytes_including_the_full_checksum() {
        let raw = packet(false);
        for (variant, split) in [(2, 8), (3, 16)] {
            let plan = packet_plan(&raw, &request(), variant, 0xbeef).unwrap();
            assert_eq!(plan.len(), 8);
            let (first, second) = (&plan[6].0, &plan[7].0);
            assert_eq!(first.len(), 20 + split);
            assert_eq!(u16::from_be_bytes(first[6..8].try_into().unwrap()), 0x2000);
            assert_eq!(
                u16::from_be_bytes(second[6..8].try_into().unwrap()),
                split as u16 / 8
            );
            assert_eq!(&first[4..6], &0xbeefu16.to_be_bytes());
            assert_eq!(&first[4..6], &second[4..6]);
            assert_eq!(ip_checksum(&first[..20]), 0);
            assert_eq!(ip_checksum(&second[..20]), 0);
            let reassembled = [&first[20..], &second[20..]].concat();
            assert_eq!(reassembled, &raw[20..]);
        }
    }
    #[test]
    fn malformed_or_protected_packets_never_enter_a_plan() {
        let raw = packet(false);
        for n in 0..raw.len() {
            assert!(packet_plan(&raw[..n], &request(), 1, 7).is_err());
        }
        for variant in [0, 4, 255] {
            assert!(packet_plan(&raw, &request(), variant, 7).is_err());
        }
        let mut bad = raw.clone();
        bad[9] = 6;
        assert!(packet_plan(&bad, &request(), 1, 7).is_err());
        let mut bad = raw.clone();
        bad[28] = 0x80;
        assert!(packet_plan(&bad, &request(), 1, 7).is_err());
        let mut bad = raw;
        bad[6] = 0x20;
        assert!(packet_plan(&bad, &request(), 1, 7).is_err());
    }
    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn independent_completed_udp_and_ipv4_fragment_vectors() {
        // Original synthetic vector computed independently with Python network-
        // order integer arithmetic, including IPv4 pseudoheader UDP checksum.
        // Documentation-only addresses: no fixture is ever sent to a socket.
        let raw = hex(
            "450000661234400040113c1cc0000201c6336402c3cbc350005223030001004612345678000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        );
        for (variant, first, second) in [
            (
                2,
                "4500001cbeef20004011afaac0000201c6336402c3cbc35000522303",
                "4500005ebeef00014011cf67c0000201c63364020001004612345678000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            ),
            (
                3,
                "45000024beef20004011afa2c0000201c6336402c3cbc350005223030001004612345678",
                "45000056beef00024011cf6ec0000201c6336402000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
            ),
        ] {
            let plan = packet_plan(&raw, &request(), variant, 0xbeef).unwrap();
            assert_eq!(plan[6], (hex(first), false));
            assert_eq!(plan[7], (hex(second), false));
            assert_eq!([&plan[6].0[20..], &plan[7].0[20..]].concat(), raw[20..]);
            // Fake datagrams explicitly require native checksum completion;
            // original/fragmented datagrams keep the completed UDP checksum.
            assert!(plan[..6].iter().all(|(_, calculate)| *calculate));
        }
    }

    #[test]
    fn options_and_ipv6_profiles_keep_the_existing_capability_boundary() {
        let mut options = packet(false);
        options.splice(20..20, [1, 1, 0, 0]);
        options[0] = 0x46;
        let size = options.len() as u16;
        options[2..4].copy_from_slice(&size.to_be_bytes());
        assert_eq!(headers(&options), Some((24, 32)));
        assert_eq!(
            packet_plan(&options, &request(), 1, 7).unwrap()[6],
            (options.clone(), false)
        );
        for variant in [2, 3] {
            assert!(packet_plan(&options, &request(), variant, 7).is_err());
        }
        let v6 = packet(true);
        for variant in [1, 2, 3] {
            let plan = packet_plan(&v6, &request(), variant, 7).unwrap();
            assert_eq!(plan.len(), 7);
            assert_eq!(plan[6], (v6.clone(), false));
        }
        let mut extension = v6;
        extension[6] = 0;
        assert!(headers(&extension).is_none());
    }
}
