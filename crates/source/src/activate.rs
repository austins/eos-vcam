//! `IMFActivate` front for the media source.
//!
//! Frame Server creates the CLSID given to `MFCreateVirtualCamera` as an activator and calls
//! `ActivateObject` on it to get the source, so the class factory must hand out this type.

use std::ffi::c_void;
use std::sync::Mutex;

use eosvcam_shared::windows_bindings::{
    E_FAIL, IMFActivate, IMFActivate_Impl, IMFAttributes, IMFAttributes_Impl, IMFMediaSourceEx,
    MF_ATTRIBUTE_TYPE, MF_ATTRIBUTES_MATCH_TYPE,
    MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, MFCreateAttributes,
    MFT_TRANSFORM_CLSID_Attribute, PROPVARIANT,
};
use windows_core::{BOOL, Error, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result, implement};

use eosvcam_shared::log;

use crate::CLSID;
use crate::media::create_source;

#[implement(IMFActivate)]
pub struct Activator {
    attrs: IMFAttributes,
    source: Mutex<Option<IMFMediaSourceEx>>,
}

impl Activator {
    pub fn new() -> Result<Self> {
        let mut attrs = None;
        // SAFETY: out pointer is valid for one interface.
        unsafe { MFCreateAttributes(&mut attrs, 0).ok()? };
        let attrs = attrs.ok_or_else(|| Error::from(E_FAIL))?;
        // SAFETY: setting standard virtual-camera activator attributes on a fresh store.
        unsafe {
            attrs
                .SetUINT32(&MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, 1)
                .ok()?;
            attrs.SetGUID(&MFT_TRANSFORM_CLSID_Attribute, &CLSID).ok()?;
        }
        Ok(Self {
            attrs,
            source: Mutex::new(None),
        })
    }
}

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        log!(
            "ActivateObject riid={:?} ppv_null={}",
            if riid.is_null() {
                None
            } else {
                // SAFETY: riid was checked for null just above.
                Some(unsafe { *riid })
            },
            ppv.is_null()
        );
        if riid.is_null() || ppv.is_null() {
            return Err(Error::from(eosvcam_shared::windows_bindings::E_POINTER));
        }
        let mut slot = self.source.lock().map_err(|_| Error::from(E_FAIL))?;
        if slot.is_none() {
            *slot = Some(create_source(Some(&self.attrs))?);
        }
        // SAFETY: riid and ppv were checked for null and come from the caller.
        let hr = unsafe {
            slot.as_ref()
                .ok_or_else(|| Error::from(E_FAIL))?
                .query(riid, ppv)
        };
        log!("ActivateObject -> {hr:?}");
        hr.ok()
    }

    fn ShutdownObject(&self) -> Result<()> {
        log!("ShutdownObject");
        if let Ok(mut slot) = self.source.lock()
            && let Some(s) = slot.take()
        {
            // SAFETY: Shutdown takes no arguments.
            let _ = unsafe { s.Shutdown() };
        }
        Ok(())
    }

    fn DetachObject(&self) -> Result<()> {
        log!("DetachObject");
        if let Ok(mut slot) = self.source.lock() {
            *slot = None;
        }
        Ok(())
    }
}

/// Forwards every `IMFAttributes` method to the `$field` store on `self` through its vtable,
/// preserving raw-pointer contracts (including null interface pointers) as supplied by the caller.
#[macro_export]
macro_rules! forward_mf_attributes {
    ($ty:ident, $field:ident) => {
        impl IMFAttributes_Impl for $ty {
            fn GetItem(&self, k: *const GUID, v: *mut PROPVARIANT) -> Result<()> {
                // SAFETY: arguments are passed through unchanged to a live IMFAttributes.
                unsafe {
                    (Interface::vtable(&self.$field).GetItem)(self.$field.as_raw(), k, v).ok()
                }
            }
            fn GetItemType(&self, k: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> {
                let mut out = MF_ATTRIBUTE_TYPE::default();
                // SAFETY: out pointer is valid; k is passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).GetItemType)(self.$field.as_raw(), k, &mut out)
                        .ok()?
                };
                Ok(out)
            }
            fn CompareItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<BOOL> {
                let mut out = BOOL::default();
                // SAFETY: out pointer is valid; k and v are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).CompareItem)(
                        self.$field.as_raw(),
                        k,
                        v,
                        &mut out,
                    )
                    .ok()?
                };
                Ok(out)
            }
            fn Compare(
                &self,
                theirs: Ref<IMFAttributes>,
                kind: MF_ATTRIBUTES_MATCH_TYPE,
            ) -> Result<BOOL> {
                // SAFETY: `theirs.as_ref()` passes Option<&IMFAttributes> without rejecting null via Ref::ok().
                unsafe { self.$field.Compare(theirs.as_ref(), kind) }
            }
            fn GetUINT32(&self, k: *const GUID) -> Result<u32> {
                let mut out = 0;
                // SAFETY: out pointer is valid.
                unsafe {
                    (Interface::vtable(&self.$field).GetUINT32)(self.$field.as_raw(), k, &mut out)
                        .ok()?
                };
                Ok(out)
            }
            fn GetUINT64(&self, k: *const GUID) -> Result<u64> {
                let mut out = 0;
                // SAFETY: out pointer is valid.
                unsafe {
                    (Interface::vtable(&self.$field).GetUINT64)(self.$field.as_raw(), k, &mut out)
                        .ok()?
                };
                Ok(out)
            }
            fn GetDouble(&self, k: *const GUID) -> Result<f64> {
                let mut out = 0.0;
                // SAFETY: out pointer is valid.
                unsafe {
                    (Interface::vtable(&self.$field).GetDouble)(self.$field.as_raw(), k, &mut out)
                        .ok()?
                };
                Ok(out)
            }
            fn GetGUID(&self, k: *const GUID) -> Result<GUID> {
                let mut out = GUID::default();
                // SAFETY: out pointer is valid.
                unsafe {
                    (Interface::vtable(&self.$field).GetGUID)(self.$field.as_raw(), k, &mut out)
                        .ok()?
                };
                Ok(out)
            }
            fn GetStringLength(&self, k: *const GUID) -> Result<u32> {
                let mut out = 0;
                // SAFETY: out pointer is valid.
                unsafe {
                    (Interface::vtable(&self.$field).GetStringLength)(
                        self.$field.as_raw(),
                        k,
                        &mut out,
                    )
                    .ok()?
                };
                Ok(out)
            }
            fn GetString(&self, k: *const GUID, buf: PWSTR, cch: u32, len: *mut u32) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).GetString)(
                        self.$field.as_raw(),
                        k,
                        buf,
                        cch,
                        len,
                    )
                    .ok()
                }
            }
            fn GetAllocatedString(
                &self,
                k: *const GUID,
                buf: *mut PWSTR,
                len: *mut u32,
            ) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).GetAllocatedString)(
                        self.$field.as_raw(),
                        k,
                        buf,
                        len,
                    )
                    .ok()
                }
            }
            fn GetBlobSize(&self, k: *const GUID) -> Result<u32> {
                let mut out = 0;
                // SAFETY: out pointer is valid.
                unsafe {
                    (Interface::vtable(&self.$field).GetBlobSize)(self.$field.as_raw(), k, &mut out)
                        .ok()?
                };
                Ok(out)
            }
            fn GetBlob(&self, k: *const GUID, buf: *mut u8, cb: u32, len: *mut u32) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).GetBlob)(self.$field.as_raw(), k, buf, cb, len)
                        .ok()
                }
            }
            fn GetAllocatedBlob(
                &self,
                k: *const GUID,
                buf: *mut *mut u8,
                len: *mut u32,
            ) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).GetAllocatedBlob)(
                        self.$field.as_raw(),
                        k,
                        buf,
                        len,
                    )
                    .ok()
                }
            }
            fn GetUnknown(
                &self,
                k: *const GUID,
                riid: *const GUID,
                ppv: *mut *mut c_void,
            ) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).GetUnknown)(self.$field.as_raw(), k, riid, ppv)
                        .ok()
                }
            }
            fn SetItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).SetItem)(self.$field.as_raw(), k, v).ok()
                }
            }
            fn DeleteItem(&self, k: *const GUID) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).DeleteItem)(self.$field.as_raw(), k).ok()
                }
            }
            fn DeleteAllItems(&self) -> Result<()> {
                // SAFETY: takes no pointer arguments.
                unsafe {
                    (Interface::vtable(&self.$field).DeleteAllItems)(self.$field.as_raw()).ok()
                }
            }
            fn SetUINT32(&self, k: *const GUID, v: u32) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).SetUINT32)(self.$field.as_raw(), k, v).ok()
                }
            }
            fn SetUINT64(&self, k: *const GUID, v: u64) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).SetUINT64)(self.$field.as_raw(), k, v).ok()
                }
            }
            fn SetDouble(&self, k: *const GUID, v: f64) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).SetDouble)(self.$field.as_raw(), k, v).ok()
                }
            }
            fn SetGUID(&self, k: *const GUID, v: *const GUID) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).SetGUID)(self.$field.as_raw(), k, v).ok()
                }
            }
            fn SetString(&self, k: *const GUID, v: &PCWSTR) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).SetString)(self.$field.as_raw(), k, *v).ok()
                }
            }
            fn SetBlob(&self, k: *const GUID, buf: *const u8, cb: u32) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).SetBlob)(self.$field.as_raw(), k, buf, cb).ok()
                }
            }
            fn SetUnknown(&self, k: *const GUID, v: Ref<IUnknown>) -> Result<()> {
                // SAFETY: `v.as_ref()` passes Option<&IUnknown> so null is forwarded without Ref::ok() E_POINTER.
                let r = unsafe { self.$field.SetUnknown(k, v.as_ref()) };
                log!(
                    "SetUnknown key={:?} ptr_null={} -> {r:?}",
                    if k.is_null() {
                        None
                    } else {
                        // SAFETY: k was checked for null just above.
                        Some(unsafe { *k })
                    },
                    v.is_null()
                );
                r.ok()
            }
            fn LockStore(&self) -> Result<()> {
                // SAFETY: takes no pointer arguments.
                unsafe { (Interface::vtable(&self.$field).LockStore)(self.$field.as_raw()).ok() }
            }
            fn UnlockStore(&self) -> Result<()> {
                // SAFETY: takes no pointer arguments.
                unsafe { (Interface::vtable(&self.$field).UnlockStore)(self.$field.as_raw()).ok() }
            }
            fn GetCount(&self) -> Result<u32> {
                let mut out = 0;
                // SAFETY: out pointer is valid.
                unsafe {
                    (Interface::vtable(&self.$field).GetCount)(self.$field.as_raw(), &mut out)
                        .ok()?
                };
                Ok(out)
            }
            fn GetItemByIndex(&self, i: u32, k: *mut GUID, v: *mut PROPVARIANT) -> Result<()> {
                // SAFETY: arguments are passed through unchanged.
                unsafe {
                    (Interface::vtable(&self.$field).GetItemByIndex)(self.$field.as_raw(), i, k, v)
                        .ok()
                }
            }
            fn CopyAllItems(&self, dest: Ref<IMFAttributes>) -> Result<()> {
                // SAFETY: `dest.as_ref()` passes Option<&IMFAttributes> without rejecting null via Ref::ok().
                unsafe { self.$field.CopyAllItems(dest.as_ref()).ok() }
            }
        }
    };
}

forward_mf_attributes!(Activator_Impl, attrs);
