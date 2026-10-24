//! Stream audio on macOS (SPEC §8.4, milestone 37): a ScreenCaptureKit
//! stream that captures audio only (its picture is 2x2 at one frame a second
//! and never read). A window share records the window's application; a screen
//! share records everything except fastcord (`excludesCurrentProcessAudio`),
//! so the call is never sent back. Needs macOS 13.
//!
//! ScreenCaptureKit delivers 48 kHz stereo `f32`, usually one buffer per
//! channel; the sample handler interleaves it into a stack block and calls the
//! handler on ScreenCaptureKit's queue, with no allocation.
#![allow(unsafe_code)]

use std::ptr::NonNull;
use std::sync::mpsc::{self, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::{CFDictionary, CFNumber, CFNumberType, CFRetained, CFString};
use objc2_core_graphics::{
    CGMainDisplayID, CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowOwnerPID,
};
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags,
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};
use parking_lot::Mutex;

use super::{
    AudioSource, CHANNELS, Handler, LoopbackError, LoopbackScope, LoopbackSupport, SAMPLE_RATE,
};
use crate::capture::macos::{answered, completion, shareable};
use crate::capture::{CaptureError, SourceId, SourceKind};

/// Samples handed to the handler at once (a multiple of the channel count).
const BLOCK: usize = 1024;

pub(super) struct Native {
    stop: SyncSender<()>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Native {
    pub(super) fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn stop(mut self) {
        let _ = self.stop.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn support() -> LoopbackSupport {
    static AUDIO: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // SAFETY: a selector check on a fresh configuration object.
    let audio = *AUDIO.get_or_init(|| unsafe {
        let configuration = SCStreamConfiguration::new();
        let object: &NSObject = &configuration;
        object.respondsToSelector(sel!(setCapturesAudio:))
    });
    LoopbackSupport {
        process_loopback: audio,
        system_loopback: false,
        chosen_target: false,
    }
}

/// The process owning window `source`, from the window server (fast and
/// synchronous, unlike a ScreenCaptureKit listing).
pub(super) fn window_process(source: SourceId) -> Option<u32> {
    if source.kind() != SourceKind::Window {
        return None;
    }
    let windows = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionIncludingWindow,
        source.raw() as u32,
    )?;
    // SAFETY: the array holds window dictionaries; the owner PID value is a
    // CFNumber when present, read as a 32-bit integer.
    unsafe {
        if windows.count() < 1 {
            return None;
        }
        let window = windows.value_at_index(0).cast::<CFDictionary>().as_ref()?;
        let key: &CFString = kCGWindowOwnerPID;
        let number = window
            .value((key as *const CFString).cast())
            .cast::<CFNumber>()
            .as_ref()?;
        let mut pid = 0i32;
        number
            .value(CFNumberType::SInt32Type, (&raw mut pid).cast())
            .then_some(pid as u32)
            .filter(|&pid| pid != 0)
    }
}

pub(super) fn list_targets() -> Result<Vec<AudioSource>, LoopbackError> {
    Ok(Vec::new())
}

fn loopback_error(error: CaptureError) -> LoopbackError {
    match error {
        CaptureError::PermissionDenied => LoopbackError::PermissionDenied,
        CaptureError::SourceClosed => LoopbackError::TargetNotFound,
        CaptureError::Native { operation, code } => LoopbackError::Native { operation, code },
        _ => LoopbackError::Worker("ScreenCaptureKit could not capture audio"),
    }
}

struct Ivars {
    handler: Mutex<Handler>,
    ended: SyncSender<()>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and `AudioOutput`
    // does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Ivars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output_sample(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Audio {
                deliver(sample, &mut self.ivars().handler.lock());
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, _error: &NSError) {
            let _ = self.ivars().ended.try_send(());
        }
    }
);

impl AudioOutput {
    fn new(handler: Handler, ended: SyncSender<()>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            handler: Mutex::new(handler),
            ended,
        });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Room for a stereo buffer list: one buffer per channel.
#[repr(C)]
struct StereoList {
    list: AudioBufferList,
    second: AudioBuffer,
}

/// Interleaves one audio sample buffer into the handler, in stack blocks.
fn deliver(sample: &CMSampleBuffer, handler: &mut Handler) {
    // SAFETY: the buffer list has room for two buffers, the block buffer that
    // owns the data is released after the last read, and every read stays
    // inside the byte sizes the list reports.
    unsafe {
        let mut stereo: StereoList = std::mem::zeroed();
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        let status = sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            &raw mut stereo.list,
            size_of::<StereoList>(),
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &raw mut block,
        );
        let _owner = NonNull::new(block).map(|block| CFRetained::from_raw(block));
        if status != 0 {
            return;
        }
        let buffers = [stereo.list.mBuffers[0], stereo.second];
        let samples = |buffer: &AudioBuffer| {
            if buffer.mData.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(
                    buffer.mData.cast::<f32>(),
                    buffer.mDataByteSize as usize / 4,
                )
            }
        };
        let mut out = [0.0f32; BLOCK];
        match (stereo.list.mNumberBuffers, buffers[0].mNumberChannels) {
            // Planar: one channel per buffer.
            (2, 1) => {
                let (left, right) = (samples(&buffers[0]), samples(&buffers[1]));
                let frames = left.len().min(right.len());
                for start in (0..frames).step_by(BLOCK / CHANNELS) {
                    let end = (start + BLOCK / CHANNELS).min(frames);
                    for (frame, index) in (start..end).enumerate() {
                        out[frame * 2] = left[index];
                        out[frame * 2 + 1] = right[index];
                    }
                    handler(&out[..(end - start) * 2]);
                }
            }
            // Already interleaved stereo.
            (1, 2) => {
                let interleaved = samples(&buffers[0]);
                let usable = interleaved.len() / CHANNELS * CHANNELS;
                for chunk in interleaved[..usable].chunks(BLOCK) {
                    handler(chunk);
                }
            }
            // Mono: the same sample on both sides.
            (1, 1) => {
                for chunk in samples(&buffers[0]).chunks(BLOCK / CHANNELS) {
                    for (frame, value) in chunk.iter().enumerate() {
                        out[frame * 2] = *value;
                        out[frame * 2 + 1] = *value;
                    }
                    handler(&out[..chunk.len() * 2]);
                }
            }
            _ => {}
        }
    }
}

/// The filter recording `scope`: everything but fastcord, or one application.
fn filter(scope: LoopbackScope) -> Result<Retained<SCContentFilter>, LoopbackError> {
    let content = shareable().map_err(loopback_error)?;
    let main = CGMainDisplayID();
    // SAFETY: property reads and initializers on objects ScreenCaptureKit returned.
    unsafe {
        let display = content
            .displays()
            .iter()
            .find(|display| display.displayID() == main)
            .or_else(|| content.displays().iter().next())
            .ok_or(LoopbackError::NoOutputDevice)?;
        match scope {
            LoopbackScope::Application { process_id } => {
                let app = content
                    .applications()
                    .iter()
                    .find(|app| app.processID() as u32 == process_id)
                    .ok_or(LoopbackError::TargetNotFound)?;
                Ok(
                    SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                        SCContentFilter::alloc(),
                        &display,
                        &NSArray::from_retained_slice(&[app]),
                        &NSArray::new(),
                    ),
                )
            }
            LoopbackScope::SystemExcludingFastcord => {
                Ok(SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::new(),
                ))
            }
            LoopbackScope::SystemIncludingFastcord | LoopbackScope::Target(_) => Err(
                LoopbackError::Unsupported("macOS always leaves fastcord out of shared audio"),
            ),
        }
    }
}

fn configuration() -> Retained<SCStreamConfiguration> {
    // SAFETY: plain property writes on a new configuration (macOS 13+ for
    // the audio ones, checked by `support`).
    unsafe {
        let configuration = SCStreamConfiguration::new();
        configuration.setWidth(2);
        configuration.setHeight(2);
        configuration.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: 1,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        configuration.setQueueDepth(1);
        configuration.setCapturesAudio(true);
        configuration.setExcludesCurrentProcessAudio(true);
        configuration.setSampleRate(SAMPLE_RATE as isize);
        configuration.setChannelCount(CHANNELS as isize);
        configuration
    }
}

pub(super) fn start(scope: LoopbackScope, handler: Handler) -> Result<Native, LoopbackError> {
    if !support().process_loopback {
        return Err(LoopbackError::Unsupported(
            "sharing audio needs macOS 13 or later",
        ));
    }
    let (stop, stop_rx) = mpsc::sync_channel(2);
    let (ready, ready_rx) = mpsc::sync_channel(1);
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let thread_running = std::sync::Arc::clone(&running);
    let ended = stop.clone();
    let thread = std::thread::Builder::new()
        .name("fastcord-share-audio".to_owned())
        .spawn(move || {
            // ScreenCaptureKit objects stay on this thread.
            let filter = match filter(scope) {
                Ok(filter) => filter,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    thread_running.store(false, std::sync::atomic::Ordering::Release);
                    return;
                }
            };
            let output = AudioOutput::new(handler, ended);
            let queue = DispatchQueue::new("dev.fastcord.share-audio", None);
            // SAFETY: the stream, its output and delegate live until the
            // stream is stopped below; the handler runs on `queue` only.
            let stream = unsafe {
                let stream = SCStream::initWithFilter_configuration_delegate(
                    SCStream::alloc(),
                    &filter,
                    &configuration(),
                    Some(ProtocolObject::from_ref(&*output)),
                );
                if let Err(error) = stream.addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&queue),
                ) {
                    let _ = ready.send(Err(LoopbackError::Native {
                        operation: "addStreamOutput",
                        code: error.code() as i32,
                    }));
                    thread_running.store(false, std::sync::atomic::Ordering::Release);
                    return;
                }
                let (block, started) = completion("startCapture");
                stream.startCaptureWithCompletionHandler(Some(&block));
                if let Err(error) = answered(&started) {
                    let _ = ready.send(Err(loopback_error(error)));
                    thread_running.store(false, std::sync::atomic::Ordering::Release);
                    return;
                }
                stream
            };
            let _ = ready.send(Ok(()));
            // A stop from the owner, the stream ending, or the owner gone.
            let _ = stop_rx.recv();
            thread_running.store(false, std::sync::atomic::Ordering::Release);
            // SAFETY: as above; waiting for the stop means no sample follows.
            unsafe {
                let (block, stopped) = completion("stopCapture");
                stream.stopCaptureWithCompletionHandler(Some(&block));
                let _ = answered(&stopped);
            }
        })
        .map_err(|_| LoopbackError::Worker("the audio capture thread could not be started"))?;
    let native = Native {
        stop,
        running,
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(native),
        Ok(Err(error)) => {
            native.stop();
            Err(error)
        }
        Err(_) => {
            native.stop();
            Err(LoopbackError::Worker(
                "ScreenCaptureKit did not start the audio",
            ))
        }
    }
}
