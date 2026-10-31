//! Shared-memory frame ring: one feeder process writes, the media source reads.
//!
//! The writer is lock-free. Each slot carries a sequence number that is zeroed while the
//! slot is rewritten, so a reader that copies a slot checks the number again afterwards and
//! drops the copy if it changed. Readers always want the newest frame and skip older ones.

use std::ptr;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering, fence};

use crate::windows_bindings::{
    CloseHandle, ConvertStringSecurityDescriptorToSecurityDescriptorW, CreateEventW,
    CreateFileMappingW, ERROR_ALREADY_EXISTS, EVENT_MODIFY_STATE, FILE_MAP_ALL_ACCESS,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree, MapViewOfFile, OpenEventW,
    OpenFileMappingW, PAGE_READWRITE, PSECURITY_DESCRIPTOR, QueryPerformanceCounter,
    QueryPerformanceFrequency, ResetEvent, SDDL_REVISION_1, SECURITY_ATTRIBUTES, SYNCHRONIZE,
    SetEvent, UnmapViewOfFile, WAIT_OBJECT_0, WaitForSingleObject,
};
use windows_core::{Error, PCWSTR, Result, w};

pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
pub const FPS: u32 = 30;
/// NV12: a full-size Y plane followed by a half-height interleaved UV plane.
pub const FRAME_BYTES: usize = (WIDTH * HEIGHT * 3 / 2) as usize;
pub const SLOTS: usize = 4;
const DATA_OFFSET: usize = 4096;
const MAP_SIZE: usize = DATA_OFFSET + SLOTS * FRAME_BYTES;
const MAGIC: u32 = 0x4543_4F53;
const VERSION: u32 = 1;

/// NUL-terminated wide name `Global\<base>`. With `EOSVCAM_NS` set (self-test) it becomes
/// `Local\<base><ns>`: it cannot clash with an installed feeder, and an unelevated user may
/// create it (Global mappings need SeCreateGlobalPrivilege).
pub fn object_name(base: &str) -> Vec<u16> {
    let ns = std::env::var("EOSVCAM_NS").unwrap_or_default();
    let scope = if ns.is_empty() { "Global" } else { "Local" };
    format!("{scope}\\{base}{ns}")
        .encode_utf16()
        .chain([0])
        .collect()
}
// SYSTEM, admins, LocalService (Frame Server), interactive and authenticated users, app containers.
const SDDL: PCWSTR =
    w!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;LS)(A;;GA;;;IU)(A;;GA;;;AU)(A;;GA;;;AC)");

/// What the feeder reports to readers.
pub mod status {
    pub const NO_FEEDER: u32 = 0;
    pub const IDLE: u32 = 1;
    pub const NO_CAMERA: u32 = 2;
    pub const ERROR: u32 = 3;
    pub const LIVE: u32 = 4;
}

/// How long a heartbeat stays fresh, in 100 ns units.
pub const BEAT_TIMEOUT: i64 = 30_000_000;
/// The feeder opens the camera while demand is newer than this.
pub const DEMAND_TIMEOUT: i64 = 20_000_000;

#[repr(C)]
struct SlotMeta {
    seq: AtomicU64,
    capture_time: AtomicI64,
}

#[repr(C, align(64))]
struct Header {
    magic: AtomicU32,
    version: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    slots: AtomicU32,
    frame_bytes: AtomicU32,
    status: AtomicU32,
    _pad: u32,
    write_seq: AtomicU64,
    feeder_beat: AtomicI64,
    demand_beat: AtomicI64,
    meta: [SlotMeta; SLOTS],
}

const _: () = assert!(size_of::<Header>() <= DATA_OFFSET);

/// Monotonic clock in 100 ns units. Consistent across processes, so frame timestamps can be shared.
pub fn now_100ns() -> i64 {
    let (mut freq, mut count) = (0i64, 0i64);
    // SAFETY: both pointers refer to live locals.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut freq);
        let _ = QueryPerformanceCounter(&mut count);
    }
    let freq = freq.max(1);
    let secs = count / freq;
    let rem = count % freq;
    secs * 10_000_000 + rem * 10_000_000 / freq
}

pub struct Mapping {
    map: HANDLE,
    event: HANDLE,
    base: *mut u8,
    writer: bool,
}

// SAFETY: the view is plain shared memory accessed through atomics or the seqlock protocol,
// and the handles are process-wide kernel objects.
unsafe impl Send for Mapping {}
// SAFETY: as for Send; readers only use atomics and the seqlock, and the single writer is the feeder.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Creates (or takes over) the mapping. Used by the feeder.
    pub fn create() -> Result<Self> {
        let (map_name, event_name) = (
            object_name("EosVCamFrames"),
            object_name("EosVCamFrameReady"),
        );
        let (map_name, event_name) = (PCWSTR(map_name.as_ptr()), PCWSTR(event_name.as_ptr()));
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL is a valid NUL-terminated string and psd is a live out-pointer.
        let sd_ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                SDDL,
                SDDL_REVISION_1 as u32,
                &mut psd,
                None,
            )
            .as_bool()
        };
        if !sd_ok {
            crate::log!("shm: security descriptor rejected, using the default one");
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: if sd_ok { psd.0 } else { ptr::null_mut() },
            bInheritHandle: false.into(),
        };
        let sa_ptr = if sd_ok {
            Some(&sa as *const SECURITY_ATTRIBUTES)
        } else {
            None
        };

        // SAFETY: sa and its descriptor outlive these calls.
        let made = unsafe {
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                sa_ptr,
                PAGE_READWRITE as u32,
                0,
                MAP_SIZE as u32,
                map_name,
            )
            .ok();
            let existed = GetLastError() == ERROR_ALREADY_EXISTS as u32;
            map.and_then(
                |m| match CreateEventW(sa_ptr, true, false, event_name).ok() {
                    Ok(e) => Ok((m, e, existed)),
                    Err(e) => {
                        let _ = CloseHandle(m);
                        Err(e)
                    }
                },
            )
        };
        if sd_ok {
            // SAFETY: psd was allocated by the conversion call above.
            unsafe {
                let _ = LocalFree(HANDLE(psd.0));
            }
        }
        let (map, event, existed) = made?;
        if existed {
            crate::log!("shm: mapping already existed, reusing it");
        }
        let base = match view(map) {
            Ok(b) => b,
            Err(e) => {
                // SAFETY: both handles were just created and are owned here.
                unsafe {
                    let _ = CloseHandle(event);
                    let _ = CloseHandle(map);
                }
                return Err(e);
            }
        };
        let m = Self {
            map,
            event,
            base,
            writer: true,
        };
        let h = m.header();
        h.magic.store(0, Ordering::Release);
        h.version.store(VERSION, Ordering::Relaxed);
        h.width.store(WIDTH, Ordering::Relaxed);
        h.height.store(HEIGHT, Ordering::Relaxed);
        h.slots.store(SLOTS as u32, Ordering::Relaxed);
        h.frame_bytes.store(FRAME_BYTES as u32, Ordering::Relaxed);
        h.status.store(status::IDLE, Ordering::Relaxed);
        h.feeder_beat.store(now_100ns(), Ordering::Relaxed);
        for s in &h.meta {
            s.seq.store(0, Ordering::Relaxed);
        }
        h.write_seq.store(0, Ordering::Relaxed);
        h.magic.store(MAGIC, Ordering::Release);
        Ok(m)
    }

    /// Opens an existing mapping. Used by the media source and `status`.
    pub fn open() -> Result<Self> {
        let (map_name, event_name) = (
            object_name("EosVCamFrames"),
            object_name("EosVCamFrameReady"),
        );
        // SAFETY: the names are valid NUL-terminated strings.
        let (map, event) = unsafe {
            let map =
                OpenFileMappingW(FILE_MAP_ALL_ACCESS as u32, false, PCWSTR(map_name.as_ptr()))
                    .ok()?;
            match OpenEventW(
                (EVENT_MODIFY_STATE | SYNCHRONIZE) as u32,
                false,
                PCWSTR(event_name.as_ptr()),
            )
            .ok()
            {
                Ok(e) => (map, e),
                Err(e) => {
                    let _ = CloseHandle(map);
                    return Err(e);
                }
            }
        };
        let base = match view(map) {
            Ok(b) => b,
            Err(e) => {
                // SAFETY: both handles were just opened and are owned here.
                unsafe {
                    let _ = CloseHandle(event);
                    let _ = CloseHandle(map);
                }
                return Err(e);
            }
        };
        let m = Self {
            map,
            event,
            base,
            writer: false,
        };
        let h = m.header();
        if h.magic.load(Ordering::Acquire) != MAGIC
            || h.version.load(Ordering::Relaxed) != VERSION
            || h.frame_bytes.load(Ordering::Relaxed) as usize != FRAME_BYTES
        {
            return Err(Error::new(
                crate::windows_bindings::E_FAIL,
                "frame ring has an unexpected layout",
            ));
        }
        Ok(m)
    }

    fn header(&self) -> &Header {
        // SAFETY: base points to a live view of at least MAP_SIZE bytes, page aligned, and the
        // header contains only atomics.
        unsafe { &*(self.base as *const Header) }
    }

    fn slot_ptr(&self, seq: u64) -> *mut u8 {
        // SAFETY: the index is below SLOTS, so the offset stays inside the view.
        unsafe {
            self.base
                .add(DATA_OFFSET + (seq as usize % SLOTS) * FRAME_BYTES)
        }
    }

    // ---- writer side ----

    /// Publishes one NV12 frame and wakes readers.
    pub fn publish(&self, frame: &[u8], capture_time: i64) {
        assert!(self.writer && frame.len() == FRAME_BYTES);
        let h = self.header();
        let seq = h.write_seq.load(Ordering::Relaxed) + 1;
        let meta = &h.meta[seq as usize % SLOTS];
        meta.seq.store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        // SAFETY: the slot holds FRAME_BYTES bytes and frame has exactly that length.
        unsafe { ptr::copy_nonoverlapping(frame.as_ptr(), self.slot_ptr(seq), FRAME_BYTES) };
        meta.capture_time.store(capture_time, Ordering::Relaxed);
        meta.seq.store(seq, Ordering::Release);
        h.write_seq.store(seq, Ordering::Release);
        // SAFETY: event is a valid handle owned by self.
        unsafe {
            let _ = SetEvent(self.event);
        }
    }

    pub fn set_status(&self, s: u32) {
        self.header().status.store(s, Ordering::Relaxed);
    }

    pub fn beat(&self) {
        self.header()
            .feeder_beat
            .store(now_100ns(), Ordering::Relaxed);
    }

    /// True while a reader has asked for frames recently.
    pub fn demanded(&self) -> bool {
        now_100ns() - self.header().demand_beat.load(Ordering::Relaxed) < DEMAND_TIMEOUT
    }

    // ---- reader side ----

    pub fn touch_demand(&self) {
        self.header()
            .demand_beat
            .store(now_100ns(), Ordering::Relaxed);
    }

    pub fn status(&self) -> u32 {
        self.header().status.load(Ordering::Relaxed)
    }

    pub fn write_seq(&self) -> u64 {
        self.header().write_seq.load(Ordering::Acquire)
    }

    pub fn feeder_alive(&self) -> bool {
        now_100ns() - self.header().feeder_beat.load(Ordering::Relaxed) < BEAT_TIMEOUT
    }

    /// Copies the newest frame if it is newer than `after`. Returns its sequence and capture time.
    pub fn latest(&self, after: u64, dst: &mut [u8]) -> Option<(u64, i64)> {
        assert!(dst.len() == FRAME_BYTES);
        let h = self.header();
        for _ in 0..4 {
            let w = h.write_seq.load(Ordering::Acquire);
            if w <= after {
                return None;
            }
            let meta = &h.meta[w as usize % SLOTS];
            if meta.seq.load(Ordering::Acquire) != w {
                continue;
            }
            let t = meta.capture_time.load(Ordering::Relaxed);
            // SAFETY: the slot holds FRAME_BYTES bytes and dst has exactly that length.
            unsafe { ptr::copy_nonoverlapping(self.slot_ptr(w), dst.as_mut_ptr(), FRAME_BYTES) };
            fence(Ordering::Acquire);
            if meta.seq.load(Ordering::Relaxed) == w {
                return Some((w, t));
            }
        }
        None
    }

    /// Waits until a frame newer than `after` exists. Returns false on timeout.
    pub fn wait_newer(&self, after: u64, timeout_ms: u32) -> bool {
        if self.write_seq() > after {
            return true;
        }
        // SAFETY: event is a valid handle owned by self.
        unsafe {
            let _ = ResetEvent(self.event);
            if self.write_seq() > after {
                return true;
            }
            WaitForSingleObject(self.event, timeout_ms) == WAIT_OBJECT_0 as u32
                && self.write_seq() > after
        }
    }
}

fn view(map: HANDLE) -> Result<*mut u8> {
    // SAFETY: map is a valid file-mapping handle.
    let v = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS as u32, 0, 0, MAP_SIZE) };
    if v.is_null() {
        Err(Error::from_thread())
    } else {
        Ok(v.cast())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: base, map and event are owned by self and not used afterwards.
        unsafe {
            let _ = UnmapViewOfFile(self.base.cast());
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.map);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(base: &str) -> String {
        String::from_utf16(&object_name(base)[..])
            .unwrap()
            .trim_end_matches('\0')
            .to_owned()
    }

    // One test: it changes a process-wide variable, so it must not run beside another.
    #[test]
    fn ring_round_trip() {
        assert_eq!(name("X"), "Global\\X");
        // SAFETY: no other test in this crate touches the environment.
        unsafe { std::env::set_var("EOSVCAM_NS", format!("unit{}", std::process::id())) };
        assert!(name("X").starts_with("Local\\Xunit"));

        let writer = Mapping::create().unwrap();
        let reader = Mapping::open().unwrap();
        let mut buf = vec![0u8; FRAME_BYTES];
        assert_eq!(reader.latest(0, &mut buf), None);
        assert!(!reader.wait_newer(0, 10));

        for i in 1..=(SLOTS as u8 + 2) {
            writer.publish(&vec![i; FRAME_BYTES], i64::from(i) * 100);
        }
        let last = SLOTS as u64 + 2;
        assert_eq!(reader.write_seq(), last);
        assert_eq!(reader.latest(0, &mut buf), Some((last, last as i64 * 100)));
        assert!(buf.iter().all(|&b| b == last as u8));
        assert_eq!(reader.latest(last, &mut buf), None);
        assert!(reader.wait_newer(last - 1, 10));

        assert!(!writer.demanded());
        reader.touch_demand();
        assert!(writer.demanded());
        writer.beat();
        assert!(reader.feeder_alive());
        writer.set_status(status::LIVE);
        assert_eq!(reader.status(), status::LIVE);
    }
}
