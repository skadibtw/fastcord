//! Pure bounded packet layouts shared by the Windows interceptor and portable
//! offline tests. No sockets, interception, elevation or checksum offload here.
use std::io;

pub(super) mod udp;

fn unavailable() -> io::Error {
    io::Error::other(fastcord_net::NetworkError::Unsupported)
}

pub(super) fn packet_plan(
    raw: &[u8],
    original: &[u8],
    fake: &[u8],
    strategy: fastcord_net::tls::Strategy,
) -> io::Result<Vec<Vec<u8>>> {
    if original.is_empty()
        || original.len() > 1024
        || fake.is_empty()
        || fake.len() > 1024
        || !matches!(
            strategy,
            fastcord_net::tls::Strategy::PacketFake | fastcord_net::tls::Strategy::PacketOverlap
        )
    {
        return Err(unavailable());
    }
    let (tcp, payload) = headers(raw).ok_or_else(unavailable)?;
    if raw[payload..] != *original {
        return Err(unavailable());
    }
    let mut packets = Vec::with_capacity(9);
    let sequence = u32::from_be_bytes(
        raw[tcp + 4..tcp + 8]
            .try_into()
            .map_err(|_| unavailable())?,
    );
    let info = match fastcord_net::tls::inspect_client_hello(original) {
        Ok(fastcord_net::tls::Inspection::ClientHello(info)) => info,
        _ => {
            return Err(unavailable());
        }
    };
    let points = [0, 1, info.sni_mid, original.len()];
    if strategy == fastcord_net::tls::Strategy::PacketOverlap {
        // zapret general.bat: multisplit pos=1,midsld, seqovl=681.
        // The peer discards the already-acknowledged overlap; its novel
        // byte is exactly original[0], with the correct sequence number.
        let mut overlap: Vec<u8> = fake.iter().copied().cycle().take(681).collect();
        overlap.push(original[0]);
        let mut first = segment(raw, tcp, payload, &overlap, sequence.wrapping_sub(681))?;
        first[tcp + 13] &= !8;
        packets.push(first);
    } else {
        let decoy = segment(raw, tcp, payload, fake, sequence.wrapping_sub(10000))?;
        for _ in 0..6 {
            packets.push(decoy.clone());
        }
    }
    let order: &[usize] = if strategy == fastcord_net::tls::Strategy::PacketOverlap {
        &[1, 2]
    } else {
        &[2, 1, 0]
    };
    for &i in order {
        let mut part = segment(
            raw,
            tcp,
            payload,
            &original[points[i]..points[i + 1]],
            sequence.wrapping_add(points[i] as u32),
        )?;
        if i != 2 {
            part[tcp + 13] &= !8;
        }
        packets.push(part);
    }
    Ok(packets)
}

pub(super) fn headers(packet: &[u8]) -> Option<(usize, usize)> {
    let declared = match packet.first()? >> 4 {
        4 => usize::from(u16::from_be_bytes(packet.get(2..4)?.try_into().ok()?)),
        6 => 40 + usize::from(u16::from_be_bytes(packet.get(4..6)?.try_into().ok()?)),
        _ => return None,
    };
    if declared != packet.len() {
        return None;
    }
    let tcp = match packet.first()? >> 4 {
        4 if packet.get(9) == Some(&6)
            && u16::from_be_bytes(packet.get(6..8)?.try_into().ok()?) & 0x3fff == 0 =>
        {
            usize::from(packet[0] & 15) * 4
        }
        6 if packet.get(6) == Some(&6) => 40,
        _ => return None,
    };
    if tcp < 20 {
        return None;
    }
    let payload = tcp + usize::from(*packet.get(tcp + 12)? >> 4) * 4;
    (payload >= tcp + 20 && payload <= packet.len()).then_some((tcp, payload))
}
fn segment(
    packet: &[u8],
    tcp: usize,
    payload: usize,
    bytes: &[u8],
    sequence: u32,
) -> io::Result<Vec<u8>> {
    let mut result = packet[..payload].to_vec();
    result.extend_from_slice(bytes);
    let length = result.len();
    if length > 65535 {
        return Err(unavailable());
    }
    match result[0] >> 4 {
        4 => result[2..4].copy_from_slice(&(length as u16).to_be_bytes()),
        6 => result[4..6].copy_from_slice(&((length - 40) as u16).to_be_bytes()),
        _ => return Err(unavailable()),
    }
    result[tcp + 4..tcp + 8].copy_from_slice(&sequence.to_be_bytes());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_net::tls::Strategy;
    fn hello() -> Vec<u8> {
        let host = b"discord.com";
        let mut body = vec![3, 3];
        body.extend_from_slice(&[0x42; 32]);
        body.extend_from_slice(&[0, 0, 2, 0x13, 1, 1, 0]);
        let mut sni = vec![0, 14, 0, 0, 11];
        sni.extend_from_slice(host);
        body.extend_from_slice(&20u16.to_be_bytes());
        body.extend_from_slice(&[0, 0, 0, 16]);
        body.extend(sni);
        let mut handshake = vec![1, 0, 0, body.len() as u8];
        handshake.extend(body);
        let mut record = vec![22, 3, 3];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend(handshake);
        record
    }
    fn packet(v6: bool, bytes: &[u8], sequence: u32) -> Vec<u8> {
        let tcp = if v6 { 40 } else { 20 };
        let mut p = vec![0; tcp + 32];
        p[0] = if v6 { 0x60 } else { 0x45 };
        if v6 {
            p[6] = 6;
            p[7] = 64;
        } else {
            p[9] = 6;
            p[8] = 64;
        }
        p[tcp..tcp + 2].copy_from_slice(&50000u16.to_be_bytes());
        p[tcp + 2..tcp + 4].copy_from_slice(&443u16.to_be_bytes());
        p[tcp + 4..tcp + 8].copy_from_slice(&sequence.to_be_bytes());
        p[tcp + 8..tcp + 12].copy_from_slice(&123456u32.to_be_bytes());
        p[tcp + 12] = 8 << 4;
        p[tcp + 13] = 0x18;
        p[tcp + 14..tcp + 16].copy_from_slice(&4096u16.to_be_bytes());
        p[tcp + 20..tcp + 32].copy_from_slice(&[1, 1, 8, 10, 0, 0, 0, 3, 0, 0, 0, 4]);
        segment(&p, tcp, tcp + 32, bytes, sequence).unwrap()
    }
    #[test]
    fn packet_profiles_reassemble_exact_hello_with_ipv4_ipv6_options_and_sequence_wrap() {
        let original = hello();
        let mut fake = original.clone();
        let n = fake.len();
        fake[n - 11..].copy_from_slice(b"example.com");
        for v6 in [false, true] {
            for sequence in [12345, u32::MAX - 2] {
                for strategy in [Strategy::PacketFake, Strategy::PacketOverlap] {
                    let raw = packet(v6, &original, sequence);
                    let (tcp, payload) = headers(&raw).unwrap();
                    let packets = packet_plan(&raw, &original, &fake, strategy).unwrap();
                    assert_eq!(
                        packets.len(),
                        if strategy == Strategy::PacketFake {
                            9
                        } else {
                            3
                        }
                    );
                    let mut assembled = vec![None; original.len()];
                    let mut total = 0;
                    for p in packets {
                        total += p.len();
                        assert!(p.len() <= 1500);
                        assert_eq!(headers(&p), Some((tcp, payload)));
                        assert_eq!(&p[tcp..tcp + 4], &raw[tcp..tcp + 4]);
                        assert_eq!(&p[tcp + 8..tcp + 13], &raw[tcp + 8..tcp + 13]);
                        assert_eq!(&p[tcp + 14..payload], &raw[tcp + 14..payload]);
                        let start = u32::from_be_bytes(p[tcp + 4..tcp + 8].try_into().unwrap())
                            .wrapping_sub(sequence);
                        for (i, &byte) in p[payload..].iter().enumerate() {
                            let offset = start.wrapping_add(i as u32) as usize;
                            if offset < assembled.len() {
                                if let Some(previous) = assembled[offset] {
                                    assert_eq!(previous, byte);
                                }
                                assembled[offset] = Some(byte);
                            }
                        }
                    }
                    assert_eq!(
                        assembled,
                        original.iter().copied().map(Some).collect::<Vec<_>>()
                    );
                    assert!(total <= original.len() + 8192);
                }
            }
        }
    }
    #[test]
    fn malformed_fragmented_extended_or_nonhello_packets_are_refused() {
        let original = hello();
        let p = packet(false, &original, 1);
        for length in 0..p.len() {
            assert!(headers(&p[..length]).is_none());
        }
        let mut fragmented = p.clone();
        fragmented[6] = 0x20;
        assert!(headers(&fragmented).is_none());
        let mut short_tcp = p.clone();
        short_tcp[20 + 12] = 4 << 4;
        assert!(headers(&short_tcp).is_none());
        let mut v6 = packet(true, &original, 1);
        v6[6] = 0;
        assert!(headers(&v6).is_none());
        assert!(packet_plan(&p, &original, &[], Strategy::PacketFake).is_err());
        assert!(packet_plan(&p, &original, &original, Strategy::Direct).is_err());
        assert!(packet_plan(&p, &[0; 43], &original, Strategy::PacketFake).is_err());
    }
}
