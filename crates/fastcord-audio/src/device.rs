//! Audio devices: enumeration, stable identity, explicit selection, stream
//! configuration, and opening cpal streams.
//!
//! Enumeration and stream creation block on the OS audio service; call them
//! off the UI thread. A saved device is identified by cpal's stable backend ID
//! (for example `wasapi:{0.0.1.00000000}.{…}`), never by its display label, and
//! a missing saved device is an error — it is never silently replaced by the
//! system default (SPEC §7.3).

use std::str::FromStr;
use std::sync::Arc;
use std::sync::mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, FromSample, Sample, SampleFormat, SizedSample, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig, SupportedStreamConfigRange,
};

use crate::callback::{InputCallback, OutputCallback, StreamCounters};
use crate::error::AudioError;

/// Capture or playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Input,
    Output,
}

/// Stable backend identifier of a device, suitable for persistence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceKey(String);

impl DeviceKey {
    /// Restores a key previously obtained from [`DeviceKey::as_str`].
    pub fn from_persisted(key: impl Into<String>) -> Option<Self> {
        let key = key.into();
        (!key.is_empty() && key.len() <= 512).then_some(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which device a stream should use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum DeviceChoice {
    /// Whatever the OS currently designates as the default device.
    #[default]
    SystemDefault,
    /// One specific device; if it is absent, opening fails.
    Device(DeviceKey),
}

/// One enumerated device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub key: DeviceKey,
    /// Human-readable name for display only.
    pub label: String,
    /// Whether this is currently the system default for its direction.
    pub is_default: bool,
}

/// Why a device or stream failed, independent of the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DeviceErrorKind {
    /// The device was unplugged or no longer exists.
    Disconnected,
    /// Another application holds the device exclusively.
    Busy,
    /// The OS denied access (for example microphone privacy settings).
    PermissionDenied,
    /// The OS audio service is not running.
    HostUnavailable,
    /// The stream must be rebuilt (format changed underneath it).
    Invalidated,
    /// The device cannot run any configuration the engine can use.
    Unsupported,
    /// Any other backend failure.
    Other,
}

impl DeviceErrorKind {
    pub(crate) fn from_cpal(kind: cpal::ErrorKind) -> Self {
        use cpal::ErrorKind as K;
        match kind {
            K::DeviceNotAvailable => Self::Disconnected,
            K::DeviceBusy => Self::Busy,
            K::PermissionDenied => Self::PermissionDenied,
            K::HostUnavailable => Self::HostUnavailable,
            K::StreamInvalidated | K::DeviceChanged => Self::Invalidated,
            K::UnsupportedConfig | K::UnsupportedOperation => Self::Unsupported,
            _ => Self::Other,
        }
    }

    pub(crate) fn from_code(code: u8) -> Self {
        const ALL: [DeviceErrorKind; 7] = [
            DeviceErrorKind::Disconnected,
            DeviceErrorKind::Busy,
            DeviceErrorKind::PermissionDenied,
            DeviceErrorKind::HostUnavailable,
            DeviceErrorKind::Invalidated,
            DeviceErrorKind::Unsupported,
            DeviceErrorKind::Other,
        ];
        ALL.get(usize::from(code))
            .copied()
            .unwrap_or(DeviceErrorKind::Other)
    }
}

impl std::fmt::Display for DeviceErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Disconnected => "the device is disconnected",
            Self::Busy => "the device is in use by another application",
            Self::PermissionDenied => "access to the device was denied",
            Self::HostUnavailable => "the system audio service is unavailable",
            Self::Invalidated => "the device configuration changed",
            Self::Unsupported => "the device format is not supported",
            Self::Other => "the audio device failed",
        })
    }
}

impl DeviceErrorKind {
    /// What the user can do about it, in one sentence.
    pub fn advice(self) -> &'static str {
        match self {
            Self::Disconnected => {
                "the device was not found. Plug it in again or choose another device."
            }
            Self::Busy => {
                "another application is using the device exclusively. Close it, or turn off exclusive mode for this device in the system sound settings."
            }
            Self::PermissionDenied => {
                "access was denied. Allow microphone access for desktop applications in the system privacy settings (Windows: Settings > Privacy & security > Microphone; macOS: System Settings > Privacy & Security > Microphone)."
            }
            Self::HostUnavailable => {
                "the system audio service is not running. Restart it or restart the computer."
            }
            Self::Invalidated => {
                "the device changed its format while in use. fastcord reopens it on its own; if it stays silent, select it again."
            }
            Self::Unsupported => {
                "the device offers no format fastcord can use. Choose another device, or another sample rate in the system sound settings."
            }
            Self::Other => "the device failed. Select it again or choose another device.",
        }
    }
}

/// Lists the devices available for `direction` on the platform's default
/// audio host. Blocking.
pub fn list_devices(direction: Direction) -> Result<Vec<DeviceInfo>, AudioError> {
    let host = cpal::default_host();
    let default_key = default_device(&host, direction)
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let devices = match direction {
        Direction::Input => host.input_devices(),
        Direction::Output => host.output_devices(),
    }
    .map_err(|error| AudioError::backend(direction, &error))?;
    let mut list = Vec::new();
    for device in devices.take(64) {
        // A device that vanished mid-enumeration has no ID; skip it.
        let Ok(id) = device.id() else { continue };
        let Some(key) = DeviceKey::from_persisted(id.to_string()) else {
            continue;
        };
        let mut label = device_label(&device);
        while label.len() > 256 {
            label.pop();
        }
        list.push(DeviceInfo {
            is_default: default_key.as_deref() == Some(key.as_str()),
            label,
            key,
        });
    }
    Ok(list)
}

fn device_label(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_owned())
        .unwrap_or_else(|_| device.to_string())
}

fn default_device(host: &cpal::Host, direction: Direction) -> Option<cpal::Device> {
    match direction {
        Direction::Input => host.default_input_device(),
        Direction::Output => host.default_output_device(),
    }
}

fn resolve(
    host: &cpal::Host,
    direction: Direction,
    choice: &DeviceChoice,
) -> Result<cpal::Device, AudioError> {
    match choice {
        DeviceChoice::SystemDefault => {
            default_device(host, direction).ok_or(AudioError::NoDefaultDevice(direction))
        }
        DeviceChoice::Device(key) => cpal::DeviceId::from_str(key.as_str())
            .ok()
            .and_then(|id| host.device_by_id(&id))
            .filter(|device| match direction {
                Direction::Input => device.supports_input(),
                Direction::Output => device.supports_output(),
            })
            .ok_or(AudioError::DeviceNotFound(direction)),
    }
}

/// The format a stream actually runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamFormat {
    pub(crate) rate: u32,
    pub(crate) channels: usize,
    /// Frames per callback as reported by the backend, when known.
    pub(crate) period_frames: Option<u32>,
}

/// A concrete stream configuration chosen for a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamPlan {
    pub(crate) config: StreamConfig,
    pub(crate) format: SampleFormat,
}

/// Preferred sample formats, best first. f32 matches the engine; integer
/// formats convert exactly; DSD is never used.
const FORMAT_PREFERENCE: [SampleFormat; 12] = [
    SampleFormat::F32,
    SampleFormat::I16,
    SampleFormat::I32,
    SampleFormat::I24,
    SampleFormat::F64,
    SampleFormat::U16,
    SampleFormat::I8,
    SampleFormat::U8,
    SampleFormat::U24,
    SampleFormat::U32,
    SampleFormat::I64,
    SampleFormat::U64,
];

/// Picks a stream configuration: the device's default channel count (stereo
/// for output when offered), 48 kHz when supported (no resampling), otherwise
/// the default rate, otherwise the lowest supported rate above 48 kHz (no
/// bandwidth loss) or failing that the highest below it, and the most precise
/// convenient sample format. A 10 ms callback period is requested when the
/// device reports that it supports one.
pub(crate) fn plan_stream(
    direction: Direction,
    ranges: &[SupportedStreamConfigRange],
    default: Option<&SupportedStreamConfig>,
) -> Option<StreamPlan> {
    const RATE: u32 = crate::codec::SAMPLE_RATE;
    let default_channels = default.map(SupportedStreamConfig::channels);
    let wanted_channels = match direction {
        Direction::Output if ranges.iter().any(|r| r.channels() == 2) => Some(2),
        _ => default_channels,
    };
    let default_rate = default.map(SupportedStreamConfig::sample_rate);
    ranges
        .iter()
        .filter(|range| range.channels() > 0)
        .filter_map(|range| {
            let format_rank = FORMAT_PREFERENCE
                .iter()
                .position(|&f| f == range.sample_format())?;
            let (rate, rate_rank) = if range.contains_rate(RATE) {
                (RATE, 0)
            } else if let Some(rate) = default_rate.filter(|&r| range.contains_rate(r)) {
                (rate, 1)
            } else {
                let rate = RATE.clamp(range.min_sample_rate(), range.max_sample_rate());
                (rate, 2)
            };
            let channel_rank = u8::from(Some(range.channels()) != wanted_channels);
            // Below 48 kHz loses voice bandwidth, so any higher rate wins.
            let distance = (rate < RATE, rate.abs_diff(RATE));
            let key = (channel_rank, rate_rank, distance, format_rank);
            Some((key, range, rate))
        })
        .min_by_key(|(key, _, _)| *key)
        .map(|(_, range, rate)| {
            let period = rate / 100;
            let buffer_size = match *range.buffer_size() {
                SupportedBufferSize::Range { min, max } if (min..=max).contains(&period) => {
                    BufferSize::Fixed(period)
                }
                _ => BufferSize::Default,
            };
            StreamPlan {
                config: StreamConfig {
                    channels: range.channels(),
                    sample_rate: rate,
                    buffer_size,
                },
                format: range.sample_format(),
            }
        })
}

/// Opens device streams for the engine thread. The engine is generic over
/// this so its lifecycle can be exercised without hardware.
pub(crate) trait Backend {
    /// Dropping a stream stops it and releases the application's callback
    /// resources before returning; backend disposal may finish separately.
    type Stream;

    /// The current default endpoint identity; queried only during an active call.
    fn default_key(&mut self, direction: Direction) -> Option<DeviceKey>;

    fn open_input(
        &mut self,
        choice: &DeviceChoice,
        counters: &Arc<StreamCounters>,
        make: &mut dyn FnMut(StreamFormat) -> InputCallback,
    ) -> Result<(Self::Stream, StreamFormat), AudioError>;

    fn open_output(
        &mut self,
        choice: &DeviceChoice,
        counters: &Arc<StreamCounters>,
        make: &mut dyn FnMut(StreamFormat) -> OutputCallback,
    ) -> Result<(Self::Stream, StreamFormat), AudioError>;
}

/// The platform's native audio host through cpal.
pub(crate) struct CpalBackend {
    host: cpal::Host,
}

/// cpal's CoreAudio monitors shut down asynchronously when their stream is
/// dropped. Keep teardown on the owner until both data and error closures have
/// released their resources, including any last monitor-held stream reference.
/// No channel operation takes place while invoking either callback.
pub(crate) struct RetiringStream<S> {
    stream: Option<S>,
    retired: mpsc::Receiver<()>,
}

type CpalStream = RetiringStream<cpal::Stream>;

impl<S> RetiringStream<S> {
    fn build<E>(
        build: impl FnOnce(mpsc::SyncSender<()>, mpsc::SyncSender<()>) -> Result<S, E>,
    ) -> Result<Self, E> {
        let (retirement, retired) = mpsc::sync_channel(0);
        // Only successful builds have a stream whose drop initiates shutdown.
        // Do not wait after an error: a backend may retain a callback from a
        // failed native registration, without providing any cleanup handle.
        let stream = build(retirement.clone(), retirement)?;
        Ok(Self {
            stream: Some(stream),
            retired,
        })
    }
}

impl<S> Drop for RetiringStream<S> {
    fn drop(&mut self) {
        drop(self.stream.take());
        // Nobody sends a message; disconnection proves both closures retired.
        let _ = self.retired.recv();
    }
}

/// Fields drop in declaration order: release callback resources before waking
/// the owner. Access through a method keeps the complete owner in the closure
/// (Rust's disjoint closure capture must not discard the retirement sender).
struct CallbackOwner<C> {
    callback: C,
    _retirement: mpsc::SyncSender<()>,
}

impl<C> CallbackOwner<C> {
    fn callback(&mut self) -> &mut C {
        &mut self.callback
    }
}

impl CpalBackend {
    pub(crate) fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }

    fn plan(device: &cpal::Device, direction: Direction) -> Result<StreamPlan, AudioError> {
        let (ranges, default) = match direction {
            Direction::Input => (
                device
                    .supported_input_configs()
                    .map(Iterator::collect::<Vec<_>>),
                device.default_input_config().ok(),
            ),
            Direction::Output => (
                device
                    .supported_output_configs()
                    .map(Iterator::collect::<Vec<_>>),
                device.default_output_config().ok(),
            ),
        };
        let ranges = ranges.map_err(|error| AudioError::backend(direction, &error))?;
        plan_stream(direction, &ranges, default.as_ref())
            .ok_or(AudioError::Device(direction, DeviceErrorKind::Unsupported))
    }

    /// Builds and starts a stream, retrying with the backend's default
    /// period if a fixed 10 ms period is refused.
    fn start<C>(
        device: &cpal::Device,
        direction: Direction,
        plan: StreamPlan,
        make: &mut dyn FnMut(StreamFormat) -> C,
        build: fn(
            &cpal::Device,
            StreamPlan,
            C,
            Arc<StreamCounters>,
        ) -> Result<CpalStream, cpal::Error>,
        counters: &Arc<StreamCounters>,
    ) -> Result<(CpalStream, StreamFormat), AudioError> {
        let mut plan = plan;
        let format = |plan: StreamPlan, period: Option<u32>| StreamFormat {
            rate: plan.config.sample_rate,
            channels: usize::from(plan.config.channels),
            period_frames: period,
        };
        let stream = loop {
            let requested = match plan.config.buffer_size {
                BufferSize::Fixed(frames) => Some(frames),
                BufferSize::Default => None,
            };
            let callback = make(format(plan, requested));
            match build(device, plan, callback, Arc::clone(counters)) {
                Ok(stream) => break stream,
                Err(_) if plan.config.buffer_size != BufferSize::Default => {
                    plan.config.buffer_size = BufferSize::Default;
                }
                Err(error) => return Err(AudioError::backend(direction, &error)),
            }
        };
        let native = stream.stream.as_ref().expect("successfully built stream");
        let period = native.buffer_size().ok();
        native
            .play()
            .map_err(|error| AudioError::backend(direction, &error))?;
        Ok((stream, format(plan, period)))
    }
}

macro_rules! dispatch_format {
    ($format:expr, $function:ident, $($arg:expr),*) => {
        match $format {
            SampleFormat::I8 => $function::<i8>($($arg),*),
            SampleFormat::I16 => $function::<i16>($($arg),*),
            SampleFormat::I24 => $function::<cpal::I24>($($arg),*),
            SampleFormat::I32 => $function::<i32>($($arg),*),
            SampleFormat::I64 => $function::<i64>($($arg),*),
            SampleFormat::U8 => $function::<u8>($($arg),*),
            SampleFormat::U16 => $function::<u16>($($arg),*),
            SampleFormat::U24 => $function::<cpal::U24>($($arg),*),
            SampleFormat::U32 => $function::<u32>($($arg),*),
            SampleFormat::U64 => $function::<u64>($($arg),*),
            SampleFormat::F32 => $function::<f32>($($arg),*),
            SampleFormat::F64 => $function::<f64>($($arg),*),
            _ => Err(cpal::Error::new(cpal::ErrorKind::UnsupportedConfig)),
        }
    };
}

fn build_input(
    device: &cpal::Device,
    plan: StreamPlan,
    callback: InputCallback,
    counters: Arc<StreamCounters>,
) -> Result<CpalStream, cpal::Error> {
    fn typed<T>(
        device: &cpal::Device,
        config: StreamConfig,
        callback: InputCallback,
        counters: Arc<StreamCounters>,
    ) -> Result<CpalStream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        RetiringStream::build(|data_retirement, error_retirement| {
            let mut callback = CallbackOwner {
                callback,
                _retirement: data_retirement,
            };
            let mut counters = CallbackOwner {
                callback: counters,
                _retirement: error_retirement,
            };
            device.build_input_stream::<T, _, _>(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| callback.callback().process(data),
                move |error: cpal::Error| counters.callback().record_error(error.kind()),
                None,
            )
        })
    }
    dispatch_format!(plan.format, typed, device, plan.config, callback, counters)
}

fn build_output(
    device: &cpal::Device,
    plan: StreamPlan,
    callback: OutputCallback,
    counters: Arc<StreamCounters>,
) -> Result<CpalStream, cpal::Error> {
    fn typed<T>(
        device: &cpal::Device,
        config: StreamConfig,
        callback: OutputCallback,
        counters: Arc<StreamCounters>,
    ) -> Result<CpalStream, cpal::Error>
    where
        T: SizedSample + Sample + FromSample<f32>,
    {
        RetiringStream::build(|data_retirement, error_retirement| {
            let mut callback = CallbackOwner {
                callback,
                _retirement: data_retirement,
            };
            let mut counters = CallbackOwner {
                callback: counters,
                _retirement: error_retirement,
            };
            device.build_output_stream::<T, _, _>(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    callback.callback().process(data)
                },
                move |error: cpal::Error| counters.callback().record_error(error.kind()),
                None,
            )
        })
    }
    dispatch_format!(plan.format, typed, device, plan.config, callback, counters)
}

impl Backend for CpalBackend {
    type Stream = CpalStream;

    fn default_key(&mut self, direction: Direction) -> Option<DeviceKey> {
        default_device(&self.host, direction)
            .and_then(|device| device.id().ok())
            .and_then(|id| DeviceKey::from_persisted(id.to_string()))
    }

    fn open_input(
        &mut self,
        choice: &DeviceChoice,
        counters: &Arc<StreamCounters>,
        make: &mut dyn FnMut(StreamFormat) -> InputCallback,
    ) -> Result<(Self::Stream, StreamFormat), AudioError> {
        let device = resolve(&self.host, Direction::Input, choice)?;
        let plan = Self::plan(&device, Direction::Input)?;
        Self::start(&device, Direction::Input, plan, make, build_input, counters)
    }

    fn open_output(
        &mut self,
        choice: &DeviceChoice,
        counters: &Arc<StreamCounters>,
        make: &mut dyn FnMut(StreamFormat) -> OutputCallback,
    ) -> Result<(Self::Stream, StreamFormat), AudioError> {
        let device = resolve(&self.host, Direction::Output, choice)?;
        let plan = Self::plan(&device, Direction::Output)?;
        Self::start(
            &device,
            Direction::Output,
            plan,
            make,
            build_output,
            counters,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_waits_for_detached_data_and_error_callbacks() {
        let counters = Arc::new(StreamCounters::default());
        let probe = Arc::downgrade(&counters);
        let (shutdown, stopped) = mpsc::channel::<()>();
        let (release_data, data_release) = mpsc::channel::<()>();
        let (release_error, error_release) = mpsc::channel::<()>();
        let (data_retired, data_done) = mpsc::channel();
        let (dropping, drop_started) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let stream = RetiringStream::build(|data_retirement, error_retirement| {
            let data = CallbackOwner {
                callback: Arc::clone(&counters),
                _retirement: data_retirement,
            };
            let error = CallbackOwner {
                callback: counters,
                _retirement: error_retirement,
            };
            std::thread::spawn(move || {
                assert!(stopped.recv().is_err());
                dropping.send(()).unwrap();
                data_release.recv().unwrap();
                drop(data);
                data_retired.send(()).unwrap();
                error_release.recv().unwrap();
                drop(error);
            });
            Ok::<_, ()>(shutdown)
        })
        .unwrap();
        let owner = std::thread::spawn(move || {
            drop(stream);
            done.send(probe.upgrade().is_none()).unwrap();
        });
        drop_started.recv().unwrap();
        assert!(matches!(
            finished.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_data.send(()).unwrap();
        data_done.recv().unwrap();
        assert!(matches!(
            finished.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_error.send(()).unwrap();
        assert!(
            finished.recv().unwrap(),
            "callback resources survived teardown"
        );
        owner.join().unwrap();
    }

    #[test]
    fn failed_build_returns_without_waiting_for_an_unowned_callback() {
        let counters = Arc::new(StreamCounters::default());
        let probe = Arc::downgrade(&counters);
        let mut retained = None;
        let result = RetiringStream::<()>::build(|data_retirement, error_retirement| {
            drop(data_retirement);
            retained = Some(CallbackOwner {
                callback: counters,
                _retirement: error_retirement,
            });
            Err("failed to open stream")
        });
        assert!(matches!(result, Err("failed to open stream")));
        assert!(probe.upgrade().is_some());
        drop(retained);
        assert!(probe.upgrade().is_none());
    }

    #[test]
    fn retirement_tracking_does_not_touch_the_heap_in_callbacks() {
        let (retirement, retired) = mpsc::sync_channel(0);
        let counters = Arc::new(StreamCounters::default());
        let (producer, _consumer) = crate::ring::sample_ring(1, 480);
        let mut input = CallbackOwner {
            callback: InputCallback::new(
                producer,
                Arc::clone(&counters),
                Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ),
            _retirement: retirement.clone(),
        };
        let mut error = CallbackOwner {
            callback: counters,
            _retirement: retirement,
        };
        let before = crate::alloc_counter::thread_callback_heap_ops();
        {
            let _scope = crate::audit::CallbackScope::enter();
            input.callback().process(&[0.25f32; 480]);
            error.callback().record_error(cpal::ErrorKind::Xrun);
        }
        assert_eq!(crate::alloc_counter::thread_callback_heap_ops(), before);
        drop(input);
        drop(error);
        assert!(matches!(
            retired.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
        buffer: SupportedBufferSize,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, min, max, buffer, format)
    }

    fn default_config(channels: u16, rate: u32, format: SampleFormat) -> SupportedStreamConfig {
        SupportedStreamConfig::new(channels, rate, SupportedBufferSize::Unknown, format)
    }

    const UNKNOWN: SupportedBufferSize = SupportedBufferSize::Unknown;

    #[test]
    fn prefers_48k_float_at_the_default_channel_count_for_input() {
        let ranges = [
            range(2, 44_100, 44_100, SampleFormat::F32, UNKNOWN),
            range(1, 8_000, 96_000, SampleFormat::I16, UNKNOWN),
            range(1, 8_000, 96_000, SampleFormat::F32, UNKNOWN),
        ];
        let default = default_config(1, 44_100, SampleFormat::I16);
        let plan = plan_stream(Direction::Input, &ranges, Some(&default)).unwrap();
        assert_eq!(plan.format, SampleFormat::F32);
        assert_eq!(plan.config.channels, 1);
        assert_eq!(plan.config.sample_rate, 48_000);
        assert_eq!(plan.config.buffer_size, BufferSize::Default);
    }

    #[test]
    fn output_prefers_stereo_over_a_surround_default() {
        let ranges = [
            range(6, 48_000, 48_000, SampleFormat::F32, UNKNOWN),
            range(2, 48_000, 48_000, SampleFormat::I16, UNKNOWN),
        ];
        let default = default_config(6, 48_000, SampleFormat::F32);
        let plan = plan_stream(Direction::Output, &ranges, Some(&default)).unwrap();
        assert_eq!(plan.config.channels, 2);
        assert_eq!(plan.format, SampleFormat::I16);
    }

    #[test]
    fn falls_back_to_the_default_rate_then_the_closest_rate() {
        let ranges = [range(2, 44_100, 44_100, SampleFormat::I16, UNKNOWN)];
        let default = default_config(2, 44_100, SampleFormat::I16);
        let plan = plan_stream(Direction::Output, &ranges, Some(&default)).unwrap();
        assert_eq!(plan.config.sample_rate, 44_100);

        let ranges = [
            range(1, 8_000, 16_000, SampleFormat::I16, UNKNOWN),
            range(1, 88_200, 192_000, SampleFormat::I16, UNKNOWN),
        ];
        let plan = plan_stream(Direction::Input, &ranges, None).unwrap();
        assert_eq!(plan.config.sample_rate, 88_200);
    }

    #[test]
    fn dsd_only_devices_are_unusable() {
        let ranges = [range(2, 48_000, 48_000, SampleFormat::DsdU8, UNKNOWN)];
        assert_eq!(plan_stream(Direction::Output, &ranges, None), None);
        assert_eq!(plan_stream(Direction::Output, &[], None), None);
    }

    #[test]
    fn requests_a_10ms_period_only_when_the_device_supports_it() {
        let ranges = [range(
            1,
            48_000,
            48_000,
            SampleFormat::F32,
            SupportedBufferSize::Range {
                min: 64,
                max: 4_096,
            },
        )];
        let plan = plan_stream(Direction::Input, &ranges, None).unwrap();
        assert_eq!(plan.config.buffer_size, BufferSize::Fixed(480));

        let ranges = [range(
            1,
            48_000,
            48_000,
            SampleFormat::F32,
            SupportedBufferSize::Range {
                min: 1_024,
                max: 4_096,
            },
        )];
        let plan = plan_stream(Direction::Input, &ranges, None).unwrap();
        assert_eq!(plan.config.buffer_size, BufferSize::Default);
    }

    #[test]
    fn error_kinds_round_trip_through_their_codes() {
        for code in 0..7 {
            assert_eq!(DeviceErrorKind::from_code(code) as u8, code);
        }
        assert_eq!(DeviceErrorKind::from_code(200), DeviceErrorKind::Other);
        assert_eq!(
            DeviceErrorKind::from_cpal(cpal::ErrorKind::DeviceNotAvailable),
            DeviceErrorKind::Disconnected
        );
    }

    #[test]
    fn a_saved_device_that_is_absent_is_an_error_not_the_default() {
        let host = cpal::default_host();
        let malformed = DeviceChoice::Device(DeviceKey::from_persisted("not a device id").unwrap());
        let absent = DeviceChoice::Device(
            DeviceKey::from_persisted(
                cpal::DeviceId::new(host.id(), "fastcord-absent-device").to_string(),
            )
            .unwrap(),
        );
        for choice in [malformed, absent] {
            assert_eq!(
                resolve(&host, Direction::Output, &choice).err(),
                Some(AudioError::DeviceNotFound(Direction::Output))
            );
        }
    }

    /// Real devices: enumeration finds a default per direction and its saved
    /// key reopens exactly that device.
    #[test]
    #[ignore = "needs real audio devices"]
    fn live_lists_devices_with_reopenable_keys() {
        let host = cpal::default_host();
        println!("host: {:?}", host.id());
        for direction in [Direction::Input, Direction::Output] {
            let devices = list_devices(direction).unwrap();
            for device in &devices {
                let marker = if device.is_default { " (default)" } else { "" };
                println!("{direction:?}: {}{marker}", device.label);
            }
            let default = devices
                .iter()
                .find(|device| device.is_default)
                .expect("a default device");
            let reopened = resolve(&host, direction, &DeviceChoice::Device(default.key.clone()))
                .expect("saved key reopens the device");
            assert_eq!(reopened.id().unwrap().to_string(), default.key.as_str());
        }
    }
}
