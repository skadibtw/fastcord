//! Direct3D 11 devices for hardware (D3D11VA) decoding.

use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_NV12};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIFactory1,
};
use windows::Win32::Media::MediaFoundation::{IMFDXGIBuffer, IMFDXGIDeviceManager};
use windows::core::Interface;

use super::runtime::{OrPlatform, Session};
use crate::codec::CodecError;

/// A hardware (non-WARP) display adapter.
pub(super) struct Adapter {
    pub adapter: IDXGIAdapter1,
    pub luid: LUID,
    pub vendor_id: u32,
    pub name: String,
}

/// Hardware adapters in DXGI order (the default adapter first).
pub(super) fn hardware_adapters() -> Result<Vec<Adapter>, CodecError> {
    // SAFETY: COM calls on valid interfaces; enumeration stops at the first
    // index DXGI reports as not found.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().or_platform("CreateDXGIFactory1")?;
        let mut adapters = Vec::new();
        let mut index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            index += 1;
            let desc = adapter.GetDesc1().or_platform("IDXGIAdapter1::GetDesc1")?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let length = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            adapters.push(Adapter {
                adapter,
                luid: desc.AdapterLuid,
                vendor_id: desc.VendorId,
                name: String::from_utf16_lossy(&desc.Description[..length])
                    .trim()
                    .to_owned(),
            });
        }
        Ok(adapters)
    }
}

/// A video-capable Direct3D 11 device behind a Media Foundation device
/// manager, as transforms require for D3D11VA.
pub(super) struct DeviceManager {
    pub manager: IMFDXGIDeviceManager,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<(u32, u32, DXGI_FORMAT, u32, u32, ID3D11Texture2D)>,
}

impl DeviceManager {
    pub(super) fn new(session: &Session, adapter: &IDXGIAdapter1) -> Result<Self, CodecError> {
        let mut device = None;
        // SAFETY: creates a device on a valid adapter with valid out-pointers.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .or_platform("D3D11CreateDevice")?;
        let device = device.ok_or(CodecError::Platform {
            operation: "D3D11CreateDevice",
            code: windows::Win32::Foundation::E_POINTER.0,
        })?;
        // The decoder uses the device from its own threads.
        let multithread: ID3D11Multithread = device.cast().or_platform("ID3D11Multithread")?;
        // SAFETY: COM call on a valid interface.
        let _previous = unsafe { multithread.SetMultithreadProtected(true) };
        // SAFETY: returns an owned immediate-context reference from the live
        // D3D11 device.
        let context = unsafe { device.GetImmediateContext() }
            .or_platform("ID3D11Device::GetImmediateContext")?;
        let (manager, token) = session.dxgi_device_manager()?;
        // SAFETY: binds the device to the manager with its own reset token.
        unsafe { manager.ResetDevice(&device, token) }
            .or_platform("IMFDXGIDeviceManager::ResetDevice")?;
        Ok(Self {
            manager,
            device,
            context,
            staging: None,
        })
    }

    /// Copies one D3D11VA NV12 output surface to a reusable CPU-readable
    /// staging texture. The mapped frame borrows this manager and is unmapped
    /// when its lease is dropped.
    pub(super) fn map_nv12(
        &mut self,
        surface: &IMFDXGIBuffer,
    ) -> Result<MappedNv12<'_>, CodecError> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: GetResource returns an owned resource pointer on success.
        unsafe {
            surface
                .GetResource(&ID3D11Texture2D::IID, &mut raw)
                .or_platform("IMFDXGIBuffer::GetResource")?;
        }
        // SAFETY: GetResource returned an owned ID3D11Texture2D reference.
        let source = unsafe { ID3D11Texture2D::from_raw(raw) };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: source is a live texture.
        unsafe { source.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 || desc.SampleDesc.Count != 1 {
            return Err(CodecError::Unsupported(
                "hardware decoder surface is not single-sample NV12",
            ));
        }
        let subresource = unsafe {
            surface
                .GetSubresourceIndex()
                .or_platform("IMFDXGIBuffer::GetSubresourceIndex")?
        };
        self.ensure_staging(&desc)?;
        let staging = &self.staging.as_ref().expect("created above").5;
        // SAFETY: source and staging are same-size, same-format, single-sample
        // textures on this device; `subresource` comes from the DXGI buffer.
        unsafe {
            self.context
                .CopySubresourceRegion(staging, 0, 0, 0, 0, &source, subresource, None)
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE {
            pData: std::ptr::null_mut(),
            RowPitch: 0,
            DepthPitch: 0,
        };
        // SAFETY: staging is a CPU-readable staging texture; `mapped` is a
        // valid out-pointer and the texture stays alive in `self.staging`.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped as *mut _))
        }
        .or_platform("ID3D11DeviceContext::Map")?;
        if mapped.pData.is_null() {
            // SAFETY: the successful Map above must be paired with Unmap.
            unsafe { self.context.Unmap(staging, 0) };
            return Err(CodecError::Platform {
                operation: "ID3D11DeviceContext::Map",
                code: windows::Win32::Foundation::E_POINTER.0,
            });
        }
        Ok(MappedNv12 {
            context: &self.context,
            texture: staging,
            data: mapped.pData.cast(),
            row_pitch: mapped.RowPitch as usize,
            width: desc.Width,
            height: desc.Height,
        })
    }

    fn ensure_staging(&mut self, source: &D3D11_TEXTURE2D_DESC) -> Result<(), CodecError> {
        let matches =
            self.staging
                .as_ref()
                .is_some_and(|(width, height, format, count, quality, _)| {
                    *width == source.Width
                        && *height == source.Height
                        && *format == source.Format
                        && *count == source.SampleDesc.Count
                        && *quality == source.SampleDesc.Quality
                });
        if matches {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: source.Width,
            Height: source.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: source.Format,
            SampleDesc: source.SampleDesc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        // SAFETY: the descriptor is a valid staging NV12 texture and the
        // optional initial-data pointer is null as required for staging usage.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .or_platform("ID3D11Device::CreateTexture2D")?;
        let staging = staging.ok_or(CodecError::Platform {
            operation: "ID3D11Device::CreateTexture2D",
            code: windows::Win32::Foundation::E_POINTER.0,
        })?;
        self.staging = Some((
            source.Width,
            source.Height,
            source.Format,
            source.SampleDesc.Count,
            source.SampleDesc.Quality,
            staging,
        ));
        Ok(())
    }
}

/// A mapped D3D11 texture; dropping it releases the CPU mapping.
pub(super) struct MappedNv12<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
    data: *const u8,
    row_pitch: usize,
    width: u32,
    height: u32,
}

impl MappedNv12<'_> {
    /// Borrows a cropped display aperture from the mapped coded NV12 surface.
    pub(super) fn image(
        &self,
        coded_size: (u32, u32),
        area: (u32, u32, u32, u32),
    ) -> Result<crate::frame::Nv12<'_>, CodecError> {
        let (coded_width, coded_height) = coded_size;
        let (x, y, width, height) = area;
        crate::codec::check_dimensions(width, height)?;
        if coded_width > self.width
            || coded_height > self.height
            || !x.is_multiple_of(2)
            || !y.is_multiple_of(2)
            || !x
                .checked_add(width)
                .is_some_and(|right| right <= coded_width)
            || !y
                .checked_add(height)
                .is_some_and(|bottom| bottom <= coded_height)
            || self.row_pitch < coded_width as usize
        {
            return Err(CodecError::Unsupported(
                "decoder display area is outside the hardware picture",
            ));
        }
        let y_len = self
            .row_pitch
            .checked_mul(self.height as usize)
            .ok_or(CodecError::Unsupported("hardware picture is too large"))?;
        let uv_len = self
            .row_pitch
            .checked_mul(self.height.div_ceil(2) as usize)
            .ok_or(CodecError::Unsupported("hardware picture is too large"))?;
        let mapped_len = y_len
            .checked_add(uv_len)
            .ok_or(CodecError::Unsupported("hardware picture is too large"))?;
        // NV12's mapped Texture2D is laid out as `height` luma rows followed
        // by `ceil(height / 2)` interleaved chroma rows at the same RowPitch.
        // D3D11_MAPPED_SUBRESOURCE::DepthPitch is not the byte length for a
        // Texture2D, so the format and returned row pitch define this span.
        // SAFETY: Map returned the start of this NV12 Texture2D's subresource;
        // the computed span covers its documented Y and UV planes, and the
        // mapping remains held for this borrow's lifetime.
        let data = unsafe { std::slice::from_raw_parts(self.data, mapped_len) };
        let y_offset = y as usize * self.row_pitch + x as usize;
        let uv_offset = y_len + (y / 2) as usize * self.row_pitch + x as usize;
        Ok(crate::frame::Nv12::new(
            width,
            height,
            &data[y_offset..],
            self.row_pitch,
            &data[uv_offset..],
            self.row_pitch,
        )?)
    }
}

impl Drop for MappedNv12<'_> {
    fn drop(&mut self) {
        // SAFETY: balances the successful Map in `DeviceManager::map_nv12`.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}
