//! The feeder: pulls live view from the camera (or a synthetic generator), converts it to NV12
//! and publishes it into the shared ring while some media source is asking for frames.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use eosvcam_shared::log;
use eosvcam_shared::shm::{self, FPS, FRAME_BYTES, HEIGHT, Mapping, WIDTH, now_100ns, status};
use eosvcam_shared::windows_bindings::{
    COINIT_MULTITHREADED, CloseHandle, CoInitializeEx, CreateMutexW, ERROR_ALREADY_EXISTS,
    GetLastError, HANDLE, timeBeginPeriod, timeEndPeriod,
};
use windows_core::PCWSTR;

use crate::decode::Decoder;
use crate::edsdk::{Frame, OpenError, Sdk};

const RETRY_DELAY: Duration = Duration::from_secs(2);
const STALL_LIMIT: Duration = Duration::from_secs(8);

/// Runs until `stop` is set. Only one feeder per machine can run.
pub fn run(stop: &AtomicBool, synthetic: bool) -> Result<()> {
    let name = shm::object_name("EosVCamFeeder");
    // SAFETY: name is a valid NUL-terminated wide string.
    let lock = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())).ok()? };
    // SAFETY: reads the error of the call above on this thread.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS as u32 {
        // SAFETY: lock is a handle we own.
        unsafe { CloseHandle(lock).ok()? };
        bail!("another EosVCam feeder is already running (stop the EosVCamFeeder service first)");
    }
    let _lock = Handle(lock);

    // SAFETY: balanced by timeEndPeriod below; COM stays initialised for the thread's lifetime.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED as u32);
        timeBeginPeriod(1);
    }
    let result = serve(stop, synthetic);
    // SAFETY: matches the timeBeginPeriod above.
    unsafe { timeEndPeriod(1) };
    result
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn serve(stop: &AtomicBool, synthetic: bool) -> Result<()> {
    let map = Mapping::create()?;
    log!(
        "feeder ready ({})",
        if synthetic { "synthetic" } else { "camera" }
    );
    let mut sdk: Option<Sdk> = None;
    let keep = || !stop.load(Ordering::Relaxed) && map.demanded();

    while !stop.load(Ordering::Relaxed) {
        map.beat();
        if !map.demanded() {
            map.set_status(status::IDLE);
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        log!("frames requested, starting capture");
        let outcome = if synthetic {
            synthetic_session(&map, &keep)
        } else {
            if sdk.is_none() {
                sdk = Sdk::load().inspect_err(|e| log!("{e:#}")).ok();
            }
            match &sdk {
                Some(sdk) => camera_session(sdk, &map, &keep),
                None => Err(anyhow::anyhow!("EDSDK unavailable")),
            }
        };
        match outcome {
            Ok(()) => map.set_status(status::IDLE),
            Err(e) => {
                log!("capture ended: {e:#}");
                map.set_status(status::ERROR);
                let retry_at = Instant::now() + RETRY_DELAY;
                while Instant::now() < retry_at && !stop.load(Ordering::Relaxed) {
                    map.beat();
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    map.set_status(status::NO_FEEDER);
    log!("feeder stopped");
    Ok(())
}

// ---------------------------------------------------------------- camera

#[derive(Default)]
struct Slot {
    jpeg: Vec<u8>,
    time: i64,
    fresh: bool,
    closed: bool,
    dropped: u32,
}

/// Single-slot hand-off: the newest JPEG wins, so a slow decode never builds latency.
#[derive(Default)]
struct Mailbox {
    slot: Mutex<Slot>,
    cv: Condvar,
}

impl Mailbox {
    fn put(&self, jpeg: &mut Vec<u8>, time: i64) {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::swap(&mut s.jpeg, jpeg);
        s.dropped += u32::from(s.fresh);
        s.fresh = true;
        s.time = time;
        self.cv.notify_one();
    }

    fn take(&self, jpeg: &mut Vec<u8>) -> Option<i64> {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        while !s.fresh && !s.closed {
            s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if !s.fresh {
            return None;
        }
        std::mem::swap(&mut s.jpeg, jpeg);
        s.fresh = false;
        Some(s.time)
    }

    fn close(&self) -> u32 {
        let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        s.closed = true;
        self.cv.notify_all();
        s.dropped
    }
}

fn camera_session(sdk: &Sdk, map: &Mapping, keep: &(dyn Fn() -> bool + Sync)) -> Result<()> {
    let session = match sdk.open() {
        Ok(s) => s,
        Err(OpenError::NoCamera) => {
            map.set_status(status::NO_CAMERA);
            bail!("no camera found (is it on, connected, and free of other apps?)");
        }
        Err(OpenError::Failed(what, code)) => {
            map.set_status(status::ERROR);
            bail!("{what} failed: 0x{code:X}");
        }
    };
    log!("camera session open");
    map.set_status(status::ERROR);
    let mailbox = Mailbox::default();
    let started = Instant::now();

    let (downloaded, dropped, failure) = thread::scope(|scope| {
        let decoder = scope.spawn(|| decode_loop(&mailbox, map));
        let mut jpeg = Vec::new();
        let mut downloaded = 0u32;
        let mut last_ok = Instant::now();
        let mut logged = 0;
        let mut failure = None;
        while keep() {
            match session.frame(&mut jpeg) {
                Frame::Ready => {
                    mailbox.put(&mut jpeg, now_100ns());
                    downloaded += 1;
                    last_ok = Instant::now();
                    map.beat();
                    continue;
                }
                Frame::NotReady => {}
                Frame::Failed(code) => {
                    if logged < 5 {
                        log!("download error 0x{code:X}");
                        logged += 1;
                    }
                }
            }
            if last_ok.elapsed() > STALL_LIMIT {
                failure = Some("no frames from the camera for 8 s");
                break;
            }
            map.beat();
            thread::sleep(Duration::from_millis(1));
        }
        let dropped = mailbox.close();
        let _ = decoder.join();
        (downloaded, dropped, failure)
    });

    log!(
        "camera session closed after {:.1}s: {downloaded} frames, {dropped} dropped before decode",
        started.elapsed().as_secs_f32()
    );
    match failure {
        Some(msg) => bail!(msg),
        None => Ok(()),
    }
}

fn decode_loop(mailbox: &Mailbox, map: &Mapping) {
    // SAFETY: initialises COM for this thread, which WIC requires.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED as u32);
    }
    let mut decoder = match Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            log!("WIC unavailable: {e:#}");
            map.set_status(status::ERROR);
            return;
        }
    };
    let (mut jpeg, mut nv12) = (Vec::new(), vec![0u8; FRAME_BYTES]);
    let (mut frames, mut errors, mut busy) = (0u32, 0u32, Duration::ZERO);
    while let Some(time) = mailbox.take(&mut jpeg) {
        let t0 = Instant::now();
        match decoder.decode(&jpeg, &mut nv12) {
            Ok((w, h)) => {
                busy += t0.elapsed();
                frames += 1;
                if frames == 1 {
                    log!(
                        "live view is {w}x{h}; first frame decoded in {:?}",
                        t0.elapsed()
                    );
                }
                map.publish(&nv12, time);
                map.set_status(status::LIVE);
            }
            Err(e) => {
                errors += 1;
                if errors <= 5 {
                    log!("decode error: {e:#}");
                }
            }
        }
    }
    if frames > 0 {
        log!(
            "decoder: {frames} frames, {errors} errors, {:.1} ms average",
            busy.as_secs_f64() * 1000.0 / f64::from(frames)
        );
    }
}

// ---------------------------------------------------------------- synthetic

fn synthetic_session(map: &Mapping, keep: &(dyn Fn() -> bool + Sync)) -> Result<()> {
    map.set_status(status::LIVE);
    let mut frame = vec![0u8; FRAME_BYTES];
    let period = Duration::from_micros(1_000_000 / u64::from(FPS));
    let mut next = Instant::now();
    let mut n = 0u32;
    while keep() {
        render_test_frame(n, &mut frame);
        map.publish(&frame, now_100ns());
        map.beat();
        n += 1;
        next += period;
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    log!("synthetic session closed after {n} frames");
    Ok(())
}

const COUNTER_BITS: usize = 16;
const BLOCK: usize = 32;

/// Gradient background, a moving bar, and the frame number as 16 black/white blocks.
pub fn render_test_frame(n: u32, frame: &mut [u8]) {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let (y_plane, uv) = frame.split_at_mut(w * h);
    let bar = (n as usize * 10) % w;
    for (row, line) in y_plane.chunks_exact_mut(w).enumerate() {
        for (x, px) in line.iter_mut().enumerate() {
            *px = (40 + x * 80 / w + row * 40 / h) as u8;
        }
        if row >= BLOCK {
            let end = (bar + 48).min(w);
            line[bar..end].fill(220);
        }
    }
    for (row, line) in uv.chunks_exact_mut(w).enumerate() {
        line.fill(128);
        if row * 2 >= BLOCK {
            let end = (bar + 48).min(w);
            for pair in line[bar & !1..end].as_chunks_mut::<2>().0 {
                *pair = [90, 210];
            }
        }
    }
    for bit in 0..COUNTER_BITS {
        let v = if (n >> bit) & 1 == 1 { 235 } else { 16 };
        for row in 0..BLOCK {
            y_plane[row * w + bit * BLOCK..row * w + (bit + 1) * BLOCK].fill(v);
        }
    }
}

/// Reads back the frame number written by [`render_test_frame`].
pub fn read_test_counter(frame: &[u8]) -> u32 {
    let w = WIDTH as usize;
    (0..COUNTER_BITS).fold(0, |n, bit| {
        n | u32::from(frame[(BLOCK / 2) * w + bit * BLOCK + BLOCK / 2] > 128) << bit
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_round_trips() {
        let mut f = vec![0; FRAME_BYTES];
        for n in [0, 1, 255, 1000, 65_535] {
            render_test_frame(n, &mut f);
            assert_eq!(read_test_counter(&f), n);
        }
    }
}
