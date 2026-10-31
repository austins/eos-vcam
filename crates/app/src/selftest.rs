//! End-to-end check of the media source without a camera or an installed virtual camera.
//!
//! Runs the synthetic feeder in this process under a private object namespace, loads the real
//! `eosvcam_source.dll`, pulls samples the way Frame Server does and judges pacing and integrity.
//! It cannot prove that Frame Server accepts the camera; `install` plus a camera app does that.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use eosvcam_shared::shm::{FPS, FRAME_BYTES, Mapping};
use eosvcam_shared::windows_bindings::{
    COINIT_MULTITHREADED, CoInitializeEx, GetProcAddress, IClassFactory, IMFActivate,
    IMFMediaSourceEx, IMFMediaStream, IMFSample, LoadLibraryW, MEMediaSample, MENewStream,
    MESourceStarted, MF_VERSION, MFSTARTUP_FULL, MFStartup, PROPVARIANT,
};
use windows_core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCSTR};

use crate::feeder::{self, read_test_counter};

const CLSID: GUID = GUID::from_u128(0x6F0B9D3A_4C1E_4E53_9A57_3D1C2B7E8A41);
type GetClassObject =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut std::ffi::c_void) -> HRESULT;

struct Check {
    ok: bool,
    text: String,
}

fn check(list: &mut Vec<Check>, ok: bool, text: String) {
    println!("[{}] {text}", if ok { "PASS" } else { "FAIL" });
    list.push(Check { ok, text });
}

fn load_source() -> Result<IMFMediaSourceEx> {
    let dll: PathBuf = std::env::current_exe()?.with_file_name("eosvcam_source.dll");
    // SAFETY: loads our own DLL and calls its documented COM export.
    unsafe {
        let m = LoadLibraryW(&HSTRING::from(dll.as_os_str()))
            .ok()
            .with_context(|| format!("loading {}", dll.display()))?;
        let entry = GetProcAddress(m, PCSTR(c"DllGetClassObject".as_ptr().cast()))
            .context("no DllGetClassObject")?;
        let entry: GetClassObject = std::mem::transmute(entry);
        let mut factory = std::ptr::null_mut();
        entry(&CLSID, &IClassFactory::IID, &mut factory).ok()?;
        let factory = IClassFactory::from_raw(factory);
        // Frame Server creates the CLSID as an activator, sets attributes (including SetUnknown), then activates the source.
        let act = factory
            .CreateInstance::<Option<&IUnknown>, IMFActivate>(None)
            .context("CreateInstance(IMFActivate)")?;
        let test_key = GUID::from_u128(0x11111111_2222_3333_4444_555555555555);
        act.SetUnknown(&test_key, None)
            .ok()
            .context("SetUnknown(None)")?;
        act.SetUINT32(&test_key, 42).ok().context("SetUINT32")?;
        let src = act
            .ActivateObject::<IMFMediaSourceEx>()
            .context("ActivateObject")?;
        let _ = src.GetSourceAttributes().context("GetSourceAttributes")?;
        let _ = src.GetStreamAttributes(0).context("GetStreamAttributes")?;
        Ok(src)
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted
        .get(((sorted.len() as f64 - 1.0) * p) as usize)
        .copied()
        .unwrap_or(0.0)
}

pub fn run(seconds: f64) -> Result<()> {
    // SAFETY: nothing else is running yet, so changing the environment cannot race.
    unsafe { std::env::set_var("EOSVCAM_NS", "SelfTest") };
    // SAFETY: plain COM/MF initialisation on the main thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED as u32);
        MFStartup(MF_VERSION as u32, MFSTARTUP_FULL as u32).ok()?;
    }
    let stop = AtomicBool::new(false);
    let mut checks = Vec::new();

    let report = thread::scope(|scope| -> Result<()> {
        let feeder = scope.spawn(|| feeder::run(&stop, true));
        let outcome = drive(seconds, &mut checks);
        stop.store(true, Ordering::Relaxed);
        match feeder.join() {
            Ok(Err(e)) => bail!("feeder failed: {e:#}"),
            Err(_) => bail!("feeder panicked"),
            Ok(Ok(())) => {}
        }
        outcome
    });
    report?;

    let failed = checks.iter().filter(|c| !c.ok).count();
    if failed > 0 {
        let names: Vec<_> = checks
            .iter()
            .filter(|c| !c.ok)
            .map(|c| c.text.as_str())
            .collect();
        bail!("{failed} check(s) failed: {}", names.join("; "));
    }
    println!("\nAll checks passed.");
    Ok(())
}

fn drive(seconds: f64, checks: &mut Vec<Check>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let map = loop {
        match Mapping::open() {
            Ok(m) => break m,
            Err(e) if Instant::now() > deadline => {
                bail!("feeder never created the frame ring: {e}")
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    };

    let source = load_source()?;
    // SAFETY: the selftest calls the source's COM methods on a live interface it owns, with valid arguments.
    let pd = unsafe { source.CreatePresentationDescriptor()? };
    let t_start = Instant::now();
    // SAFETY: as above.
    unsafe {
        source
            .Start(&pd, &GUID::zeroed(), &PROPVARIANT::default())
            .ok()?
    };

    let mut stream: Option<IMFMediaStream> = None;
    let mut started = false;
    while !started {
        // SAFETY: as above.
        let event = unsafe { source.GetEvent(Default::default())? };
        // SAFETY: event is a live interface returned just above.
        match unsafe { event.GetType()? }.0 {
            t if t == MENewStream as u32 => {
                // SAFETY: as above.
                let value = unsafe { event.GetValue()? };
                stream = Some(value.into_unknown()?.cast()?);
            }
            t if t == MESourceStarted as u32 => started = true,
            _ => {}
        }
    }
    let stream = stream.context("source never announced its stream")?;
    check(
        checks,
        true,
        "source started and announced one stream".into(),
    );

    let (mut arrivals, mut counters, mut times) = (Vec::new(), Vec::new(), Vec::new());
    let mut first_frame = None;
    while t_start.elapsed().as_secs_f64() < seconds {
        // SAFETY: stream and its events are live interfaces owned here.
        unsafe { stream.RequestSample(None).ok()? };
        let sample: IMFSample = loop {
            // SAFETY: as above.
            let event = unsafe { stream.GetEvent(Default::default())? };
            // SAFETY: as above.
            if unsafe { event.GetType()? }.0 == MEMediaSample as u32 {
                // SAFETY: as above.
                break unsafe { event.GetValue()? }.into_unknown()?.cast()?;
            }
        };
        let now = Instant::now();
        first_frame.get_or_insert(now - t_start);
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0);
        // SAFETY: the buffer is locked for the duration of the read.
        let counter = unsafe {
            let buf = sample.ConvertToContiguousBuffer()?;
            buf.Lock(&mut ptr, None, Some(&mut len)).ok()?;
            let c = (len as usize == FRAME_BYTES)
                .then(|| read_test_counter(std::slice::from_raw_parts(ptr, FRAME_BYTES)));
            buf.Unlock().ok()?;
            c
        };
        arrivals.push(now);
        counters.push(counter.unwrap_or(u32::MAX));
        // SAFETY: sample is live.
        times.push(unsafe { sample.GetSampleTime()? });
    }

    // SAFETY: the source is live and no longer used afterwards.
    unsafe {
        source.Stop().ok()?;
        source.Shutdown().ok()?;
    }

    let delivered = counters.len();
    let wanted = (seconds * f64::from(FPS) * 0.85) as usize;
    check(
        checks,
        delivered >= wanted,
        format!("delivered {delivered} samples in {seconds:.0}s (need {wanted})"),
    );
    check(
        checks,
        counters.iter().all(|&c| c != u32::MAX),
        "every sample had the expected size".into(),
    );
    let repeats = counters.windows(2).filter(|w| w[0] == w[1]).count();
    check(
        checks,
        repeats * 20 <= delivered,
        format!("{repeats} repeated frames of {delivered}"),
    );
    check(
        checks,
        counters.windows(2).all(|w| w[1] >= w[0]),
        "frame numbers never went backwards".into(),
    );
    let gaps = counters
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]).saturating_sub(1))
        .sum::<u32>();
    check(
        checks,
        gaps as usize * 10 <= delivered,
        format!("{gaps} frames skipped between samples"),
    );
    check(
        checks,
        times.windows(2).all(|w| w[1] > w[0]),
        "sample times strictly increase".into(),
    );

    let mut intervals: Vec<f64> = arrivals
        .windows(2)
        .map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0)
        .collect();
    intervals.sort_by(f64::total_cmp);
    let (p50, p95, max) = (
        percentile(&intervals, 0.5),
        percentile(&intervals, 0.95),
        percentile(&intervals, 1.0),
    );
    println!("      sample interval ms: p50 {p50:.1}, p95 {p95:.1}, max {max:.1}");
    check(
        checks,
        p95 < 60.0,
        format!("p95 interval {p95:.1} ms is under 60 ms"),
    );
    let first = first_frame.unwrap_or_default();
    check(
        checks,
        first < Duration::from_secs(2),
        format!("first frame after {} ms", first.as_millis()),
    );

    // The feeder should stop capturing shortly after the source stops asking.
    let released = Instant::now() + Duration::from_secs(6);
    while map.demanded() && Instant::now() < released {
        thread::sleep(Duration::from_millis(100));
    }
    check(
        checks,
        !map.demanded(),
        "demand cleared after the source stopped".into(),
    );
    Ok(())
}

/// Asks the running feeder for frames for a while and reports what it publishes.
pub fn demand(seconds: u64) -> Result<()> {
    let map = Mapping::open()
        .context("no feeder is running (start the EosVCamFeeder service or `eosvcam console`)")?;
    let mut last = map.write_seq();
    for second in 1..=seconds {
        for _ in 0..4 {
            map.touch_demand();
            thread::sleep(Duration::from_millis(250));
        }
        let now = map.write_seq();
        println!("{second:>3}s  status {}  {} fps", map.status(), now - last);
        last = now;
    }
    println!("status codes: 1 idle, 2 no camera, 3 starting or failing, 4 live");
    Ok(())
}
