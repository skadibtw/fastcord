//! Stream audio on Linux (SPEC §8.4, milestone 36): one PipeWire capture
//! stream on a node the user chose. An output (`Audio/Sink`) is captured
//! through its monitor, which is everything it plays; an input
//! (`Audio/Source`) as it is. The session manager links the stream to the
//! node named by `target.object` and must not move it elsewhere when the node
//! goes away (`node.dont-reconnect`); the capture then ends.
//!
//! PipeWire converts whatever the node runs at to 48 kHz interleaved stereo
//! `f32`. The process callback runs on the capture thread's loop and hands
//! the samples to the handler in small stack-sized blocks, with no allocation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;

use super::{
    AudioSource, AudioTarget, CHANNELS, Handler, LoopbackError, LoopbackScope, LoopbackSupport,
    SAMPLE_RATE,
};
use crate::capture::{MAX_TITLE_CHARS, SourceId};

/// Nodes listed at once, so a busy graph cannot grow the picker.
const MAX_TARGETS: usize = 64;
/// How long the daemon may take to list its nodes or open the stream.
const ANSWER: Duration = Duration::from_secs(3);
/// Samples handed to the handler at once (a multiple of the channel count).
const BLOCK: usize = 1024;

pub(super) struct Native {
    stop: pw::channel::Sender<()>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Native {
    pub(super) fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub(super) fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn support() -> LoopbackSupport {
    LoopbackSupport {
        process_loopback: false,
        system_loopback: false,
        chosen_target: true,
    }
}

pub(super) fn window_process(_: SourceId) -> Option<u32> {
    None
}

const NO_PIPEWIRE: LoopbackError =
    LoopbackError::Unsupported("sharing audio needs a running PipeWire daemon");

pub(super) fn list_targets() -> Result<Vec<AudioSource>, LoopbackError> {
    targets(std::process::id())
}

/// The outputs and inputs on the default PipeWire daemon, outputs first,
/// leaving out nodes of process `own`.
fn targets(own: u32) -> Result<Vec<AudioSource>, LoopbackError> {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    pw::init();
    let mainloop = pw::main_loop::MainLoopBox::new(None).map_err(|_| NO_PIPEWIRE)?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None).map_err(|_| NO_PIPEWIRE)?;
    let core = context.connect(None).map_err(|_| NO_PIPEWIRE)?;
    let registry = core.get_registry().map_err(|_| NO_PIPEWIRE)?;
    // Nodes with their client, and each client's process: a node's own
    // properties do not name its process.
    let found = Rc::new(RefCell::new(Vec::new()));
    let clients = Rc::new(RefCell::new(Vec::new()));
    let _registry = registry
        .add_listener_local()
        .global({
            let found = Rc::clone(&found);
            let clients = Rc::clone(&clients);
            move |global| {
                let Some(props) = global.props.as_ref() else {
                    return;
                };
                if global.type_ == pw::types::ObjectType::Client {
                    let pid = props
                        .get("application.process.id")
                        .or_else(|| props.get("pipewire.sec.pid"))
                        .and_then(|pid| pid.parse::<u32>().ok());
                    let mut clients = clients.borrow_mut();
                    if let Some(pid) = pid
                        && clients.len() < MAX_TARGETS * 4
                    {
                        clients.push((global.id, pid));
                    }
                    return;
                }
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let whole_output = match props.get("media.class") {
                    Some("Audio/Sink") => true,
                    Some("Audio/Source") => false,
                    _ => return,
                };
                let Some(serial) = props.get("object.serial").and_then(|s| s.parse().ok()) else {
                    return;
                };
                let client = props.get("client.id").and_then(|id| id.parse::<u32>().ok());
                let name = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .or_else(|| props.get("node.name"))
                    .unwrap_or("Unnamed");
                let title = if whole_output {
                    format!("Everything “{name}” plays")
                } else {
                    format!("Input “{name}”")
                };
                let mut found = found.borrow_mut();
                if found.len() < MAX_TARGETS {
                    let source = AudioSource {
                        target: AudioTarget {
                            serial,
                            whole_output,
                        },
                        title: title.chars().take(MAX_TITLE_CHARS).collect(),
                    };
                    found.push((source, client));
                }
            }
        })
        .register();
    // Every global was announced once the daemon answers this round trip.
    let pending = core.sync(0).map_err(|_| NO_PIPEWIRE)?;
    let done = Rc::new(Cell::new(false));
    let _core = core
        .add_listener_local()
        .done({
            let done = Rc::clone(&done);
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                }
            }
        })
        .register();
    let deadline = Instant::now() + ANSWER;
    while !done.get() {
        if Instant::now() >= deadline {
            return Err(LoopbackError::Worker("PipeWire did not list its nodes"));
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
    let clients = clients.take();
    let mut found: Vec<AudioSource> = found
        .take()
        .into_iter()
        .filter(|(_, client)| {
            !clients
                .iter()
                .any(|&(id, pid)| Some(id) == *client && pid == own)
        })
        .map(|(source, _)| source)
        .collect();
    // Outputs first, each group in the daemon's order.
    found.sort_by_key(|source| !source.target.whole_output);
    Ok(found)
}

pub(super) fn start(scope: LoopbackScope, handler: Handler) -> Result<Native, LoopbackError> {
    let LoopbackScope::Target(target) = scope else {
        return Err(LoopbackError::Unsupported(
            "on Linux, choose the output or input to share audio from",
        ));
    };
    // The session manager would otherwise leave a stream for a vanished node
    // unlinked forever.
    if !targets(0)?
        .iter()
        .any(|source| source.target.serial == target.serial)
    {
        return Err(LoopbackError::TargetNotFound);
    }
    let (stop, stop_rx) = pw::channel::channel();
    let running = Arc::new(AtomicBool::new(true));
    let (ready, ready_rx) = std::sync::mpsc::sync_channel(1);
    let thread_running = Arc::clone(&running);
    let thread = std::thread::Builder::new()
        .name("fastcord-share-audio".to_owned())
        .spawn(move || {
            let result = capture(target, handler, stop_rx, &thread_running, &ready);
            thread_running.store(false, Ordering::Release);
            if let Err(error) = result {
                let _ = ready.try_send(Err(error));
            }
        })
        .map_err(|_| LoopbackError::Worker("the audio capture thread could not be started"))?;
    let native = Native {
        stop,
        running,
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(ANSWER) {
        Ok(Ok(())) => Ok(native),
        Ok(Err(error)) => {
            native.stop();
            Err(error)
        }
        Err(_) => {
            native.stop();
            Err(LoopbackError::Worker(
                "PipeWire did not open the audio stream",
            ))
        }
    }
}

fn pipewire_error(_: pw::Error) -> LoopbackError {
    LoopbackError::Worker("PipeWire refused the audio stream")
}

/// The one format asked for: PipeWire converts the node to it.
fn format() -> Result<Vec<u8>, LoopbackError> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|_| LoopbackError::Worker("the PipeWire audio format could not be built"))
}

/// Hands little-endian `f32` bytes to the handler in blocks of whole frames.
fn deliver(bytes: &[u8], handler: &mut Handler) {
    let mut block = [0.0f32; BLOCK];
    let frame = CHANNELS * 4;
    let usable = bytes.len() / frame * frame;
    for chunk in bytes[..usable].chunks(BLOCK * 4) {
        let samples = chunk.len() / 4;
        for (sample, raw) in block.iter_mut().zip(chunk.as_chunks::<4>().0) {
            *sample = f32::from_le_bytes(*raw);
        }
        handler(&block[..samples]);
    }
}

fn capture(
    target: AudioTarget,
    handler: Handler,
    stop: pw::channel::Receiver<()>,
    running: &AtomicBool,
    ready: &std::sync::mpsc::SyncSender<Result<(), LoopbackError>>,
) -> Result<(), LoopbackError> {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    pw::init();
    let mainloop = pw::main_loop::MainLoopBox::new(None).map_err(|_| NO_PIPEWIRE)?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None).map_err(|_| NO_PIPEWIRE)?;
    let core = context.connect(None).map_err(|_| NO_PIPEWIRE)?;
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::NODE_NAME => "fastcord-share-audio",
        "target.object" => target.serial.to_string(),
        "node.dont-reconnect" => "true",
    };
    if target.whole_output {
        props.insert("stream.capture.sink", "true");
    }
    let stream =
        pw::stream::StreamBox::new(&core, "fastcord-share-audio", props).map_err(pipewire_error)?;
    let ended = Rc::new(Cell::new(false));
    let handler = Rc::new(RefCell::new(handler));
    let _stop = {
        let ended = Rc::clone(&ended);
        stop.attach(mainloop.loop_(), move |()| ended.set(true))
    };
    let _listener = stream
        .add_local_listener::<()>()
        .state_changed({
            let ended = Rc::clone(&ended);
            move |_, _, _, state| {
                if matches!(
                    state,
                    pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected
                ) {
                    ended.set(true);
                }
            }
        })
        .process({
            let handler = Rc::clone(&handler);
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                if let Some(bytes) = data
                    .data()
                    .and_then(|bytes| bytes.get(offset..offset.checked_add(size)?))
                {
                    deliver(bytes, &mut handler.borrow_mut());
                }
            }
        })
        .register()
        .map_err(pipewire_error)?;
    let format = format()?;
    let mut params = [
        spa::pod::Pod::from_bytes(&format).ok_or(LoopbackError::Worker(
            "the PipeWire audio format could not be built",
        ))?,
    ];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(pipewire_error)?;
    let _ = ready.try_send(Ok(()));
    while !ended.get() && running.load(Ordering::Acquire) {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_become_whole_stereo_frames_in_bounded_blocks() {
        let samples: Vec<f32> = (0..3001).map(|n| n as f32).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut handler: Handler = Box::new({
            let seen = Arc::clone(&seen);
            move |block: &[f32]| seen.lock().push(block.to_vec())
        });
        deliver(&bytes, &mut handler);
        let seen = seen.lock();
        assert!(
            seen.iter()
                .all(|block| block.len() <= BLOCK && block.len() % 2 == 0)
        );
        let flat: Vec<f32> = seen.iter().flatten().copied().collect();
        // The odd last sample is not half a frame.
        assert_eq!(flat, samples[..3000]);
    }

    /// A 440 Hz tone on the daemon of `XDG_RUNTIME_DIR`, driving the graph
    /// itself: an `Audio/Source` named "fastcord test tone", or, with
    /// `through_sink`, a player into a new output "fastcord test output".
    fn tone(serial: std::sync::mpsc::Sender<()>, stop: Arc<AtomicBool>, through_sink: bool) {
        pw::init();
        let mainloop = pw::main_loop::MainLoopBox::new(None).unwrap();
        let context = pw::context::ContextBox::new(mainloop.loop_(), None).unwrap();
        let core = context.connect(None).unwrap();
        let _sink = through_sink.then(|| {
            core.create_object::<pw::node::Node>(
                "adapter",
                &pw::properties::properties! {
                    "factory.name" => "support.null-audio-sink",
                    *pw::keys::NODE_NAME => "fastcord-test-output",
                    *pw::keys::NODE_DESCRIPTION => "fastcord test output",
                    *pw::keys::MEDIA_CLASS => "Audio/Sink",
                    "audio.position" => "FL,FR",
                },
            )
            .unwrap()
        });
        let props = if through_sink {
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::NODE_NAME => "fastcord-test-player",
                "target.object" => "fastcord-test-output",
            }
        } else {
            pw::properties::properties! {
                *pw::keys::MEDIA_CLASS => "Audio/Source",
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::NODE_NAME => "fastcord-test-tone",
                *pw::keys::NODE_DESCRIPTION => "fastcord test tone",
            }
        };
        let stream = pw::stream::StreamBox::new(&core, "fastcord-test-tone", props).unwrap();
        let phase = std::cell::Cell::new(0.0f32);
        let _listener = stream
            .add_local_listener::<()>()
            .state_changed(move |_, _, _, state| {
                if state == pw::stream::StreamState::Paused {
                    let _ = serial.send(());
                }
            })
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let data = &mut buffer.datas_mut()[0];
                let Some(bytes) = data.data() else {
                    return;
                };
                let frames = (bytes.len() / 8).min(1024);
                for frame in bytes[..frames * 8].as_chunks_mut::<8>().0 {
                    let value = (phase.get() * std::f32::consts::TAU).sin() * 0.5;
                    phase.set((phase.get() + 440.0 / 48_000.0).fract());
                    frame[..4].copy_from_slice(&value.to_le_bytes());
                    frame[4..].copy_from_slice(&value.to_le_bytes());
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = 8;
                *chunk.size_mut() = (frames * 8) as u32;
            })
            .register()
            .unwrap();
        let format = format().unwrap();
        let flags = pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS;
        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                if through_sink {
                    flags | pw::stream::StreamFlags::AUTOCONNECT
                } else {
                    flags
                },
                &mut [spa::pod::Pod::from_bytes(&format).unwrap()],
            )
            .unwrap();
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            mainloop.loop_().iterate(pw::loop_::Timeout::Finite(
                next.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
            ));
            if Instant::now() >= next {
                next += Duration::from_millis(20);
                let _ = stream.trigger_process();
            }
        }
    }

    /// Captures `target` for a second: the stereo sample count and RMS.
    fn record(target: AudioTarget) -> (usize, f64) {
        let energy = Arc::new(parking_lot::Mutex::new((0usize, 0.0f64)));
        let capture = start(
            LoopbackScope::Target(target),
            Box::new({
                let energy = Arc::clone(&energy);
                move |block: &[f32]| {
                    let mut energy = energy.lock();
                    energy.0 += block.len();
                    energy.1 += block.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();
                }
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        assert!(capture.is_running());
        capture.stop();
        let (samples, energy) = *energy.lock();
        (samples, (energy / samples.max(1) as f64).sqrt())
    }

    #[test]
    #[ignore = "needs a PipeWire daemon and session manager on XDG_RUNTIME_DIR"]
    fn an_output_is_captured_through_its_monitor() {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let player = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || tone(ready_tx, stop, true)
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let output = targets(0)
            .unwrap()
            .into_iter()
            .find(|source| source.title.contains("fastcord test output"))
            .expect("the output is listed");
        assert!(output.target.whole_output);
        assert!(output.title.starts_with("Everything"));
        let (samples, rms) = record(output.target);
        stop.store(true, Ordering::Relaxed);
        player.join().unwrap();
        assert!(samples > 48_000, "{samples} samples");
        assert!((0.3..0.4).contains(&rms), "rms {rms}");
    }

    #[test]
    #[ignore = "needs a PipeWire daemon and session manager on XDG_RUNTIME_DIR"]
    fn a_chosen_pipewire_input_is_captured_as_48_khz_stereo_and_stops() {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let source = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || tone(ready_tx, stop, false)
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Our own process is left out of the picker; the test asks for all.
        let listed = targets(0).unwrap();
        let tone = listed
            .iter()
            .find(|source| source.title.contains("fastcord test tone"))
            .expect("the tone is listed");
        assert!(!tone.target.whole_output);
        assert!(
            !list_targets()
                .unwrap()
                .iter()
                .any(|source| source.title.contains("fastcord test tone")),
            "fastcord's own nodes are never offered"
        );

        let (samples, rms) = record(tone.target);
        stop.store(true, Ordering::Relaxed);
        source.join().unwrap();
        // About one second of stereo; a 0.5 sine has RMS 0.354.
        assert!(samples > 48_000, "{samples} samples");
        assert!((0.3..0.4).contains(&rms), "rms {rms}");

        let gone = AudioTarget {
            serial: u32::MAX,
            whole_output: false,
        };
        assert_eq!(
            start(LoopbackScope::Target(gone), Box::new(|_| {})).err(),
            Some(LoopbackError::TargetNotFound)
        );
    }
}
