//! Direct3D 11 for capture: the device the frame pool renders on, the GPU
//! converter (a video processor that scales the captured BGRA surface to the
//! output size and converts it to NV12 in BT.709 limited range) and the CPU
//! fallback that reads the surface back and converts it with
//! [`convert`](super::super::convert).

use std::mem::ManuallyDrop;
use std::time::Duration;

use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Win32::Foundation::{E_POINTER, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::core::Interface;

use super::super::convert::{self, Bgra};
use super::super::{CaptureError, Conversion, ConversionPath, VideoSample};
use super::runtime::OrCapture;

/// The Direct3D 11 device captured frames are rendered on. All use of its
/// immediate context stays on the capture thread; the device is marked
/// multithread-protected because the frame pool renders on its own threads.
pub(super) struct Device {
    pub(super) device: ID3D11Device,
    pub(super) context: ID3D11DeviceContext,
    /// The same device as the WinRT object the frame pool takes.
    pub(super) winrt: IDirect3DDevice,
    /// A software (WARP) device: no GPU adapter could be used.
    pub(super) software: bool,
}

impl Device {
    pub(super) fn new() -> Result<Self, CaptureError> {
        let (device, context, software) = match Self::create(D3D_DRIVER_TYPE_HARDWARE) {
            Ok((device, context)) => (device, context, false),
            Err(_) => {
                let (device, context) = Self::create(D3D_DRIVER_TYPE_WARP)?;
                (device, context, true)
            }
        };
        let multithread: ID3D11Multithread = device.cast().capture("ID3D11Multithread")?;
        // SAFETY: a plain COM call on a valid interface.
        let _previous = unsafe { multithread.SetMultithreadProtected(true) };
        let dxgi: IDXGIDevice = device.cast().capture("ID3D11Device as IDXGIDevice")?;
        // SAFETY: wraps a live DXGI device in its WinRT projection.
        let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .capture("CreateDirect3D11DeviceFromDXGIDevice")?
            .cast()
            .capture("IInspectable as IDirect3DDevice")?;
        Ok(Self {
            device,
            context,
            winrt,
            software,
        })
    }

    fn create(
        driver: D3D_DRIVER_TYPE,
    ) -> Result<(ID3D11Device, ID3D11DeviceContext), CaptureError> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let (mut device, mut context) = (None, None);
        // SAFETY: creates a device on the default adapter with valid out
        // pointers; BGRA support is required by the capture surfaces and video
        // support by the converter.
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .capture("D3D11CreateDevice")?;
        match (device, context) {
            (Some(device), Some(context)) => Ok((device, context)),
            _ => Err(CaptureError::Native {
                operation: "D3D11CreateDevice",
                code: E_POINTER.0,
            }),
        }
    }
}

/// One converted NV12 picture still on the GPU (see
/// [`capture::GpuFrame`](super::super::GpuFrame)).
pub struct GpuFrame<'a> {
    device: &'a ID3D11Device,
    texture: &'a ID3D11Texture2D,
    width: u32,
    height: u32,
    timestamp: Duration,
}

impl<'a> GpuFrame<'a> {
    /// The capture's device, which owns [`texture`](Self::texture).
    pub fn device(&self) -> &'a ID3D11Device {
        self.device
    }

    /// A single-sample NV12 render-target texture of `width` x `height`. It is
    /// reused after a few more pictures, so a consumer must be done with it
    /// (or hold the interface) by the time the callback's encoder call returns.
    pub fn texture(&self) -> &'a ID3D11Texture2D {
        self.texture
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn timestamp(&self) -> Duration {
        self.timestamp
    }
}

/// The Direct3D texture behind a captured frame's surface.
pub(super) fn surface_texture(surface: &IDirect3DSurface) -> Result<ID3D11Texture2D, CaptureError> {
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .capture("IDirect3DSurface as IDirect3DDxgiInterfaceAccess")?;
    // SAFETY: the access interface belongs to a live surface of this device.
    unsafe { access.GetInterface::<ID3D11Texture2D>() }.capture("GetInterface(ID3D11Texture2D)")
}

pub(super) fn texture_size(texture: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: a plain query of a live texture into a valid structure.
    unsafe { texture.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

fn create_texture(
    device: &Device,
    desc: &D3D11_TEXTURE2D_DESC,
) -> Result<ID3D11Texture2D, CaptureError> {
    let mut texture = None;
    // SAFETY: a valid descriptor and out pointer; staging usage takes no
    // initial data.
    unsafe {
        device
            .device
            .CreateTexture2D(desc, None, Some(&mut texture))
    }
    .capture("ID3D11Device::CreateTexture2D")?;
    texture.ok_or(CaptureError::Native {
        operation: "ID3D11Device::CreateTexture2D",
        code: E_POINTER.0,
    })
}

/// Releases a CPU mapping when dropped, also on unwinding.
struct Mapping<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
    data: *const u8,
    pitch: usize,
}

impl<'a> Mapping<'a> {
    fn read(
        context: &'a ID3D11DeviceContext,
        texture: &'a ID3D11Texture2D,
    ) -> Result<Self, CaptureError> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `texture` is a staging texture with CPU read access; the
        // mapping is released by `Drop`.
        unsafe { context.Map(texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .capture("ID3D11DeviceContext::Map")?;
        let mapping = Self {
            context,
            texture,
            data: mapped.pData.cast(),
            pitch: mapped.RowPitch as usize,
        };
        if mapping.data.is_null() {
            return Err(CaptureError::Native {
                operation: "ID3D11DeviceContext::Map",
                code: E_POINTER.0,
            });
        }
        Ok(mapping)
    }

    /// The first `len` bytes of the mapped subresource.
    fn bytes(&self, len: usize) -> &[u8] {
        // SAFETY: Map returned the start of a subresource of at least
        // `pitch * rows` bytes and every caller asks for exactly its rows; the
        // mapping lives as long as `self`.
        unsafe { std::slice::from_raw_parts(self.data, len) }
    }
}

impl Drop for Mapping<'_> {
    fn drop(&mut self) {
        // SAFETY: balances the successful Map in `read`.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

/// The converter in use. `Auto` starts on the GPU and, if the video processor
/// refuses a frame, switches to the CPU path for the rest of the session.
pub(super) struct Converter {
    inner: Inner,
    fallback: bool,
}

enum Inner {
    Gpu(Box<Gpu>),
    Cpu(Box<Cpu>),
}

impl Converter {
    pub(super) fn new(device: &Device, choice: Conversion) -> Result<Self, CaptureError> {
        // A software (WARP) device renders the video processor on the CPU
        // anyway, slower than the direct conversion.
        if device.software && choice == Conversion::Gpu {
            return Err(CaptureError::Unsupported(
                "no GPU adapter is available for the video processor",
            ));
        }
        let inner = match choice {
            Conversion::Cpu => Inner::Cpu(Box::default()),
            Conversion::Gpu => Inner::Gpu(Box::new(Gpu::new(device)?)),
            Conversion::Auto if device.software => Inner::Cpu(Box::default()),
            Conversion::Auto => match Gpu::new(device) {
                Ok(gpu) => Inner::Gpu(Box::new(gpu)),
                Err(_) => Inner::Cpu(Box::default()),
            },
        };
        Ok(Self {
            inner,
            fallback: choice == Conversion::Auto,
        })
    }

    pub(super) fn path(&self) -> ConversionPath {
        match self.inner {
            Inner::Gpu(_) => ConversionPath::Gpu,
            Inner::Cpu(_) => ConversionPath::Cpu,
        }
    }

    /// Converts the `content` region of `texture` to an NV12 picture of
    /// `output` size and passes it to `consume`.
    pub(super) fn deliver<R>(
        &mut self,
        device: &Device,
        texture: &ID3D11Texture2D,
        content: (u32, u32),
        output: (u32, u32),
        timestamp: Duration,
        consume: impl FnOnce(&VideoSample<'_>) -> R,
    ) -> Result<R, CaptureError> {
        if let Inner::Gpu(gpu) = &mut self.inner {
            match gpu.convert(device, texture, content, output) {
                Ok(()) => return gpu.with_sample(device, output, timestamp, consume),
                Err(error) if !self.fallback => return Err(error),
                Err(_) => self.inner = Inner::Cpu(Box::default()),
            }
        }
        let Inner::Cpu(cpu) = &mut self.inner else {
            unreachable!("the GPU path returned or switched to the CPU path above");
        };
        cpu.deliver(device, texture, content, output, timestamp, consume)
    }
}

impl Converter {
    /// As [`deliver`](Self::deliver), but hands the converted texture itself to
    /// `consume` with no readback. `None`: the CPU converter is in use (or the
    /// video processor just failed in automatic mode), so nothing was
    /// converted.
    pub(super) fn deliver_gpu<R>(
        &mut self,
        device: &Device,
        texture: &ID3D11Texture2D,
        content: (u32, u32),
        output: (u32, u32),
        timestamp: Duration,
        consume: impl FnOnce(&GpuFrame<'_>) -> R,
    ) -> Result<Option<R>, CaptureError> {
        let Inner::Gpu(gpu) = &mut self.inner else {
            return Ok(None);
        };
        match gpu.blit(device, texture, content, output) {
            Ok(target) => Ok(Some(consume(&GpuFrame {
                device: &device.device,
                texture: &target,
                width: output.0,
                height: output.1,
                timestamp,
            }))),
            Err(error) if !self.fallback => Err(error),
            Err(_) => {
                self.inner = Inner::Cpu(Box::default());
                Ok(None)
            }
        }
    }
}

/// The NV12 layout the converter produces: the chroma plane directly after
/// `height` luma rows, the same pitch for both.
fn nv12_bytes(pitch: usize, height: u32) -> usize {
    convert::nv12_len(pitch, height as usize)
}

struct Gpu {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    color: Option<ID3D11VideoContext1>,
    pipeline: Option<Pipeline>,
}

/// Everything that depends on the source texture size and the output size.
struct Pipeline {
    texture: (u32, u32),
    output: (u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// The converted pictures, used in turn.
    targets: Vec<Target>,
    next: usize,
    staging: ID3D11Texture2D,
}

struct Target {
    texture: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
}

/// Output textures in rotation: one being filled, one with the consumer, one
/// to spare. At most 1280x720 NV12 each (1.4 MB).
const RING: usize = 3;

impl Gpu {
    fn new(device: &Device) -> Result<Self, CaptureError> {
        let gpu = Self {
            video_device: device
                .device
                .cast()
                .capture("ID3D11Device as ID3D11VideoDevice")?,
            video_context: device
                .context
                .cast()
                .capture("ID3D11DeviceContext as ID3D11VideoContext")?,
            color: device.context.cast::<ID3D11VideoContext1>().ok(),
            pipeline: None,
        };
        // Build the smallest pipeline now, so a machine whose driver cannot
        // convert BGRA to NV12 is found out at start rather than at the first
        // picture.
        drop(Pipeline::new(&gpu, device, (64, 64), (64, 64))?);
        Ok(gpu)
    }

    /// Converts into the next texture of the output ring and reads it back
    /// into the staging texture.
    fn convert(
        &mut self,
        device: &Device,
        texture: &ID3D11Texture2D,
        content: (u32, u32),
        output: (u32, u32),
    ) -> Result<(), CaptureError> {
        let target = self.blit(device, texture, content, output)?;
        let pipeline = self.pipeline.as_ref().expect("built by blit");
        // SAFETY: both textures are NV12 of the same size on this device.
        unsafe { device.context.CopyResource(&pipeline.staging, &target) };
        Ok(())
    }

    /// Scales and converts `texture` into the next texture of the output ring
    /// and returns it. The ring (three textures) keeps the picture intact while
    /// a consumer still reads the previous ones.
    fn blit(
        &mut self,
        device: &Device,
        texture: &ID3D11Texture2D,
        content: (u32, u32),
        output: (u32, u32),
    ) -> Result<ID3D11Texture2D, CaptureError> {
        let size = texture_size(texture);
        if !self
            .pipeline
            .as_ref()
            .is_some_and(|pipeline| pipeline.texture == size && pipeline.output == output)
        {
            self.pipeline = Some(Pipeline::new(self, device, size, output)?);
        }
        let pipeline = self.pipeline.as_mut().expect("built above");
        let slot = pipeline.next;
        pipeline.next = (slot + 1) % RING;
        let pipeline = &*pipeline;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY: a valid texture, enumerator and descriptor, and a valid out
        // pointer.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                &pipeline.enumerator,
                &input_desc,
                Some(&mut input),
            )
        }
        .capture("ID3D11VideoDevice::CreateVideoProcessorInputView")?;
        let source = RECT {
            left: 0,
            top: 0,
            right: content.0.min(size.0) as i32,
            bottom: content.1.min(size.1) as i32,
        };
        let target = RECT {
            left: 0,
            top: 0,
            right: output.0 as i32,
            bottom: output.1 as i32,
        };
        let processor = &pipeline.processor;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(input),
            ..Default::default()
        };
        // SAFETY: the processor, views and rectangles are valid for these
        // calls; the stream's input view reference is released right after the
        // blit (it was wrapped in ManuallyDrop to satisfy the ABI).
        let blit = unsafe {
            self.video_context
                .VideoProcessorSetStreamSourceRect(processor, 0, true, Some(&source));
            self.video_context
                .VideoProcessorSetStreamDestRect(processor, 0, true, Some(&target));
            self.video_context
                .VideoProcessorSetOutputTargetRect(processor, true, Some(&target));
            let blit = self.video_context.VideoProcessorBlt(
                processor,
                &pipeline.targets[slot].view,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            blit
        };
        blit.capture("ID3D11VideoContext::VideoProcessorBlt")?;
        Ok(pipeline.targets[slot].texture.clone())
    }

    /// Maps the converted picture, hands it over, and unmaps it.
    fn with_sample<R>(
        &self,
        device: &Device,
        output: (u32, u32),
        timestamp: Duration,
        consume: impl FnOnce(&VideoSample<'_>) -> R,
    ) -> Result<R, CaptureError> {
        let pipeline = self.pipeline.as_ref().expect("converted before mapping");
        let mapping = Mapping::read(&device.context, &pipeline.staging)?;
        let bytes = mapping.bytes(nv12_bytes(mapping.pitch, output.1));
        let sample = VideoSample::nv12(output.0, output.1, mapping.pitch, bytes, timestamp)?;
        Ok(consume(&sample))
    }
}

impl Pipeline {
    fn new(
        gpu: &Gpu,
        device: &Device,
        texture: (u32, u32),
        output: (u32, u32),
    ) -> Result<Self, CaptureError> {
        let rate = DXGI_RATIONAL {
            Numerator: 30,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: texture.0,
            InputHeight: texture.1,
            OutputFrameRate: rate,
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: a valid content description.
        let enumerator = unsafe { gpu.video_device.CreateVideoProcessorEnumerator(&content) }
            .capture("ID3D11VideoDevice::CreateVideoProcessorEnumerator")?;
        let supports = |format, flag: u32| {
            // SAFETY: a plain query of a valid enumerator.
            unsafe { enumerator.CheckVideoProcessorFormat(format) }
                .is_ok_and(|support| support & flag != 0)
        };
        if !supports(
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32,
        ) || !supports(
            DXGI_FORMAT_NV12,
            D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32,
        ) {
            return Err(CaptureError::Unsupported(
                "the GPU video processor cannot convert BGRA to NV12",
            ));
        }
        // SAFETY: a valid enumerator; rate conversion index 0 is always valid.
        let processor = unsafe { gpu.video_device.CreateVideoProcessor(&enumerator, 0) }
            .capture("ID3D11VideoDevice::CreateVideoProcessor")?;
        let staging = create_texture(
            device,
            &D3D11_TEXTURE2D_DESC {
                Width: output.0,
                Height: output.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            },
        )?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut targets = Vec::with_capacity(RING);
        for _ in 0..RING {
            let texture = create_texture(
                device,
                &D3D11_TEXTURE2D_DESC {
                    Width: output.0,
                    Height: output.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                },
            )?;
            let mut view = None;
            // SAFETY: a valid render-target texture, enumerator, descriptor
            // and out pointer.
            unsafe {
                gpu.video_device.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &output_desc,
                    Some(&mut view),
                )
            }
            .capture("ID3D11VideoDevice::CreateVideoProcessorOutputView")?;
            let view = view.ok_or(CaptureError::Native {
                operation: "ID3D11VideoDevice::CreateVideoProcessorOutputView",
                code: E_POINTER.0,
            })?;
            targets.push(Target { texture, view });
        }
        // SAFETY: valid processor and stream index 0; these only configure how
        // the next blit behaves. Automatic enhancement stays off: a screen
        // must not be denoised or sharpened.
        unsafe {
            gpu.video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            gpu.video_context
                .VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            if let Some(color) = &gpu.color {
                // RGB in full range (what the desktop composes) to studio-range
                // BT.709 YCbCr (what the decoder and the encoder assume).
                color.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                color.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            }
        }
        Ok(Self {
            texture,
            output,
            enumerator,
            processor,
            targets,
            next: 0,
            staging,
        })
    }
}

/// The CPU fallback: read the surface back whole, then scale and convert it.
#[derive(Default)]
struct Cpu {
    staging: Option<((u32, u32), ID3D11Texture2D)>,
    converter: convert::Converter,
    nv12: Vec<u8>,
}

impl Cpu {
    fn deliver<R>(
        &mut self,
        device: &Device,
        texture: &ID3D11Texture2D,
        content: (u32, u32),
        output: (u32, u32),
        timestamp: Duration,
        consume: impl FnOnce(&VideoSample<'_>) -> R,
    ) -> Result<R, CaptureError> {
        let size = texture_size(texture);
        if self
            .staging
            .as_ref()
            .is_none_or(|(known, _)| *known != size)
        {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: a plain query of a live texture into a valid structure.
            unsafe { texture.GetDesc(&mut desc) };
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            self.staging = Some((size, create_texture(device, &desc)?));
        }
        let staging = &self.staging.as_ref().expect("created above").1;
        // SAFETY: both textures have the same size, format and sample count.
        unsafe { device.context.CopyResource(staging, texture) };
        let mapping = Mapping::read(&device.context, staging)?;
        let (width, height) = (content.0.min(size.0), content.1.min(size.1));
        let source = Bgra::new(
            mapping.bytes(mapping.pitch * size.1 as usize),
            mapping.pitch,
            width,
            height,
        )?;
        let stride = output.0 as usize;
        self.nv12.resize(nv12_bytes(stride, output.1), 0);
        self.converter
            .convert(&source, output.0, output.1, stride, &mut self.nv12)?;
        drop(mapping);
        let sample = VideoSample::nv12(output.0, output.1, stride, &self.nv12, timestamp)?;
        Ok(consume(&sample))
    }
}
