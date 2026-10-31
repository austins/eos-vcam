//! Minimal dynamic binding to Canon's EDSDK, covering the live-view calls the feeder needs.

use std::ffi::c_void;
use std::fs;
use std::mem::transmute;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use eosvcam_shared::log;
use eosvcam_shared::windows_bindings::{GetProcAddress, LoadLibraryW, SetDllDirectoryW};
use windows_core::{HSTRING, PCSTR};

type Ref = *mut c_void;
type Err = u32;

pub const ERR_NOT_READY: u32 = 0xA102;
const PROP_EVF_OUTPUT_DEVICE: u32 = 0x500;
const PROP_EVF_MODE: u32 = 0x501;
const OUTPUT_PC: u32 = 2;

/// EdsDeviceInfo from EDSDKTypes.h.
#[repr(C)]
pub struct DeviceInfo {
    port: [u8; 256],
    description: [u8; 256],
    sub_type: u32,
    reserved: u32,
}

/// Text up to the first NUL of a fixed-size C string field.
fn c_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_owned()
}

macro_rules! sdk {
    ($($field:ident = $sym:literal : fn($($arg:ty),*) -> $ret:ty;)*) => {
        pub struct Sdk { $($field: unsafe extern "system" fn($($arg),*) -> $ret,)* }

        impl Sdk {
            fn bind(m: eosvcam_shared::windows_bindings::HMODULE) -> Result<Self> {
                Ok(Self { $($field: {
                    // SAFETY: the symbol is NUL-terminated and the signature matches EDSDK's C header.
                    let p = unsafe { GetProcAddress(m, PCSTR(concat!($sym, "\0").as_ptr())) }
                        .with_context(|| format!("EDSDK.dll has no {}", $sym))?;
                    // SAFETY: the symbol exists in this SDK version and the type matches its C signature.
                    unsafe { transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn($($arg),*) -> $ret>(p) }
                },)* })
            }
        }
    };
}

sdk! {
    init = "EdsInitializeSDK": fn() -> Err;
    term = "EdsTerminateSDK": fn() -> Err;
    camera_list = "EdsGetCameraList": fn(*mut Ref) -> Err;
    child_count = "EdsGetChildCount": fn(Ref, *mut u32) -> Err;
    child = "EdsGetChildAtIndex": fn(Ref, i32, *mut Ref) -> Err;
    device_info = "EdsGetDeviceInfo": fn(Ref, *mut DeviceInfo) -> Err;
    open_session = "EdsOpenSession": fn(Ref) -> Err;
    close_session = "EdsCloseSession": fn(Ref) -> Err;
    set_prop = "EdsSetPropertyData": fn(Ref, u32, i32, u32, *const c_void) -> Err;
    memory_stream = "EdsCreateMemoryStream": fn(u64, *mut Ref) -> Err;
    evf_image = "EdsCreateEvfImageRef": fn(Ref, *mut Ref) -> Err;
    download_evf = "EdsDownloadEvfImage": fn(Ref, Ref) -> Err;
    pointer = "EdsGetPointer": fn(Ref, *mut *mut u8) -> Err;
    length = "EdsGetLength": fn(Ref, *mut u64) -> Err;
    release = "EdsRelease": fn(Ref) -> u32;
}

/// Looks for EDSDK.dll next to the executable, then anywhere under the Canon folders in Program Files.
/// Only administrator-writable locations are searched because the feeder service runs as LocalSystem.
fn find_dll() -> Option<PathBuf> {
    let mut roots = vec![];
    if let Ok(exe) = std::env::current_exe() {
        roots.extend(exe.parent().map(Path::to_path_buf));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        roots.extend(std::env::var_os(var).map(|p| Path::new(&p).join("Canon")));
    }
    for root in roots {
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            if dir.join("EDSDK.dll").is_file() {
                return Some(dir);
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            stack.extend(entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        }
    }
    None
}

impl Sdk {
    pub fn load() -> Result<Self> {
        let Some(dir) = find_dll() else {
            bail!(
                "EDSDK.dll not found; run `eosvcam install` with Canon's EOS Webcam Utility Pro .zip next to eosvcam.exe, or use `install --sdk PATH`"
            );
        };
        log!("using EDSDK from {}", dir.display());
        // SAFETY: loading a vendor DLL by absolute path; its dependencies sit in the same folder.
        unsafe {
            SetDllDirectoryW(&HSTRING::from(dir.as_os_str()))
                .ok()
                .context("SetDllDirectory")?;
            let m = LoadLibraryW(&HSTRING::from(dir.join("EDSDK.dll").as_os_str()))
                .ok()
                .context("loading EDSDK.dll")?;
            Self::bind(m)
        }
    }

    /// Initialises the SDK, opens the first camera and switches live view output to the PC.
    pub fn open(&self) -> Result<Session<'_>, OpenError> {
        // SAFETY: plain FFI calls following the EDSDK contract; every acquired ref is released.
        unsafe {
            let e = (self.init)();
            if e != 0 {
                return Err(OpenError::Failed("EdsInitializeSDK", e));
            }
            let mut s = Session {
                sdk: self,
                list: std::ptr::null_mut(),
                cam: std::ptr::null_mut(),
            };
            let (mut count, e) = (0, (self.camera_list)(&mut s.list));
            if e != 0 {
                return Err(OpenError::Failed("EdsGetCameraList", e));
            }
            (self.child_count)(s.list, &mut count);
            if count == 0 {
                return Err(OpenError::NoCamera);
            }
            let e = (self.child)(s.list, 0, &mut s.cam);
            if e != 0 {
                return Err(OpenError::Failed("EdsGetChildAtIndex", e));
            }
            let mut info = DeviceInfo {
                port: [0; 256],
                description: [0; 256],
                sub_type: 0,
                reserved: 0,
            };
            if (self.device_info)(s.cam, &mut info) == 0 {
                log!(
                    "camera: {} on {} (sub type {})",
                    c_str(&info.description),
                    c_str(&info.port),
                    info.sub_type
                );
            }
            let e = (self.open_session)(s.cam);
            if e != 0 {
                (self.release)(s.cam);
                s.cam = std::ptr::null_mut();
                return Err(OpenError::Failed("EdsOpenSession", e));
            }
            // Some bodies reject Evf_Mode yet stream anyway; only the output device is required.
            let on = 1u32;
            let _ = (self.set_prop)(s.cam, PROP_EVF_MODE, 0, 4, (&raw const on).cast());
            let pc = OUTPUT_PC;
            let e = (self.set_prop)(s.cam, PROP_EVF_OUTPUT_DEVICE, 0, 4, (&raw const pc).cast());
            if e != 0 {
                return Err(OpenError::Failed("Evf_OutputDevice", e));
            }
            Ok(s)
        }
    }
}

#[derive(Debug)]
pub enum OpenError {
    NoCamera,
    Failed(&'static str, u32),
}

pub enum Frame {
    Ready,
    NotReady,
    Failed(u32),
}

pub struct Session<'a> {
    sdk: &'a Sdk,
    list: Ref,
    cam: Ref,
}

impl Session<'_> {
    /// Downloads one live-view JPEG into `jpeg` (cleared first).
    pub fn frame(&self, jpeg: &mut Vec<u8>) -> Frame {
        let sdk = self.sdk;
        // SAFETY: stream and image refs are released on every path; the stream's buffer is only
        // read before the stream is released.
        unsafe {
            let (mut stream, mut image) = (std::ptr::null_mut(), std::ptr::null_mut());
            let mut e = (sdk.memory_stream)(0, &mut stream);
            if e == 0 {
                e = (sdk.evf_image)(stream, &mut image);
            }
            if e == 0 {
                e = (sdk.download_evf)(self.cam, image);
            }
            let mut out = Frame::Failed(e);
            if e == 0 {
                let (mut p, mut len) = (std::ptr::null_mut(), 0u64);
                if (sdk.pointer)(stream, &mut p) == 0
                    && (sdk.length)(stream, &mut len) == 0
                    && !p.is_null()
                {
                    jpeg.clear();
                    jpeg.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
                    out = Frame::Ready;
                }
            } else if e == ERR_NOT_READY {
                out = Frame::NotReady;
            }
            if !image.is_null() {
                (sdk.release)(image);
            }
            if !stream.is_null() {
                (sdk.release)(stream);
            }
            out
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let sdk = self.sdk;
        // SAFETY: cam and list are either null or live refs owned by this session.
        unsafe {
            if !self.cam.is_null() {
                let off = 0u32;
                let _ = (sdk.set_prop)(
                    self.cam,
                    PROP_EVF_OUTPUT_DEVICE,
                    0,
                    4,
                    (&raw const off).cast(),
                );
                (sdk.close_session)(self.cam);
                (sdk.release)(self.cam);
            }
            if !self.list.is_null() {
                (sdk.release)(self.list);
            }
            (sdk.term)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_str_stops_at_nul_and_trims() {
        assert_eq!(c_str(b"Canon EOS 600D  \0garbage"), "Canon EOS 600D");
        assert_eq!(c_str(b"no terminator"), "no terminator");
        assert_eq!(c_str(&[0; 4]), "");
    }

    #[test]
    fn device_info_matches_the_sdk_layout() {
        assert_eq!(size_of::<DeviceInfo>(), 520);
    }
}
