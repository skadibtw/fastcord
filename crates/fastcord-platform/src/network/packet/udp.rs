//! Scoped zapret2-style UDP recovery. Only a canonical discovery request is
//! intercepted; protected media never enters this helper.

use super::super::packet_plan::udp::{headers, packet_plan};
use super::*;
use rand_core::{OsRng, RngCore};

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
