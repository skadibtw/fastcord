//! WASAPI loopback capture on one MTA thread (SPEC §8.4, §8.7). Unsafe code is
//! confined to this file: it is the Win32 audio FFI.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::mem::{ManuallyDrop, size_of};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, E_INVALIDARG, HANDLE, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, PROCESS_LOOPBACK_MODE,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    IAgileObject, IAgileObject_Impl,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::core::{Error, IUnknown, Interface, PCWSTR, implement};

use super::{CHANNELS, Handler, LoopbackError, LoopbackScope, LoopbackSupport, SAMPLE_RATE};
use crate::capture::SourceId;

/// HRESULT_FROM_WIN32(ERROR_NOT_FOUND): no default output device.
const ELEMENT_NOT_FOUND: windows::core::HRESULT = windows::core::HRESULT(0x8007_0490_u32 as i32);

/// First Windows build with process loopback (SPEC §8.4, [S21]).
const PROCESS_LOOPBACK_BUILD: u32 = 20_348;
const START_TIMEOUT: Duration = Duration::from_secs(8);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// Also drain on a timeout: some process-loopback devices produce packets
/// without signalling their event. Keep that fallback within one Opus frame.
const IDLE_WAIT_MS: u32 = 20;
/// Device buffer: 200 ms in 100 ns units.
const BUFFER_DURATION: i64 = 2_000_000;
/// Frames of zeros handed over per call while the OS reports silence.
const ZERO_FRAMES: usize = 4_800;

struct Handle(HANDLE);

// SAFETY: kernel event handles are valid on any thread, and the handle is
// closed only after every `Arc` reference is gone.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns one successfully created handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn event() -> Result<Arc<Handle>, LoopbackError> {
    // SAFETY: an unnamed auto-reset event with default security, owned by `Handle`.
    unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map(|handle| Arc::new(Handle(handle)))
        .map_err(|error| native("CreateEventW", &error))
}

fn native(operation: &'static str, error: &Error) -> LoopbackError {
    match error.code() {
        E_ACCESSDENIED => LoopbackError::PermissionDenied,
        ELEMENT_NOT_FOUND => LoopbackError::NoOutputDevice,
        code => LoopbackError::Native {
            operation,
            code: code.0,
        },
    }
}

pub(super) struct Native {
    stop: Arc<Handle>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Native {
    pub(super) fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub(super) fn stop(mut self) {
        // SAFETY: the event outlives the call.
        let _ = unsafe { SetEvent(self.stop.0) };
        if let Some(thread) = self.thread.take() {
            join_within(thread, STOP_TIMEOUT);
        }
    }
}

/// Joins `thread` unless it hangs (a stuck audio driver must not hang the app).
fn join_within(thread: JoinHandle<()>, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !thread.is_finished() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if thread.is_finished() {
        let _ = thread.join();
    }
}

pub(super) fn support() -> LoopbackSupport {
    static BUILD: OnceLock<u32> = OnceLock::new();
    let build = *BUILD.get_or_init(|| {
        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a correctly sized, writable OSVERSIONINFOW.
        let status = unsafe { RtlGetVersion(&mut info) };
        if status.is_ok() {
            info.dwBuildNumber
        } else {
            0
        }
    });
    LoopbackSupport {
        process_loopback: build >= PROCESS_LOOPBACK_BUILD,
        system_loopback: true,
    }
}

pub(super) fn window_process(source: SourceId) -> Option<u32> {
    let mut process = 0u32;
    // SAFETY: the call only reads the window's owner; a stale handle returns 0.
    unsafe {
        GetWindowThreadProcessId(
            HWND(source.raw() as usize as *mut c_void),
            Some(&mut process),
        )
    };
    (process != 0).then_some(process)
}

pub(super) fn start(scope: LoopbackScope, handler: Handler) -> Result<Native, LoopbackError> {
    if !scope.includes_fastcord() && !support().process_loopback {
        return Err(LoopbackError::Unsupported(
            "this Windows version cannot capture one application's audio",
        ));
    }
    let stop = event()?;
    let running = Arc::new(AtomicBool::new(true));
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("fastcord-loopback".to_owned())
        .spawn({
            let stop = Arc::clone(&stop);
            let running = Arc::clone(&running);
            move || run(scope, handler, &stop, &running, &ready_tx)
        })
        .map_err(|_| LoopbackError::Worker("could not start the audio capture thread"))?;
    let native = Native {
        stop,
        running,
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(native),
        Ok(Err(error)) => {
            native.stop();
            Err(error)
        }
        Err(_) => {
            native.stop();
            Err(LoopbackError::Worker("audio capture did not start in time"))
        }
    }
}

fn run(
    scope: LoopbackScope,
    mut handler: Handler,
    stop: &Handle,
    running: &AtomicBool,
    ready: &mpsc::Sender<Result<(), LoopbackError>>,
) {
    // SAFETY: balanced with `CoUninitialize` below on this thread.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(error) = capture(scope, &mut handler, stop, ready) {
        let _ = ready.send(Err(error));
    }
    running.store(false, Ordering::Release);
    if com.is_ok() {
        // SAFETY: this thread initialized COM above.
        unsafe { CoUninitialize() };
    }
}

fn capture(
    scope: LoopbackScope,
    handler: &mut Handler,
    stop: &Handle,
    ready: &mpsc::Sender<Result<(), LoopbackError>>,
) -> Result<(), LoopbackError> {
    let client = activate(scope)?;
    let format = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * (CHANNELS as u32) * 4,
        nBlockAlign: (CHANNELS * 4) as u16,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // SAFETY: `format` is a valid WAVEFORMATEX that outlives the call.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_DURATION,
            0,
            &format,
            None,
        )
    }
    .map_err(|error| native("IAudioClient::Initialize", &error))?;
    let wake = event()?;
    // SAFETY: `wake` outlives the stream, which is stopped before it drops.
    unsafe { client.SetEventHandle(wake.0) }
        .map_err(|error| native("IAudioClient::SetEventHandle", &error))?;
    // SAFETY: the client is initialized.
    let reader: IAudioCaptureClient = unsafe { client.GetService() }
        .map_err(|error| native("IAudioClient::GetService", &error))?;
    // SAFETY: the client is initialized.
    unsafe { client.Start() }.map_err(|error| native("IAudioClient::Start", &error))?;
    let _ = ready.send(Ok(()));
    let zeros = vec![0f32; ZERO_FRAMES * CHANNELS];
    let result = pump(&reader, &wake, stop, handler, &zeros);
    // SAFETY: the client was started above.
    let _ = unsafe { client.Stop() };
    result
}

fn pump(
    reader: &IAudioCaptureClient,
    wake: &Handle,
    stop: &Handle,
    handler: &mut Handler,
    zeros: &[f32],
) -> Result<(), LoopbackError> {
    let handles = [stop.0, wake.0];
    loop {
        // SAFETY: both handles are alive for the whole loop.
        let waited = unsafe { WaitForMultipleObjects(&handles, false, IDLE_WAIT_MS) };
        if waited != WAIT_TIMEOUT && waited.0.wrapping_sub(WAIT_OBJECT_0.0) != 1 {
            // The stop event, or a failed wait: either way the capture is over.
            return Ok(());
        }
        // A busy producer must not starve the stop event.
        for _ in 0..64 {
            // SAFETY: the stream is running.
            let next = unsafe { reader.GetNextPacketSize() }
                .map_err(|error| native("GetNextPacketSize", &error))?;
            if next == 0 {
                break;
            }
            let mut data = std::ptr::null_mut::<u8>();
            let mut frames = 0u32;
            let mut flags = 0u32;
            // SAFETY: all out pointers are valid locals.
            unsafe { reader.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                .map_err(|error| native("GetBuffer", &error))?;
            let count = frames as usize;
            if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                let mut left = count;
                while left > 0 {
                    let part = left.min(ZERO_FRAMES);
                    handler(&zeros[..part * CHANNELS]);
                    left -= part;
                }
            } else {
                // SAFETY: WASAPI returns `frames` whole frames of the requested
                // 32-bit float format, 4-byte aligned, valid until ReleaseBuffer.
                let samples =
                    unsafe { std::slice::from_raw_parts(data.cast::<f32>(), count * CHANNELS) };
                handler(samples);
            }
            // SAFETY: releases exactly the buffer obtained above.
            unsafe { reader.ReleaseBuffer(frames) }
                .map_err(|error| native("ReleaseBuffer", &error))?;
        }
    }
}

fn activate(scope: LoopbackScope) -> Result<IAudioClient, LoopbackError> {
    match scope {
        LoopbackScope::SystemIncludingFastcord => {
            // SAFETY: plain COM activation on an initialized MTA thread.
            let devices: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                    .map_err(|error| native("MMDeviceEnumerator", &error))?;
            // SAFETY: as above.
            let device = unsafe { devices.GetDefaultAudioEndpoint(eRender, eConsole) }
                .map_err(|error| native("GetDefaultAudioEndpoint", &error))?;
            // SAFETY: as above.
            unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
                .map_err(|error| native("IMMDevice::Activate", &error))
        }
        LoopbackScope::Application { process_id } => activate_process(
            process_id,
            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
        ),
        LoopbackScope::SystemExcludingFastcord => {
            // SAFETY: a plain query of this process's id.
            let own = unsafe { GetCurrentProcessId() };
            activate_process(own, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE)
        }
    }
}

/// Completes the asynchronous activation by setting an event. It must be agile
/// because the system calls it on one of its own threads.
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Done(Arc<Handle>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
    fn ActivateCompleted(
        &self,
        _: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // SAFETY: the event outlives the handler (the activator waits on it).
        unsafe { SetEvent(self.0.0) }
    }
}

impl IAgileObject_Impl for Done_Impl {}

fn activate_process(
    process_id: u32,
    mode: PROCESS_LOOPBACK_MODE,
) -> Result<IAudioClient, LoopbackError> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: process_id,
                ProcessLoopbackMode: mode,
            },
        },
    };
    // Never dropped: the blob points at `params` on this stack, not CoTaskMem.
    let property = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw mut params).cast::<u8>(),
                    },
                },
            }),
        },
    });
    let done = event()?;
    let handler: IActivateAudioInterfaceCompletionHandler = Done(Arc::clone(&done)).into();
    // SAFETY: the property and `params` stay alive until activation completed.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*property),
            &handler,
        )
    }
    .map_err(|error| native("ActivateAudioInterfaceAsync", &error))?;
    // SAFETY: `done` is alive; the handler signals it once.
    if unsafe { WaitForSingleObject(done.0, 5_000) } != WAIT_OBJECT_0 {
        return Err(LoopbackError::Worker("audio activation did not complete"));
    }
    let mut result = windows::core::HRESULT(0);
    let mut unknown: Option<IUnknown> = None;
    // SAFETY: both out parameters are valid locals.
    unsafe { operation.GetActivateResult(&mut result, &mut unknown) }
        .map_err(|error| native("GetActivateResult", &error))?;
    if result.is_err() {
        // A process that no longer exists is rejected as an invalid argument.
        return Err(if result == E_INVALIDARG {
            LoopbackError::TargetNotFound
        } else {
            native("process loopback activation", &Error::from(result))
        });
    }
    unknown
        .ok_or(LoopbackError::Worker("audio activation returned no client"))?
        .cast::<IAudioClient>()
        .map_err(|error| native("IAudioClient", &error))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::super::{LoopbackScope, start};

    /// Opens process loopback excluding this very process (no Discord; silence
    /// arrives as zeros) and reads frames.
    #[test]
    #[ignore = "needs a Windows 10 2004+ audio session"]
    fn process_loopback_of_this_process_delivers_frames() {
        if !super::support().process_loopback {
            return;
        }
        let frames = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&frames);
        let capture = start(
            LoopbackScope::SystemExcludingFastcord,
            Box::new(move |block| {
                counter.fetch_add(block.len() / 2, Ordering::Relaxed);
            }),
        )
        .expect("process loopback starts");
        let deadline = Instant::now() + Duration::from_secs(3);
        while frames.load(Ordering::Relaxed) < 4_800 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(capture.is_running());
        drop(capture);
        assert!(frames.load(Ordering::Relaxed) >= 4_800, "no frames arrived");
    }

    #[test]
    #[ignore = "needs a default audio output device"]
    fn system_loopback_starts_and_stops() {
        let capture = start(LoopbackScope::SystemIncludingFastcord, Box::new(|_| {}))
            .expect("classic loopback starts");
        assert!(capture.is_running());
        drop(capture);
    }
}
