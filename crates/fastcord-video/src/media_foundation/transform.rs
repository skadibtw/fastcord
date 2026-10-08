//! Drives one Media Foundation transform through the synchronous or the
//! asynchronous (hardware) processing model.

use std::mem::ManuallyDrop;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFAttributes,
    IMFCollection, IMFMediaBuffer, IMFMediaEvent, IMFMediaEventGenerator, IMFSample, IMFTransform,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_E_UNSUPPORTED_D3D_TYPE,
    MF_EVENT_TYPE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFT_INPUT_STREAM_INFO,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
    MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
};
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

use super::runtime::{OrPlatform, Session};
use crate::codec::CodecError;

/// Receives what a transform produces.
pub(super) trait Sink {
    /// Selects a new output type on `mft`'s `output_id` stream after a stream
    /// change or a hardware-to-software renegotiation.
    fn stream_changed(&mut self, mft: &IMFTransform, output_id: u32) -> Result<(), CodecError>;

    fn output(&mut self, sample: &IMFSample) -> Result<(), CodecError>;

    /// Asynchronous transforms only: whether every output the sink waits for
    /// has arrived, so [`Transform::push`] may return before the transform
    /// asks for more input.
    fn caught_up(&self) -> bool;
}

/// One transform and its processing state.
pub(super) struct Transform {
    mft: IMFTransform,
    activate: IMFActivate,
    /// The event queue of an asynchronous transform.
    events: Option<EventQueue>,
    input_id: u32,
    output_id: u32,
    /// Asynchronous only: input requests not yet answered.
    need_input: u32,
    output_info: MFT_OUTPUT_STREAM_INFO,
    /// The reusable output sample for transforms that do not allocate their
    /// own.
    output_sample: Option<(IMFSample, IMFMediaBuffer)>,
    streaming: bool,
    software_fallback: bool,
}

/// What one `ProcessOutput` call produced.
enum Step {
    Output,
    NeedMoreInput,
    StreamChanged,
    NoOutput,
}

impl Transform {
    /// Activates the transform; an asynchronous one is unlocked for use.
    pub(super) fn activate(activate: IMFActivate) -> Result<Self, CodecError> {
        // SAFETY: COM calls on valid interfaces; `Session` keeps COM and Media
        // Foundation initialized on this thread for the caller's lifetime.
        unsafe {
            let mft: IMFTransform = activate
                .ActivateObject()
                .or_platform("IMFActivate::ActivateObject")?;
            let mut transform = Self {
                mft,
                activate,
                events: None,
                input_id: 0,
                output_id: 0,
                need_input: 0,
                output_info: MFT_OUTPUT_STREAM_INFO::default(),
                output_sample: None,
                streaming: false,
                software_fallback: false,
            };
            if let Ok(attributes) = transform.mft.GetAttributes()
                && attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0
            {
                attributes
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .or_platform("MF_TRANSFORM_ASYNC_UNLOCK")?;
                let generator = transform
                    .mft
                    .cast()
                    .or_platform("IMFTransform as IMFMediaEventGenerator")?;
                transform.events = Some(EventQueue::new(generator));
            }
            let (mut input, mut output) = ([0u32], [0u32]);
            match transform.mft.GetStreamIDs(&mut input, &mut output) {
                Ok(()) => (transform.input_id, transform.output_id) = (input[0], output[0]),
                // Fixed stream counts use IDs 0 and 0.
                Err(error) if error.code() == E_NOTIMPL => {}
                Err(error) => return Err(error).or_platform("IMFTransform::GetStreamIDs"),
            }
            Ok(transform)
        }
    }

    pub(super) fn mft(&self) -> &IMFTransform {
        &self.mft
    }

    pub(super) fn input_id(&self) -> u32 {
        self.input_id
    }

    pub(super) fn output_id(&self) -> u32 {
        self.output_id
    }

    pub(super) fn attributes(&self) -> Option<IMFAttributes> {
        // SAFETY: COM call on a valid interface.
        unsafe { self.mft.GetAttributes() }.ok()
    }

    pub(super) fn input_alignment(&self) -> Result<u32, CodecError> {
        let mut info = MFT_INPUT_STREAM_INFO::default();
        // SAFETY: `info` is a valid output structure for this COM call.
        unsafe { self.mft.GetInputStreamInfo(self.input_id, &mut info) }
            .map(|()| info.cbAlignment.max(1))
            .or_platform("IMFTransform::GetInputStreamInfo")
    }

    pub(super) fn software_fallback(&self) -> bool {
        self.software_fallback
    }

    fn detach_d3d_manager(&mut self, sink: &mut dyn Sink) -> Result<(), CodecError> {
        // The Media Foundation D3D11 decode contract requires retrying on the
        // same MFT after detaching the manager, then renegotiating the output.
        // SAFETY: the message accepts a null manager pointer to request
        // software decoding on this valid transform.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0) }
            .or_platform("MFT_MESSAGE_SET_D3D_MANAGER")?;
        sink.stream_changed(&self.mft, self.output_id)?;
        self.refresh_output_info()?;
        self.software_fallback = true;
        Ok(())
    }

    /// Starts streaming once the media types are set.
    pub(super) fn start(&mut self) -> Result<(), CodecError> {
        self.refresh_output_info()?;
        // SAFETY: COM calls on a valid interface; the messages take no
        // parameter.
        unsafe {
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .or_platform("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
            self.streaming = true;
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .or_platform("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;
        }
        Ok(())
    }

    fn refresh_output_info(&mut self) -> Result<(), CodecError> {
        // SAFETY: COM call on a valid interface.
        self.output_info = unsafe { self.mft.GetOutputStreamInfo(self.output_id) }
            .or_platform("IMFTransform::GetOutputStreamInfo")?;
        // A different output format may need a larger sample.
        self.output_sample = None;
        Ok(())
    }

    /// Feeds one input sample and delivers the output that becomes available.
    pub(super) fn push(
        &mut self,
        session: &Session,
        sample: &IMFSample,
        sink: &mut dyn Sink,
        allow_software_fallback: bool,
    ) -> Result<(), CodecError> {
        if self.events.is_some() {
            while self.need_input == 0 {
                self.next_event(session, sink, allow_software_fallback)?;
            }
            loop {
                match self.process_input(sample) {
                    Ok(()) => break,
                    Err(error)
                        if error.code() == MF_E_UNSUPPORTED_D3D_TYPE
                            && allow_software_fallback
                            && !self.software_fallback =>
                    {
                        self.detach_d3d_manager(sink)?;
                    }
                    Err(error) => return Err(error).or_platform("IMFTransform::ProcessInput"),
                }
            }
            self.need_input -= 1;
            // Low latency: wait for this input's output unless the transform
            // asks for more input first (it is buffering).
            while !sink.caught_up() && self.need_input == 0 {
                self.next_event(session, sink, allow_software_fallback)?;
            }
            return Ok(());
        }
        loop {
            // SAFETY: COM call with a valid sample.
            match unsafe { self.mft.ProcessInput(self.input_id, sample, 0) } {
                Ok(()) => break,
                Err(error) if error.code() == MF_E_NOTACCEPTING => {
                    let fallback_before = self.software_fallback;
                    let delivered = self.pull_all(session, sink, allow_software_fallback)?;
                    if delivered == 0 && self.software_fallback == fallback_before {
                        return Err(CodecError::Platform {
                            operation: "IMFTransform::ProcessInput",
                            code: error.code().0,
                        });
                    }
                }
                Err(error)
                    if error.code() == MF_E_UNSUPPORTED_D3D_TYPE
                        && allow_software_fallback
                        && !self.software_fallback =>
                {
                    self.detach_d3d_manager(sink)?;
                }
                Err(error) => return Err(error).or_platform("IMFTransform::ProcessInput"),
            }
        }
        self.pull_all(session, sink, allow_software_fallback)
            .map(drop)
    }

    /// Delivers all pending output. The transform accepts input afterwards.
    pub(super) fn drain(
        &mut self,
        session: &Session,
        sink: &mut dyn Sink,
        allow_software_fallback: bool,
    ) -> Result<(), CodecError> {
        loop {
            // SAFETY: COM call on a valid interface; the drain message takes no
            // parameter.
            match unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) } {
                Ok(()) => break,
                Err(error)
                    if error.code() == MF_E_UNSUPPORTED_D3D_TYPE
                        && allow_software_fallback
                        && !self.software_fallback =>
                {
                    self.detach_d3d_manager(sink)?;
                }
                Err(error) => return Err(error).or_platform("MFT_MESSAGE_COMMAND_DRAIN"),
            }
        }
        if self.events.is_none() {
            return self
                .pull_all(session, sink, allow_software_fallback)
                .map(drop);
        }
        while !self.next_event(session, sink, allow_software_fallback)? {}
        // Input requests from before the drain are void. A drained
        // asynchronous transform asks for input again only once a new stream
        // starts.
        self.need_input = 0;
        // SAFETY: COM call on a valid interface; the message takes no
        // parameter.
        unsafe {
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
        .or_platform("MFT_MESSAGE_NOTIFY_START_OF_STREAM")
    }

    fn process_input(&self, sample: &IMFSample) -> windows::core::Result<()> {
        // SAFETY: COM call with a valid sample.
        unsafe { self.mft.ProcessInput(self.input_id, sample, 0) }
    }

    /// Asynchronous only: waits for and handles one event. Returns whether it
    /// was the end of a drain.
    fn next_event(
        &mut self,
        session: &Session,
        sink: &mut dyn Sink,
        allow_software_fallback: bool,
    ) -> Result<bool, CodecError> {
        let Some(events) = &mut self.events else {
            return Ok(true);
        };
        let kind = events.next()?;
        if kind == METransformNeedInput {
            self.need_input += 1;
        } else if kind == METransformHaveOutput {
            loop {
                match self.process_output(session, sink) {
                    Ok(Step::StreamChanged) => {}
                    Err(error)
                        if is_unsupported_d3d_type(&error)
                            && allow_software_fallback
                            && !self.software_fallback =>
                    {
                        self.detach_d3d_manager(sink)?;
                    }
                    Ok(_) => break,
                    Err(error) => return Err(error),
                }
            }
        } else if kind == METransformDrainComplete {
            return Ok(true);
        }
        Ok(false)
    }

    /// Synchronous only: delivers output until the transform needs input.
    /// Returns the number of samples delivered.
    fn pull_all(
        &mut self,
        session: &Session,
        sink: &mut dyn Sink,
        allow_software_fallback: bool,
    ) -> Result<usize, CodecError> {
        let mut delivered = 0;
        loop {
            match self.process_output(session, sink) {
                Ok(Step::Output) => delivered += 1,
                Ok(Step::StreamChanged | Step::NoOutput) => {}
                Ok(Step::NeedMoreInput) => return Ok(delivered),
                Err(error)
                    if is_unsupported_d3d_type(&error)
                        && allow_software_fallback
                        && !self.software_fallback =>
                {
                    self.detach_d3d_manager(sink)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn process_output(
        &mut self,
        session: &Session,
        sink: &mut dyn Sink,
    ) -> Result<Step, CodecError> {
        let provides =
            MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0;
        let sample = if self.output_info.dwFlags & provides as u32 != 0 {
            None
        } else {
            Some(self.output_sample(session)?)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.output_id,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        // SAFETY: COM call with one output buffer for the output stream. The
        // transform either fills the caller's sample or stores an owned
        // sample; it may also store an owned event collection. Both are
        // released below.
        let result = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };
        let [buffer] = buffers;
        let no_sample = buffer.dwStatus & MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32 != 0;
        let sample = ManuallyDrop::into_inner(buffer.pSample);
        let events = ManuallyDrop::into_inner(buffer.pEvents);
        if let Some(events) = events {
            process_output_events(&events)?;
        }
        match result {
            Ok(()) if no_sample => {
                drop(sample);
                Ok(Step::NoOutput)
            }
            Ok(()) => match sample {
                Some(sample) => {
                    sink.output(&sample)?;
                    Ok(Step::Output)
                }
                None => Ok(Step::NoOutput),
            },
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Step::NeedMoreInput),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                sink.stream_changed(&self.mft, self.output_id)?;
                self.refresh_output_info()?;
                Ok(Step::StreamChanged)
            }
            Err(error) => Err(error).or_platform("IMFTransform::ProcessOutput"),
        }
    }

    /// The reusable sample for transforms that write into caller memory.
    fn output_sample(&mut self, session: &Session) -> Result<IMFSample, CodecError> {
        if self.output_sample.is_none() {
            let size = self.output_info.cbSize.max(1);
            let buffer =
                session.aligned_memory_buffer(size, self.output_info.cbAlignment.max(1))?;
            let sample = session.sample()?;
            // SAFETY: COM call with a valid buffer.
            unsafe { sample.AddBuffer(&buffer) }.or_platform("IMFSample::AddBuffer")?;
            self.output_sample = Some((sample, buffer));
        }
        let (sample, buffer) = self.output_sample.as_ref().expect("allocated above");
        // SAFETY: COM call; the previous output was consumed by the sink.
        unsafe { buffer.SetCurrentLength(0) }.or_platform("IMFMediaBuffer::SetCurrentLength")?;
        Ok(sample.clone())
    }
}

fn is_unsupported_d3d_type(error: &CodecError) -> bool {
    matches!(
        error,
        CodecError::Platform { code, .. } if *code == MF_E_UNSUPPORTED_D3D_TYPE.0
    )
}

/// Checks in-band output events before handling their associated samples.
fn process_output_events(events: &IMFCollection) -> Result<(), CodecError> {
    // SAFETY: COM calls on a valid event collection.
    let count =
        unsafe { events.GetElementCount() }.or_platform("IMFCollection::GetElementCount")?;
    for index in 0..count {
        // SAFETY: the index is below the collection's reported count.
        let event: IMFMediaEvent = unsafe { events.GetElement(index) }
            .and_then(|event| event.cast())
            .or_platform("IMFCollection::GetElement")?;
        // SAFETY: COM call on a valid event.
        let status = unsafe { event.GetStatus() }.or_platform("IMFMediaEvent::GetStatus")?;
        status.ok().or_platform("in-band transform event")?;
    }
    Ok(())
}

impl Drop for Transform {
    fn drop(&mut self) {
        // SAFETY: COM calls on valid interfaces. Failures cannot be acted on
        // during teardown.
        unsafe {
            if self.streaming {
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
            // Releases hardware resources now rather than at the last
            // reference (asynchronous transforms require it).
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// How long an asynchronous transform may stay silent while fastcord waits
/// for an input request, an output, or the end of a drain. A healthy hardware
/// encoder answers within milliseconds; a hung driver must not hang the codec
/// thread with it.
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);

type EventResult = Result<(MF_EVENT_TYPE, HRESULT), CodecError>;

/// The events of an asynchronous transform, received on Media Foundation's
/// work queue and handed to the codec thread with a timeout.
struct EventQueue {
    generator: IMFMediaEventGenerator,
    callback: IMFAsyncCallback,
    receiver: Receiver<EventResult>,
    /// A `BeginGetEvent` is outstanding.
    pending: bool,
}

impl EventQueue {
    fn new(generator: IMFMediaEventGenerator) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            generator,
            callback: EventCallback { sender }.into(),
            receiver,
            pending: false,
        }
    }

    /// The next event's type; a failed event is an error.
    fn next(&mut self) -> Result<MF_EVENT_TYPE, CodecError> {
        if !self.pending {
            let state: IUnknown = self
                .generator
                .cast()
                .or_platform("IMFMediaEventGenerator as IUnknown")?;
            // SAFETY: COM call with a live callback; the generator is passed
            // as the request state so the callback can complete it.
            unsafe { self.generator.BeginGetEvent(&self.callback, &state) }
                .or_platform("IMFMediaEventGenerator::BeginGetEvent")?;
            self.pending = true;
        }
        let (kind, status) = match self.receiver.recv_timeout(EVENT_TIMEOUT) {
            Ok(event) => event?,
            // The request stays outstanding: a late event is still received
            // by the next call.
            Err(RecvTimeoutError::Timeout) => {
                return Err(CodecError::Stalled("the hardware codec stopped responding"));
            }
            Err(RecvTimeoutError::Disconnected) => unreachable!("the callback owns a sender"),
        };
        self.pending = false;
        status.ok().or_platform("asynchronous transform event")?;
        Ok(kind)
    }
}

#[implement(IMFAsyncCallback)]
struct EventCallback {
    sender: Sender<EventResult>,
}

impl IMFAsyncCallback_Impl for EventCallback_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        // Default work queue and behavior.
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        let event = || -> windows::core::Result<(MF_EVENT_TYPE, HRESULT)> {
            let result = result.ok()?;
            // SAFETY: COM calls on valid interfaces: the request state is the
            // generator that `EventQueue::next` passed to BeginGetEvent.
            unsafe {
                let generator: IMFMediaEventGenerator = result.GetState()?.cast()?;
                let event = generator.EndGetEvent(result)?;
                Ok((MF_EVENT_TYPE(event.GetType()? as i32), event.GetStatus()?))
            }
        };
        // The codec may be gone (its queue dropped); then the event is moot.
        let _ = self
            .sender
            .send(event().or_platform("IMFMediaEventGenerator::EndGetEvent"));
        Ok(())
    }
}

/// Reusable system-memory input samples. A sample is reused only after the
/// transform released it; at most [`InputPool::CAPACITY`] are kept
/// (SPEC §8.5), and busier pipelines get transient ones.
#[derive(Default)]
pub(super) struct InputPool {
    samples: Vec<(IMFSample, IMFMediaBuffer, u32)>,
}

impl InputPool {
    const CAPACITY: usize = 3;

    /// A sample whose single buffer holds `bytes` written by `fill`.
    pub(super) fn sample(
        &mut self,
        session: &Session,
        bytes: usize,
        alignment: u32,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<IMFSample, CodecError> {
        let length = u32::try_from(bytes)
            .map_err(|_| CodecError::Unsupported("sample is larger than 4 GiB"))?;
        let reusable = self
            .samples
            .iter()
            .position(|(sample, _, capacity)| *capacity >= length && !shared(sample));
        let (sample, buffer) = match reusable {
            Some(index) => {
                let (sample, buffer, _) = &self.samples[index];
                (sample.clone(), buffer.clone())
            }
            None => {
                let buffer = session.aligned_memory_buffer(length, alignment)?;
                let sample = session.sample()?;
                // SAFETY: COM call with a valid buffer.
                unsafe { sample.AddBuffer(&buffer) }.or_platform("IMFSample::AddBuffer")?;
                if self.samples.len() < Self::CAPACITY {
                    self.samples.push((sample.clone(), buffer.clone(), length));
                } else if let Some(slot) = self
                    .samples
                    .iter_mut()
                    .find(|(sample, _, capacity)| *capacity < length && !shared(sample))
                {
                    // Replace a free but too small sample.
                    *slot = (sample.clone(), buffer.clone(), length);
                }
                (sample, buffer)
            }
        };
        let mut data = std::ptr::null_mut();
        let mut capacity = 0u32;
        // SAFETY: locks the buffer for writing; it is unlocked before return.
        // No one else holds the sample (checked above or newly created), and
        // the locked region is `capacity >= length` bytes.
        unsafe {
            buffer
                .Lock(&mut data, Some(&mut capacity), None)
                .or_platform("IMFMediaBuffer::Lock")?;
            if capacity < length {
                let _ = buffer.Unlock();
                return Err(CodecError::Platform {
                    operation: "IMFMediaBuffer::Lock",
                    code: windows::Win32::Foundation::E_UNEXPECTED.0,
                });
            }
            fill(std::slice::from_raw_parts_mut(data, bytes));
            buffer.Unlock().or_platform("IMFMediaBuffer::Unlock")?;
            buffer
                .SetCurrentLength(length)
                .or_platform("IMFMediaBuffer::SetCurrentLength")?;
            sample
                .DeleteAllItems()
                .or_platform("IMFSample::DeleteAllItems")?;
        }
        Ok(sample)
    }
}

/// Whether anyone besides the caller's reference holds `object`.
///
/// Media Foundation's own sample and buffer objects return their true
/// reference count from `Release`; a transform that still holds an input
/// sample keeps the count above one.
fn shared(object: &IMFSample) -> bool {
    let raw = object.as_raw();
    // SAFETY: `raw` is a live COM object (we hold a reference); the AddRef is
    // balanced by the Release, so the count is unchanged afterwards.
    unsafe {
        let vtable = *(raw as *const *const windows::core::IUnknown_Vtbl);
        ((*vtable).AddRef)(raw);
        ((*vtable).Release)(raw) > 1
    }
}
