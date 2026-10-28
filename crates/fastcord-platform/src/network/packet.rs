//! Optional, explicitly elevated WinDivert recovery. TLS capture is narrowed
//! by destination and all 256 ClientHello-random bits; UDP capture by the exact
//! canonical discovery and call SSRC. No broad port/host filter.
#![allow(unsafe_code)]

mod udp;
pub(super) use udp::serve as serve_udp;

use std::ffi::{CString, c_void};
#[cfg(debug_assertions)]
use std::io::Write;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{GetLastError, HMODULE};
use windows::Win32::NetworkManagement::IpHelper::{GetIfEntry2, MIB_IF_ROW2};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::PCWSTR;

#[repr(C)]
#[derive(Clone, Copy)]
struct Address {
    timestamp: i64,
    flags: u32,
    reserved: u32,
    data: [u8; 64],
}
const _: () = assert!(std::mem::size_of::<Address>() == 80);
type Open = unsafe extern "system" fn(*const i8, u32, i16, u64) -> isize;
type Recv = unsafe extern "system" fn(isize, *mut u8, u32, *mut u32, *mut Address) -> i32;
type SendPacket = unsafe extern "system" fn(isize, *const u8, u32, *mut u32, *const Address) -> i32;
type Close = unsafe extern "system" fn(isize) -> i32;
type Stop = unsafe extern "system" fn(isize, u32) -> i32;
type Param = unsafe extern "system" fn(isize, u32, u64) -> i32;
type Checksums = unsafe extern "system" fn(*mut u8, u32, *mut Address, u64) -> i32;

struct Api {
    module: isize,
    _pinned: Vec<std::fs::File>,
    open: Open,
    recv: Recv,
    send: SendPacket,
    close: Close,
    stop: Stop,
    param: Param,
    checksums: Checksums,
}
impl Drop for Api {
    fn drop(&mut self) {
        // SAFETY: the module outlives every function pointer/handle through Arc.
        unsafe {
            let _ = windows::Win32::Foundation::FreeLibrary(HMODULE(self.module as *mut c_void));
        }
    }
}
static API: OnceLock<Result<Arc<Api>, fastcord_net::NetworkError>> = OnceLock::new();
fn unavailable() -> io::Error {
    io::Error::other(fastcord_net::NetworkError::Unsupported)
}
unsafe fn symbol<T: Copy>(module: HMODULE, name: &'static [u8]) -> io::Result<T> {
    // SAFETY: private callers use the exact WinDivert 2.2.2 ABI function types.
    let pointer = unsafe { GetProcAddress(module, windows::core::PCSTR(name.as_ptr())) }
        .ok_or_else(unavailable)?;
    if std::mem::size_of::<T>() != std::mem::size_of_val(&pointer) {
        return Err(unavailable());
    }
    Ok(unsafe { std::mem::transmute_copy(&pointer) })
}
/// The unmodified upstream files, embedded so the standalone executable needs
/// no `windivert/` folder. They are hash-checked again under a deny-write
/// handle before loading, wherever they were read from.
const PINNED: [(&str, &[u8], &str); 2] = [
    (
        "WinDivert.dll",
        include_bytes!("../../native/windivert/WinDivert.dll"),
        "c1e060ee19444a259b2162f8af0f3fe8c4428a1c6f694dce20de194ac8d7d9a2",
    ),
    (
        "WinDivert64.sys",
        include_bytes!("../../native/windivert/WinDivert64.sys"),
        "8da085332782708d8767bcace5327a6ec7283c17cfb85e40b03cd2323a90ddc2",
    ),
];
/// Writes the embedded pair into a versioned cache folder unless an intact
/// copy is already there. A copy that is in use stays; `load` rejects it if
/// its bytes differ.
fn extract_embedded() -> Result<std::path::PathBuf, fastcord_net::NetworkError> {
    let directory = crate::paths::cache_dir("fastcord")
        .ok_or(fastcord_net::NetworkError::Unsupported)?
        .join("windivert-2.2.2-A");
    std::fs::create_dir_all(&directory).map_err(|_| fastcord_net::NetworkError::Unsupported)?;
    for (name, bytes, _) in PINNED {
        let path = directory.join(name);
        if std::fs::read(&path).is_ok_and(|current| current == bytes) {
            continue;
        }
        let _ = crate::paths::write_atomic(&path, bytes);
    }
    Ok(directory)
}
fn load() -> Result<Arc<Api>, fastcord_net::NetworkError> {
    use std::os::windows::fs::OpenOptionsExt;
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("windivert")));
    let directory = if cfg!(debug_assertions) {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("native/windivert")
    } else if let Some(beside) = beside.filter(|d| d.join("WinDivert.dll").is_file()) {
        beside
    } else {
        extract_embedded()?
    };
    let mut pinned = Vec::with_capacity(2);
    for (name, _, expected) in PINNED {
        // Deny write/delete sharing throughout DLL/driver use, including the
        // interval between verification and loading in the elevated process.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0)
            .open(directory.join(name))
            .map_err(|_| fastcord_net::NetworkError::Unsupported)?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 8192];
        let mut length = 0;
        loop {
            let n = file
                .read(&mut buffer)
                .map_err(|_| fastcord_net::NetworkError::Unsupported)?;
            if n == 0 {
                break;
            }
            length += n;
            if length > 2 * 1024 * 1024 {
                return Err(fastcord_net::NetworkError::Unsupported);
            }
            hash.update(&buffer[..n]);
        }
        if format!("{:x}", hash.finalize()) != expected {
            return Err(fastcord_net::NetworkError::Unsupported);
        }
        pinned.push(file);
    }
    let path: Vec<u16> = directory
        .join("WinDivert.dll")
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect();
    // SAFETY: pinned full path and System32-only dependency search (the cache
    // folder is user-writable, so nothing beside the DLL is searched). The owned
    // module remains loaded for the lifetime of all Arc<Api> users.
    let module =
        unsafe { LoadLibraryExW(PCWSTR(path.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map_err(|_| fastcord_net::NetworkError::Unsupported)?;
    let api = unsafe {
        Api {
            module: module.0 as isize,
            _pinned: pinned,
            open: symbol(module, b"WinDivertOpen\0")
                .map_err(|_| fastcord_net::NetworkError::Unsupported)?,
            recv: symbol(module, b"WinDivertRecv\0")
                .map_err(|_| fastcord_net::NetworkError::Unsupported)?,
            send: symbol(module, b"WinDivertSend\0")
                .map_err(|_| fastcord_net::NetworkError::Unsupported)?,
            close: symbol(module, b"WinDivertClose\0")
                .map_err(|_| fastcord_net::NetworkError::Unsupported)?,
            stop: symbol(module, b"WinDivertShutdown\0")
                .map_err(|_| fastcord_net::NetworkError::Unsupported)?,
            param: symbol(module, b"WinDivertSetParam\0")
                .map_err(|_| fastcord_net::NetworkError::Unsupported)?,
            checksums: symbol(module, b"WinDivertHelperCalcChecksums\0")
                .map_err(|_| fastcord_net::NetworkError::Unsupported)?,
        }
    };
    Ok(Arc::new(api))
}
use std::os::windows::ffi::OsStrExt;

struct State {
    api: Arc<Api>,
    handle: Mutex<Option<isize>>,
    canceled: AtomicBool,
    done: Mutex<bool>,
    wake: Condvar,
}

pub(super) fn initialize() -> io::Result<()> {
    let api = API.get_or_init(load).clone().map_err(io::Error::other)?;
    // Empty capture proves driver availability/admin access without capturing.
    let h = unsafe { (api.open)(c"false".as_ptr(), 0, 100, 0) };
    if h == -1 || h == 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        (api.close)(h);
    }
    Ok(())
}

pub(super) fn serve(
    peer: std::net::SocketAddr,
    original: &[u8],
    fake: &[u8],
    strategy: fastcord_net::tls::Strategy,
    armed: impl FnOnce() -> io::Result<()>,
) -> io::Result<usize> {
    timed(|state| recover(state, peer, original, fake, strategy, armed))
}

fn timed(run: impl FnOnce(&State) -> io::Result<usize>) -> io::Result<usize> {
    let api = API.get_or_init(load).clone().map_err(io::Error::other)?;
    let state = Arc::new(State {
        api,
        handle: Mutex::new(None),
        canceled: AtomicBool::new(false),
        done: Mutex::new(false),
        wake: Condvar::new(),
    });
    let timer = state.clone();
    let watchdog = std::thread::Builder::new()
        .name("fastcord-packet-deadline".into())
        .spawn(move || {
            let done = timer.done.lock().unwrap_or_else(|p| p.into_inner());
            let (done, _) = timer
                .wake
                .wait_timeout_while(done, Duration::from_secs(4), |done| !*done)
                .unwrap_or_else(|p| p.into_inner());
            if !*done {
                timer.cancel();
            }
        })?;
    let result = run(&state);
    *state.done.lock().unwrap_or_else(|p| p.into_inner()) = true;
    state.wake.notify_all();
    let _ = watchdog.join();
    result
}
impl State {
    fn cancel(&self) {
        self.canceled.store(true, Ordering::Relaxed);
        if let Some(handle) = *self.handle.lock().unwrap_or_else(|p| p.into_inner()) {
            // SAFETY: handle cannot close while this mutex is held.
            unsafe {
                (self.api.stop)(handle, 1);
            }
        }
    }
    fn send(&self, packet: &mut [u8], address: &Address, checksums: bool) -> io::Result<()> {
        let mut address = *address;
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ok_or_else(unavailable)?;
        // SAFETY: exact buffers and live WinDivert handle, bounded by caller.
        unsafe {
            if checksums
                && (self.api.checksums)(packet.as_mut_ptr(), packet.len() as u32, &mut address, 0)
                    == 0
            {
                return Err(unavailable());
            }
            let mut sent = 0;
            if (self.api.send)(
                handle,
                packet.as_ptr(),
                packet.len() as u32,
                &mut sent,
                &address,
            ) == 0
                || sent as usize != packet.len()
            {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
        }
        Ok(())
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(handle) = self
            .handle
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            // SAFETY: final Arc, no remaining receive/send/cancel operation.
            unsafe {
                (self.api.close)(handle);
            }
        }
    }
}
fn recover(
    state: &State,
    peer: std::net::SocketAddr,
    original: &[u8],
    fake: &[u8],
    strategy: fastcord_net::tls::Strategy,
    armed: impl FnOnce() -> io::Result<()>,
) -> io::Result<usize> {
    let destination = if peer.is_ipv4() {
        format!("ip.DstAddr == {}", peer.ip())
    } else {
        format!("ipv6.DstAddr == {}", peer.ip())
    };
    let mut filter = format!(
        "outbound and tcp and {destination} and tcp.DstPort == {} and tcp.PayloadLength >= 43",
        peer.port()
    );
    for (i, word) in original[11..43].as_chunks::<4>().0.iter().enumerate() {
        let value = u32::from_be_bytes(*word);
        filter.push_str(&format!(
            " and tcp.Payload32[{}b] == 0x{value:08x}",
            11 + i * 4
        ));
    }
    let filter = CString::new(filter).map_err(|_| unavailable())?;
    // SAFETY: exact ABI, NUL-terminated narrow capture filter, NETWORK layer.
    let handle = unsafe { (state.api.open)(filter.as_ptr(), 0, 100, 0) };
    if handle == -1 || handle == 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() }.0 as i32,
        ));
    }
    #[cfg(debug_assertions)]
    let _ = writeln!(std::io::stderr(), "Scoped packet filter armed.");
    {
        let mut active = state.handle.lock().unwrap_or_else(|p| p.into_inner());
        *active = Some(handle);
    }
    if state.canceled.load(Ordering::Relaxed) {
        state.cancel();
        return Err(io::ErrorKind::Interrupted.into());
    }
    // SAFETY: live handle; small bounded kernel queue, not a traffic backlog.
    unsafe {
        for (parameter, value) in [(0, 32), (1, 100), (2, 131072)] {
            if (state.api.param)(handle, parameter, value) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    armed()?;
    let mut packet = vec![0u8; 65535];
    for _ in 0..8 {
        let mut length = 0;
        let mut address = Address {
            timestamp: 0,
            flags: 0,
            reserved: 0,
            data: [0; 64],
        };
        // SAFETY: buffers cover the maximum IP packet and the exact 80-byte ABI.
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
        // SAFETY: initialized full MIB row, no persistent identifiers logged.
        let physical = unsafe { GetIfEntry2(&mut row) }.is_ok()
            && row.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
        #[cfg(debug_assertions)]
        let _ = writeln!(
            std::io::stderr(),
            "Matching ClientHello captured; physical egress={physical}"
        );
        if !physical {
            state.send(raw, &address, false)?;
            continue;
        }
        let Some((_, payload)) = headers(raw) else {
            state.send(raw, &address, false)?;
            return Err(unavailable());
        };
        if raw[payload..] != *original {
            state.send(raw, &address, false)?;
            return Err(unavailable());
        }
        for mut part in packet_plan(raw, original, fake, strategy)? {
            state.send(&mut part, &address, true)?;
        }
        return Ok(original.len());
    }
    Err(unavailable())
}

fn packet_plan(
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

fn headers(packet: &[u8]) -> Option<(usize, usize)> {
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
