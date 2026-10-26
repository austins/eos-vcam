//! Media Foundation custom media source for the EosVCam virtual camera.
//!
//! Frame Server loads this DLL in-process. It exposes one live NV12 stream and pulls frames
//! from the shared-memory ring written by the feeder service.

mod activate;
mod media;

use std::ffi::c_void;
use std::sync::Once;

use eosvcam_shared::log;
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, S_FALSE,
};
use windows::Win32::Media::MediaFoundation::IMFMediaSourceEx;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement};

pub use media::create_source;

pub(crate) const CLSID: GUID = GUID::from_u128(0x6F0B9D3A_4C1E_4E53_9A57_3D1C2B7E8A41);

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        if ppv.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        // SAFETY: riid was checked for null.
        log!("CreateInstance iid={:?}", unsafe { *riid });
        // Frame Server asks for an IMFActivate; plain source IIDs are still served directly.
        let act: IUnknown = activate::Activator::new()?.into();
        // SAFETY: riid and ppv were checked for null and come from the caller of CreateInstance.
        let hr = unsafe { act.query(riid, ppv) };
        log!("activator query -> {hr:?}");
        if hr.is_ok() {
            return Ok(());
        }
        let src: IMFMediaSourceEx = create_source(None)?;
        // SAFETY: as above.
        let hr = unsafe { src.query(riid, ppv) };
        if hr.is_err() {
            // Break the source<->stream cycle so the worker thread does not leak.
            // SAFETY: src is a live source owned by this function.
            let _ = unsafe { src.Shutdown() };
        }
        hr.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

/// # Safety
/// Called by COM with valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    static INIT: Once = Once::new();
    INIT.call_once(|| log::init("source", false));
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked for null above.
    if unsafe { *rclsid } != CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    log!("DllGetClassObject");
    let factory: IClassFactory = Factory.into();
    // SAFETY: riid and ppv were checked for null.
    unsafe { factory.query(riid, ppv) }
}

/// Frame Server may keep the DLL loaded; never offer to unload it.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
