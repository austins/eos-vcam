use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eosvcam_shared::log;
use eosvcam_shared::shm::{FPS, FRAME_BYTES, HEIGHT, Mapping, WIDTH, now_100ns, status};
use windows::Win32::Foundation::{E_FAIL, E_POINTER, S_OK};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSCAMERAPROFILE_HighFrameRate, KSCAMERAPROFILE_Legacy,
    KSIDENTIFIER, PINNAME_VIDEO_CAPTURE,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{
    AsImpl, BOOL, Error, GUID, HRESULT, IUnknown, Interface, PCSTR, PCWSTR, PWSTR, Ref, Result,
    implement, w,
};

use crate::forward_mf_attributes;

const SAMPLE_DURATION: i64 = 10_000_000 / FPS as i64;
const LIVE_WAIT_MS: u32 = 150;
const IDLE_WAIT_MS: u32 = 33;
const REOPEN_INTERVAL: i64 = 10_000_000;
const ERROR_SET_NOT_FOUND: HRESULT = HRESULT(0x8007_0492_u32 as i32);

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

fn black(frame: &mut [u8]) {
    let (y, uv) = frame.split_at_mut((WIDTH * HEIGHT) as usize);
    y.fill(16);
    uv.fill(128);
}

fn add_sensor_profiles(attrs: &IMFAttributes) -> Result<()> {
    type CreateCol = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
    type CreateProf =
        unsafe extern "system" fn(*const GUID, u32, PCWSTR, *mut *mut c_void) -> HRESULT;
    // SAFETY: dynamically resolves mfsensorgroup.dll (present on Windows 10/11, absent under Wine) and adds stream 0 profiles.
    unsafe {
        let m = LoadLibraryW(w!("mfsensorgroup.dll"))?;
        let mk_col: CreateCol = std::mem::transmute(
            GetProcAddress(m, PCSTR(c"MFCreateSensorProfileCollection".as_ptr().cast()))
                .ok_or_else(|| Error::from(E_FAIL))?,
        );
        let mk_prof: CreateProf = std::mem::transmute(
            GetProcAddress(m, PCSTR(c"MFCreateSensorProfile".as_ptr().cast()))
                .ok_or_else(|| Error::from(E_FAIL))?,
        );
        let mut raw = std::ptr::null_mut();
        mk_col(&mut raw).ok()?;
        let col = IMFSensorProfileCollection::from_raw(raw);
        for (ty, filter) in [
            (&KSCAMERAPROFILE_Legacy, w!("((RES==;FRT<=30,1;SUT==))")),
            (
                &KSCAMERAPROFILE_HighFrameRate,
                w!("((RES==;FRT>=60,1;SUT==))"),
            ),
        ] {
            let mut p = std::ptr::null_mut();
            mk_prof(ty, 0, PCWSTR::null(), &mut p).ok()?;
            let prof = IMFSensorProfile::from_raw(p);
            prof.AddProfileFilter(0, filter)?;
            col.AddProfile(&prof)?;
        }
        attrs.SetUnknown(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, &col)
    }
}

pub fn create_source(init_attrs: Option<&IMFAttributes>) -> Result<IMFMediaSourceEx> {
    // SAFETY: MFStartup has no preconditions; it is reference counted.
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
    let mt = media_type()?;
    // SAFETY: the media type is valid and the descriptor array holds one entry.
    let desc = unsafe { MFCreateStreamDescriptor(0, &[Some(mt.clone())])? };
    let mut stream_attrs = None;
    // SAFETY: the out pointer is a live local.
    unsafe {
        MFCreateAttributes(&mut stream_attrs, 4)?;
    }
    let stream_attrs = stream_attrs.ok_or_else(|| Error::from(E_FAIL))?;
    // SAFETY: attribute setters and the type handler are called on live interfaces with valid arguments.
    unsafe {
        desc.GetMediaTypeHandler()?.SetCurrentMediaType(&mt)?;
        for a in [desc.cast::<IMFAttributes>()?, stream_attrs.clone()] {
            a.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
            a.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
            a.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
            a.SetUINT32(
                &MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
                MFFrameSourceTypes_Color.0 as u32,
            )?;
        }
    }
    // SAFETY: the descriptor slice outlives the call.
    let pd = unsafe { MFCreatePresentationDescriptor(Some(&[Some(desc.clone())]))? };
    // SAFETY: stream 0 exists in the descriptor just created.
    unsafe { pd.SelectStream(0)? };

    let mut attrs = None;
    // SAFETY: the out pointer is a live local.
    unsafe { MFCreateAttributes(&mut attrs, 10)? };
    let attrs = attrs.ok_or_else(|| Error::from(E_FAIL))?;
    if let Some(init) = init_attrs {
        // SAFETY: both attribute stores are valid.
        let _ = unsafe { init.CopyAllItems(&attrs) };
    }
    // Wine lacks mfsensorgroup.dll, so log and continue if sensor profiles are unavailable.
    if let Err(e) = add_sensor_profiles(&attrs) {
        log!("sensor profiles skipped: {e}");
    }

    // SAFETY: MFCreateEventQueue has no preconditions.
    let queue = unsafe { MFCreateEventQueue()? };
    let src = windows::core::ComObject::new(Source {
        attrs,
        queue,
        pd,
        stream: Mutex::new(None),
        shut: AtomicBool::new(false),
        started_once: AtomicBool::new(false),
    });
    let iface: IMFMediaSourceEx = src.to_interface();
    let stream = Stream::create(iface.cast()?, desc, stream_attrs)?;
    *src.stream.lock().map_err(|_| Error::from(E_FAIL))? = Some(stream);
    log!("source created");
    Ok(iface)
}

fn media_type() -> Result<IMFMediaType> {
    // SAFETY: plain attribute setters on a fresh media type.
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, pack(WIDTH, HEIGHT))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE_RANGE_MIN, pack(FPS, 1))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE_RANGE_MAX, pack(FPS, 1))?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        mt.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        mt.SetUINT32(&MF_MT_DEFAULT_STRIDE, WIDTH)?;
        mt.SetUINT32(&MF_MT_SAMPLE_SIZE, FRAME_BYTES as u32)?;
        mt.SetUINT32(&MF_MT_AVG_BITRATE, (FRAME_BYTES as u32) * 8 * FPS)?;
        mt.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        mt.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        mt.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        mt.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(mt)
    }
}

/// Forwards `IMFMediaEventGenerator` to the type's `queue` field.
macro_rules! event_generator {
    ($ty:ident) => {
        impl IMFMediaEventGenerator_Impl for $ty {
            fn GetEvent(
                &self,
                flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
            ) -> Result<IMFMediaEvent> {
                // SAFETY: the queue is a live interface owned by self.
                unsafe { self.queue.GetEvent(flags.0 as u32) }
            }
            fn BeginGetEvent(
                &self,
                cb: Ref<'_, IMFAsyncCallback>,
                state: Ref<'_, IUnknown>,
            ) -> Result<()> {
                // SAFETY: the queue is live; the callback and state come from the caller.
                unsafe { self.queue.BeginGetEvent(cb.as_ref(), state.as_ref()) }
            }
            fn EndGetEvent(&self, res: Ref<'_, IMFAsyncResult>) -> Result<IMFMediaEvent> {
                // SAFETY: the queue is live; the async result comes from the caller.
                unsafe { self.queue.EndGetEvent(res.as_ref()) }
            }
            fn QueueEvent(
                &self,
                met: u32,
                ext: *const GUID,
                hr: HRESULT,
                val: *const PROPVARIANT,
            ) -> Result<()> {
                // SAFETY: the queue is live; the caller's pointers are forwarded as given.
                unsafe { self.queue.QueueEventParamVar(met, ext, hr, val) }
            }
        }
    };
}

fn queue_event(q: &IMFMediaEventQueue, kind: MF_EVENT_TYPE) -> Result<()> {
    // SAFETY: an empty variant and the null GUID are valid event parameters.
    unsafe {
        q.QueueEventParamVar(
            kind.0 as u32,
            &GUID::zeroed(),
            S_OK,
            &PROPVARIANT::default(),
        )
    }
}

// ---------------------------------------------------------------- source

#[implement(IMFMediaSourceEx, IMFAttributes, IMFGetService, IKsControl)]
struct Source {
    attrs: IMFAttributes,
    queue: IMFMediaEventQueue,
    pd: IMFPresentationDescriptor,
    stream: Mutex<Option<IMFMediaStream2>>,
    shut: AtomicBool,
    started_once: AtomicBool,
}

event_generator!(Source_Impl);
forward_mf_attributes!(Source_Impl, attrs);

impl Source_Impl {
    fn check(&self) -> Result<()> {
        if self.shut.load(Ordering::Acquire) {
            Err(MF_E_SHUTDOWN.into())
        } else {
            Ok(())
        }
    }

    fn stream(&self) -> Result<IMFMediaStream2> {
        self.stream
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }
}

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        self.check()?;
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        log!("CreatePresentationDescriptor");
        self.check()?;
        // SAFETY: pd is a live descriptor owned by self.
        unsafe { self.pd.Clone() }
    }

    fn Start(
        &self,
        _pd: Ref<'_, IMFPresentationDescriptor>,
        _fmt: *const GUID,
        pos: *const PROPVARIANT,
    ) -> Result<()> {
        log!("Start");
        self.check()?;
        let stream = self.stream()?;
        let first = !self.started_once.swap(true, Ordering::AcqRel);
        let kind = if first { MENewStream } else { MEUpdatedStream };
        let unk: IUnknown = stream.cast()?;
        // SAFETY: the queue and stream are valid; pos is either null or a caller-owned variant.
        unsafe {
            self.queue
                .QueueEventParamUnk(kind.0 as u32, &GUID::zeroed(), S_OK, &unk)?;
            let empty = PROPVARIANT::default();
            let pos = if pos.is_null() {
                &empty as *const PROPVARIANT
            } else {
                pos
            };
            self.queue
                .QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, pos)?;
        }
        stream_ref(&stream)?.start()
    }

    fn Stop(&self) -> Result<()> {
        log!("Stop");
        self.check()?;
        stream_ref(&self.stream()?)?.stop()?;
        queue_event(&self.queue, MESourceStopped)
    }

    fn Pause(&self) -> Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }

    fn Shutdown(&self) -> Result<()> {
        log!("Shutdown");
        self.shut.store(true, Ordering::Release);
        if let Some(s) = self.stream.lock().ok().and_then(|mut s| s.take()) {
            if let Ok(s) = stream_ref(&s) {
                s.shutdown();
            }
        }
        // SAFETY: the queue is a live interface owned by self.
        unsafe { self.queue.Shutdown() }
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        log!("GetSourceAttributes");
        self.check()?;
        Ok(self.attrs.clone())
    }

    fn GetStreamAttributes(&self, id: u32) -> Result<IMFAttributes> {
        log!("GetStreamAttributes id={id}");
        self.check()?;
        if id != 0 {
            return Err(MF_E_INVALIDSTREAMNUMBER.into());
        }
        Ok(stream_ref(&self.stream()?)?.attrs.clone())
    }

    fn SetD3DManager(&self, _mgr: Ref<'_, IUnknown>) -> Result<()> {
        log!("SetD3DManager");
        Ok(())
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(
        &self,
        _svc: *const GUID,
        _riid: *const GUID,
        out: *mut *mut c_void,
    ) -> Result<()> {
        if out.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: out was checked for null.
        unsafe { *out = std::ptr::null_mut() };
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for Source_Impl {
    fn KsProperty(
        &self,
        _p: *const KSIDENTIFIER,
        _l: u32,
        _d: *mut c_void,
        _dl: u32,
        r: *mut u32,
    ) -> Result<()> {
        if !r.is_null() {
            // SAFETY: r was checked for null.
            unsafe { *r = 0 };
        }
        Err(ERROR_SET_NOT_FOUND.into())
    }
    fn KsMethod(
        &self,
        _p: *const KSIDENTIFIER,
        _l: u32,
        _d: *mut c_void,
        _dl: u32,
        r: *mut u32,
    ) -> Result<()> {
        if !r.is_null() {
            // SAFETY: r was checked for null.
            unsafe { *r = 0 };
        }
        Err(ERROR_SET_NOT_FOUND.into())
    }
    fn KsEvent(
        &self,
        _p: *const KSIDENTIFIER,
        _l: u32,
        _d: *mut c_void,
        _dl: u32,
        r: *mut u32,
    ) -> Result<()> {
        if !r.is_null() {
            // SAFETY: r was checked for null.
            unsafe { *r = 0 };
        }
        Err(ERROR_SET_NOT_FOUND.into())
    }
}

// ---------------------------------------------------------------- stream

#[implement(IMFMediaStream2, IMFAttributes, IKsControl)]
struct Stream {
    attrs: IMFAttributes,
    queue: IMFMediaEventQueue,
    source: Mutex<Option<IMFMediaSource>>,
    desc: IMFStreamDescriptor,
    tx: Mutex<Option<Sender<Option<IUnknown>>>>,
    running: Arc<AtomicBool>,
    state: AtomicI32,
}

/// Borrows the Rust object behind a stream interface that this module created.
fn stream_ref(s: &IMFMediaStream2) -> Result<&Stream> {
    // SAFETY: every IMFMediaStream2 stored in Source was produced by Stream::create.
    Ok(unsafe { AsImpl::<Stream>::as_impl(s) })
}

impl Stream {
    fn create(
        source: IMFMediaSource,
        desc: IMFStreamDescriptor,
        attrs: IMFAttributes,
    ) -> Result<IMFMediaStream2> {
        // SAFETY: MFCreateEventQueue has no preconditions.
        let queue = unsafe { MFCreateEventQueue()? };
        let (tx, rx) = channel();
        let running = Arc::new(AtomicBool::new(false));
        let job = Handoff((Worker::new(queue.clone(), running.clone()), rx));
        std::thread::Builder::new()
            .name("eosvcam-frames".into())
            .spawn(move || {
                let job = job; // capture the whole wrapper, not its fields
                let Handoff((worker, rx)) = job;
                worker.run(rx)
            })
            .map_err(|_| Error::from(E_FAIL))?;
        Ok(Self {
            attrs,
            queue,
            source: Mutex::new(Some(source)),
            desc,
            tx: Mutex::new(Some(tx)),
            running,
            state: AtomicI32::new(MF_STREAM_STATE_STOPPED.0),
        }
        .into())
    }

    fn start(&self) -> Result<()> {
        self.state
            .store(MF_STREAM_STATE_RUNNING.0, Ordering::Release);
        self.running.store(true, Ordering::Release);
        queue_event(&self.queue, MEStreamStarted)
    }

    fn stop(&self) -> Result<()> {
        self.state
            .store(MF_STREAM_STATE_STOPPED.0, Ordering::Release);
        self.running.store(false, Ordering::Release);
        queue_event(&self.queue, MEStreamStopped)
    }

    fn shutdown(&self) {
        self.running.store(false, Ordering::Release);
        if let Ok(mut s) = self.source.lock() {
            *s = None;
        }
        if let Ok(mut t) = self.tx.lock() {
            *t = None;
        }
        // SAFETY: the queue is a live interface owned by self.
        unsafe {
            let _ = self.queue.Shutdown();
        }
    }
}

event_generator!(Stream_Impl);
forward_mf_attributes!(Stream_Impl, attrs);

impl IKsControl_Impl for Stream_Impl {
    fn KsProperty(
        &self,
        _p: *const KSIDENTIFIER,
        _l: u32,
        _d: *mut c_void,
        _dl: u32,
        r: *mut u32,
    ) -> Result<()> {
        if !r.is_null() {
            // SAFETY: r was checked for null.
            unsafe { *r = 0 };
        }
        Err(ERROR_SET_NOT_FOUND.into())
    }
    fn KsMethod(
        &self,
        _p: *const KSIDENTIFIER,
        _l: u32,
        _d: *mut c_void,
        _dl: u32,
        r: *mut u32,
    ) -> Result<()> {
        if !r.is_null() {
            // SAFETY: r was checked for null.
            unsafe { *r = 0 };
        }
        Err(ERROR_SET_NOT_FOUND.into())
    }
    fn KsEvent(
        &self,
        _p: *const KSIDENTIFIER,
        _l: u32,
        _d: *mut c_void,
        _dl: u32,
        r: *mut u32,
    ) -> Result<()> {
        if !r.is_null() {
            // SAFETY: r was checked for null.
            unsafe { *r = 0 };
        }
        Err(ERROR_SET_NOT_FOUND.into())
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        self.source
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        Ok(self.desc.clone())
    }

    fn RequestSample(&self, token: Ref<'_, IUnknown>) -> Result<()> {
        let tx = self.tx.lock().map_err(|_| Error::from(E_FAIL))?;
        match tx.as_ref() {
            Some(tx) => tx
                .send(token.as_ref().cloned())
                .map_err(|_| MF_E_SHUTDOWN.into()),
            None => Err(MF_E_SHUTDOWN.into()),
        }
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, state: MF_STREAM_STATE) -> Result<()> {
        log!("SetStreamState {}", state.0);
        match state {
            MF_STREAM_STATE_RUNNING => {
                self.state.store(state.0, Ordering::Release);
                self.running.store(true, Ordering::Release);
                Ok(())
            }
            MF_STREAM_STATE_PAUSED | MF_STREAM_STATE_STOPPED => {
                self.state.store(state.0, Ordering::Release);
                self.running.store(false, Ordering::Release);
                Ok(())
            }
            _ => Err(MF_E_INVALID_STATE_TRANSITION.into()),
        }
    }

    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        Ok(MF_STREAM_STATE(self.state.load(Ordering::Acquire)))
    }
}

// ---------------------------------------------------------------- worker

/// Moves free-threaded Media Foundation objects (event queue, sample tokens) to the worker thread.
struct Handoff<T>(T);
// SAFETY: the wrapped Media Foundation objects are agile and safe to use from any thread.
unsafe impl<T> Send for Handoff<T> {}

/// Answers `RequestSample` calls with the newest frame from the ring.
struct Worker {
    queue: IMFMediaEventQueue,
    running: Arc<AtomicBool>,
    map: Option<Mapping>,
    last_try: i64,
    seq: u64,
    last_time: i64,
    frame: Vec<u8>,
    scratch: Vec<u8>,
    delivered: u64,
    repeated: u64,
}

impl Worker {
    fn new(queue: IMFMediaEventQueue, running: Arc<AtomicBool>) -> Self {
        let mut frame = vec![0; FRAME_BYTES];
        black(&mut frame);
        Self {
            queue,
            running,
            map: None,
            last_try: 0,
            seq: 0,
            last_time: 0,
            frame,
            scratch: vec![0; FRAME_BYTES],
            delivered: 0,
            repeated: 0,
        }
    }

    fn run(mut self, rx: Receiver<Option<IUnknown>>) {
        while let Ok(token) = rx.recv() {
            if !self.running.load(Ordering::Acquire) {
                continue;
            }
            if let Err(e) = self.serve(token) {
                log!("serve failed: {e}");
                std::thread::sleep(Duration::from_millis(IDLE_WAIT_MS.into()));
            }
        }
        log!(
            "worker exit: {} delivered, {} repeated",
            self.delivered,
            self.repeated
        );
    }

    fn connect(&mut self) {
        let now = now_100ns();
        let stale = self.map.as_ref().is_none_or(|m| !m.feeder_alive());
        if stale && now - self.last_try > REOPEN_INTERVAL {
            self.last_try = now;
            if let Ok(m) = Mapping::open() {
                if self.map.is_none() {
                    log!("connected to frame ring");
                }
                // Keep `seq`: a dead feeder's mapping survives, and resetting would replay its last frame.
                if self.map.replace(m).is_none() {
                    self.seq = 0;
                }
            }
        }
    }

    fn serve(&mut self, token: Option<IUnknown>) -> Result<()> {
        self.connect();
        let mut fresh = false;
        let mut time = 0;
        match &self.map {
            Some(m) => {
                m.touch_demand();
                if m.write_seq() < self.seq {
                    self.seq = 0;
                }
                let live = m.status() == status::LIVE && m.feeder_alive();
                if m.wait_newer(self.seq, if live { LIVE_WAIT_MS } else { IDLE_WAIT_MS }) {
                    // Read aside so a rejected snapshot cannot corrupt the frame we repeat.
                    if let Some((s, t)) = m.latest(self.seq, &mut self.scratch) {
                        std::mem::swap(&mut self.frame, &mut self.scratch);
                        self.seq = s;
                        time = t;
                        fresh = true;
                    }
                }
                if !fresh && !live {
                    black(&mut self.frame);
                }
            }
            None => std::thread::sleep(Duration::from_millis(IDLE_WAIT_MS.into())),
        }
        if fresh {
            self.delivered += 1;
        } else {
            self.repeated += 1;
            time = now_100ns();
        }
        time = time.max(self.last_time + 1);
        self.last_time = time;
        self.deliver(token, time)
    }

    fn deliver(&self, token: Option<IUnknown>, time: i64) -> Result<()> {
        if !self.running.load(Ordering::Acquire) {
            return Ok(());
        }
        // SAFETY: the buffer is locked for exactly FRAME_BYTES bytes and unlocked before use.
        unsafe {
            let buf = MFCreateMemoryBuffer(FRAME_BYTES as u32)?;
            let mut dst = std::ptr::null_mut();
            buf.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(self.frame.as_ptr(), dst, FRAME_BYTES);
            buf.Unlock()?;
            buf.SetCurrentLength(FRAME_BYTES as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(SAMPLE_DURATION)?;
            if let Some(t) = &token {
                sample.SetUnknown(&MFSampleExtension_Token, t)?;
            }
            self.queue
                .QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample)
        }
    }
}
