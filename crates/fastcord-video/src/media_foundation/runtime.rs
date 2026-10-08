//! Media Foundation platform entry points, loaded at runtime.
//!
//! `mfplat.dll` is absent on Windows N/KN editions without the Media Feature
//! Pack and on Windows Server without the Media Foundation feature. Linking it
//! statically would stop fastcord from starting there at all, so its exports
//! are resolved with `GetProcAddress` and a missing library becomes
//! [`CodecError::PlatformUnavailable`] for the video features alone.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr;
use std::sync::LazyLock;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample, MF_VERSION,
    MFMediaType_Video, MFSTARTUP_LITE, MFT_ENUM_FLAG, MFT_REGISTER_TYPE_INFO,
};
use windows::Win32::System::Com::{
    COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{GUID, HRESULT, Interface, PCSTR, s, w};

use crate::codec::CodecError;

const UNAVAILABLE: CodecError = CodecError::PlatformUnavailable("Media Foundation");

type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type ShutdownFn = unsafe extern "system" fn() -> HRESULT;
type EnumFn = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut Option<IMFActivate>,
    *mut u32,
) -> HRESULT;
type CreateFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type CreateManagerFn = unsafe extern "system" fn(*mut u32, *mut *mut c_void) -> HRESULT;
type CreateAlignedBufferFn = unsafe extern "system" fn(u32, u32, *mut *mut c_void) -> HRESULT;

/// The `mfplat.dll` exports fastcord uses.
struct Mfplat {
    startup: StartupFn,
    shutdown: ShutdownFn,
    enum_ex: EnumFn,
    create_media_type: CreateFn,
    create_sample: CreateFn,
    create_aligned_memory_buffer: CreateAlignedBufferFn,
    create_dxgi_device_manager: CreateManagerFn,
}

static MFPLAT: LazyLock<Option<Mfplat>> = LazyLock::new(load);

fn mfplat() -> Result<&'static Mfplat, CodecError> {
    MFPLAT.as_ref().ok_or(UNAVAILABLE)
}

fn load() -> Option<Mfplat> {
    // SAFETY: loads a system library by name from System32 only; the module
    // stays loaded for the life of the process, so the resolved function
    // pointers never dangle.
    let module =
        unsafe { LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()?;
    /// Resolves `name` as a function of type `F`.
    ///
    /// # Safety
    /// `F` must be the export's exact `extern "system"` signature.
    unsafe fn export<F: Copy>(
        module: windows::Win32::Foundation::HMODULE,
        name: PCSTR,
    ) -> Option<F> {
        // SAFETY: `module` is a loaded library and `name` a NUL-terminated
        // literal.
        let address = unsafe { GetProcAddress(module, name) }?;
        assert_eq!(size_of::<F>(), size_of_val(&address));
        // SAFETY: the caller guarantees `F` is the export's signature; both
        // are plain function pointers of the same size.
        Some(unsafe { std::mem::transmute_copy(&address) })
    }
    // SAFETY: each type alias above is the documented signature of the
    // export (mfapi.h).
    unsafe {
        Some(Mfplat {
            startup: export::<StartupFn>(module, s!("MFStartup"))?,
            shutdown: export::<ShutdownFn>(module, s!("MFShutdown"))?,
            enum_ex: export::<EnumFn>(module, s!("MFTEnumEx"))?,
            create_media_type: export::<CreateFn>(module, s!("MFCreateMediaType"))?,
            create_sample: export::<CreateFn>(module, s!("MFCreateSample"))?,
            create_aligned_memory_buffer: export::<CreateAlignedBufferFn>(
                module,
                s!("MFCreateAlignedMemoryBuffer"),
            )?,
            create_dxgi_device_manager: export::<CreateManagerFn>(
                module,
                s!("MFCreateDXGIDeviceManager"),
            )?,
        })
    }
}

/// Maps a failed native call to [`CodecError::Platform`].
pub(super) trait OrPlatform<T> {
    fn or_platform(self, operation: &'static str) -> Result<T, CodecError>;
}

impl<T> OrPlatform<T> for windows::core::Result<T> {
    fn or_platform(self, operation: &'static str) -> Result<T, CodecError> {
        self.map_err(|error| CodecError::Platform {
            operation,
            code: error.code().0,
        })
    }
}

/// Converts an out-pointer filled by a creation function into its interface.
///
/// # Safety
/// On success `raw` must be an owned (AddRef'd) pointer to an object that
/// implements `T`.
unsafe fn created<T: Interface>(
    result: HRESULT,
    raw: *mut c_void,
    operation: &'static str,
) -> Result<T, CodecError> {
    result.ok().or_platform(operation)?;
    if raw.is_null() {
        return Err(CodecError::Platform {
            operation,
            code: windows::Win32::Foundation::E_POINTER.0,
        });
    }
    // SAFETY: guaranteed by the caller.
    Ok(unsafe { T::from_raw(raw) })
}

/// COM and Media Foundation initialized on the current thread for the life of
/// one codec. Media Foundation start-up is reference counted, so codecs on the
/// same thread nest freely. Not `Send`: it must be dropped on its thread.
pub(super) struct Session {
    api: &'static Mfplat,
    uninitialize_com: bool,
    _thread_bound: PhantomData<*const ()>,
}

impl Session {
    pub(super) fn start() -> Result<Self, CodecError> {
        let api = mfplat()?;
        // SAFETY: initializes COM for this thread; balanced in `Drop` when it
        // succeeds. Media Foundation's asynchronous transforms require an MTA;
        // an existing incompatible apartment must not be accepted.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE) };
        if result.is_err() {
            return Err(CodecError::Platform {
                operation: "CoInitializeEx",
                code: result.0,
            });
        }
        let session = Self {
            api,
            uninitialize_com: result.is_ok(),
            _thread_bound: PhantomData,
        };
        // SAFETY: plain start-up call with the SDK version it was built for.
        let result = unsafe { (api.startup)(MF_VERSION, MFSTARTUP_LITE) };
        if result.is_err() {
            // Only COM must be undone; skip MFShutdown.
            let mut session = std::mem::ManuallyDrop::new(session);
            session.release_com();
            return Err(CodecError::Platform {
                operation: "MFStartup",
                code: result.0,
            });
        }
        Ok(session)
    }

    fn release_com(&mut self) {
        if std::mem::take(&mut self.uninitialize_com) {
            // SAFETY: balances the successful CoInitializeEx in `start`, on
            // the same thread (`Session` is not `Send`).
            unsafe { CoUninitialize() };
        }
    }

    /// Transforms of `category` that convert video `input` to `output`, in
    /// Media Foundation's merit order.
    pub(super) fn transforms(
        &self,
        category: GUID,
        flags: MFT_ENUM_FLAG,
        input: GUID,
        output: GUID,
    ) -> Result<Vec<IMFActivate>, CodecError> {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: input,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: output,
        };
        let mut array: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: all pointers are valid for the call; on success the array
        // and its `count` interface references belong to the caller.
        unsafe {
            (self.api.enum_ex)(
                category,
                flags.0 as u32,
                &input,
                &output,
                &mut array,
                &mut count,
            )
        }
        .ok()
        .or_platform("MFTEnumEx")?;
        let mut found = Vec::with_capacity(count as usize);
        if !array.is_null() {
            for index in 0..count as usize {
                // SAFETY: `index < count`; each element is read (moved out)
                // exactly once before the array is freed.
                if let Some(activate) = unsafe { array.add(index).read() } {
                    found.push(activate);
                }
            }
            // SAFETY: the array was allocated by MFTEnumEx with
            // CoTaskMemAlloc and its elements were moved out above.
            unsafe { CoTaskMemFree(Some(array as *const c_void)) };
        }
        Ok(found)
    }

    pub(super) fn media_type(&self) -> Result<IMFMediaType, CodecError> {
        let mut raw = ptr::null_mut();
        // SAFETY: `raw` receives an owned IMFMediaType on success.
        unsafe {
            let result = (self.api.create_media_type)(&mut raw);
            created(result, raw, "MFCreateMediaType")
        }
    }

    pub(super) fn sample(&self) -> Result<IMFSample, CodecError> {
        let mut raw = ptr::null_mut();
        // SAFETY: `raw` receives an owned IMFSample on success.
        unsafe {
            let result = (self.api.create_sample)(&mut raw);
            created(result, raw, "MFCreateSample")
        }
    }

    pub(super) fn aligned_memory_buffer(
        &self,
        capacity: u32,
        alignment: u32,
    ) -> Result<IMFMediaBuffer, CodecError> {
        let mut raw = ptr::null_mut();
        // SAFETY: `raw` receives an owned IMFMediaBuffer on success.
        unsafe {
            // MFCreateAlignedMemoryBuffer takes an alignment mask (N - 1),
            // whereas MFT stream info reports alignment in bytes.
            let result = (self.api.create_aligned_memory_buffer)(
                capacity,
                memory_alignment_mask(alignment),
                &mut raw,
            );
            created(result, raw, "MFCreateAlignedMemoryBuffer")
        }
    }

    /// A DXGI device manager and its reset token.
    pub(super) fn dxgi_device_manager(&self) -> Result<(IMFDXGIDeviceManager, u32), CodecError> {
        let mut raw = ptr::null_mut();
        let mut token = 0u32;
        // SAFETY: `token` and `raw` are valid out-pointers; `raw` receives an
        // owned IMFDXGIDeviceManager on success.
        let manager = unsafe {
            let result = (self.api.create_dxgi_device_manager)(&mut token, &mut raw);
            created(result, raw, "MFCreateDXGIDeviceManager")?
        };
        Ok((manager, token))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: balances the successful MFStartup in `start`. The result is
        // ignored: nothing can be done about a failed shutdown.
        let _ = unsafe { (self.api.shutdown)() };
        self.release_com();
    }
}

fn memory_alignment_mask(alignment_bytes: u32) -> u32 {
    alignment_bytes.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::memory_alignment_mask;

    #[test]
    fn aligned_buffer_argument_is_the_required_alignment_mask() {
        for (bytes, expected_mask) in [(0, 0), (1, 0), (16, 15), (32, 31), (64, 63)] {
            assert_eq!(memory_alignment_mask(bytes), expected_mask);
        }
    }
}
