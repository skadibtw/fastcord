//! Scoped zapret2-style UDP recovery. Only a canonical discovery request is
//! intercepted; protected media never enters this helper.

use super::*;
use rand_core::{OsRng, RngCore};

const QUIC: &[u8; 1200] = include_bytes!("../../../native/zapret2/quic_initial_www_google_com.bin");
const REPEATS: usize = 6;

pub(crate) fn serve(
    peer: std::net::SocketAddr,
    request: &[u8],
    variant: u8,
    armed: impl FnOnce() -> io::Result<()>,
) -> io::Result<usize> {
    if !fastcord_net::udp::is_discovery_request(request)
        || !fastcord_net::is_public_destination(peer.ip())
        || peer.port() == 0
        || !(1..=3).contains(&variant)
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    timed(|state| recover(state, peer, request, variant, armed))
}

fn recover(
    state: &State,
    peer: std::net::SocketAddr,
    request: &[u8],
    variant: u8,
    armed: impl FnOnce() -> io::Result<()>,
) -> io::Result<usize> {
    let destination = if peer.is_ipv4() {
        format!("ip.DstAddr == {}", peer.ip())
    } else {
        format!("ipv6.DstAddr == {}", peer.ip())
    };
    let ssrc = u32::from_be_bytes(request[4..8].try_into().map_err(|_| unavailable())?);
    let filter = CString::new(format!(
        "outbound and udp and {destination} and udp.DstPort == {} and udp.PayloadLength == 74 and udp.Payload32[0] == 0x00010046 and udp.Payload32[4b] == 0x{ssrc:08x}",
        peer.port()
    )).map_err(|_| unavailable())?;
    // SAFETY: exact ABI and a bounded filter matching one requested discovery.
    let handle = unsafe { (state.api.open)(filter.as_ptr(), 0, 100, 0) };
    if handle == -1 || handle == 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() }.0 as i32,
        ));
    }
    *state.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
    if state.canceled.load(Ordering::Relaxed) {
        state.cancel();
        return Err(io::ErrorKind::Interrupted.into());
    }
    // SAFETY: live handle, bounded packet count, age and bytes.
    unsafe {
        for (parameter, value) in [(0, 32), (1, 100), (2, 131072)] {
            if (state.api.param)(handle, parameter, value) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    armed()?;
    let mut packet = vec![0; 1500];
    for _ in 0..8 {
        let mut length = 0;
        let mut address = Address {
            timestamp: 0,
            flags: 0,
            reserved: 0,
            data: [0; 64],
        };
        // SAFETY: buffers cover a discovery IP packet; exact WinDivert ABI.
        if unsafe {
            (state.api.recv)(
                handle,
                packet.as_mut_ptr(),
                packet.len() as u32,
                &mut length,
                &mut address,
            )
        } == 0
        {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() }.0 as i32,
            ));
        }
        let raw = packet
            .get_mut(..length as usize)
            .ok_or(io::ErrorKind::InvalidData)?;
        let interface =
            u32::from_ne_bytes(address.data[..4].try_into().map_err(|_| unavailable())?);
        let mut row = MIB_IF_ROW2 {
            InterfaceIndex: interface,
            ..Default::default()
        };
        // SAFETY: initialized full native structure, no identity is persisted.
        let physical = unsafe { GetIfEntry2(&mut row) }.is_ok()
            && row.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
        if !physical {
            state.send(raw, &address, false)?;
            continue;
        }
        let Some((_, payload)) = headers(raw) else {
            state.send(raw, &address, false)?;
            return Err(unavailable());
        };
        if raw[payload..] != *request {
            state.send(raw, &address, false)?;
            return Err(unavailable());
        }
        // Complete offloaded UDP checksum before fragmentation. Fragments keep
        // that checksum; only their individual IPv4 headers are recalculated.
        // SAFETY: live complete packet and exact address ABI.
        if unsafe { (state.api.checksums)(raw.as_mut_ptr(), raw.len() as u32, &mut address, 0) }
            == 0
        {
            state.send(raw, &address, false)?;
            return Err(unavailable());
        }
        let mut id = [0; 2];
        if OsRng.try_fill_bytes(&mut id).is_err() {
            state.send(raw, &address, false)?;
            return Err(unavailable());
        }
        let plan = match packet_plan(raw, request, variant, u16::from_ne_bytes(id)) {
            Ok(plan) => plan,
            Err(error) => {
                state.send(raw, &address, false)?;
                return Err(error);
            }
        };
        for (mut part, calculate) in plan {
            if let Err(error) = state.send(&mut part, &address, calculate) {
                let _ = state.send(raw, &address, false);
                return Err(error);
            }
        }
        #[cfg(debug_assertions)]
        let _ = writeln!(
            std::io::stderr(),
            "Scoped UDP packet recovery applied; variant={variant}; physical=true"
        );
        return Ok(request.len());
    }
    Err(unavailable())
}

fn headers(packet: &[u8]) -> Option<(usize, usize)> {
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
fn packet_plan(
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
}
