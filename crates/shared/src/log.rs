//! Size-capped log file shared by every EosVCam process, plus an optional stderr echo.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Sink {
    file: Option<File>,
    path: String,
    old_path: String,
    bytes: u64,
    echo: bool,
    start: Instant,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();
/// A log that grows past this is moved to `<name>.old.log`, replacing the previous one.
const MAX_LOG_BYTES: u64 = 1_000_000;

fn open_append(path: &str) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

impl Sink {
    fn rotate(&mut self) {
        self.file = None;
        let _ = fs::rename(&self.path, &self.old_path);
        self.file = open_append(&self.path);
        self.bytes = 0;
    }
}

/// Opens `<DATA_DIR>\<name>.log`. Failure to open is not fatal; lines then go to stderr only.
pub fn init(name: &str, echo: bool) {
    if SINK.get().is_some() {
        return;
    }
    let path = format!(r"{}\{name}.log", crate::DATA_DIR);
    let _ = fs::create_dir_all(crate::DATA_DIR);
    let mut sink = Sink {
        file: None,
        old_path: format!(r"{}\{name}.old.log", crate::DATA_DIR),
        bytes: fs::metadata(&path).map_or(0, |m| m.len()),
        path,
        echo,
        start: Instant::now(),
    };
    if sink.bytes > MAX_LOG_BYTES {
        sink.rotate();
    } else {
        sink.file = open_append(&sink.path);
    }
    let _ = SINK.set(Mutex::new(sink));
    write(format_args!("---- {name} started ----"));
}

pub fn write(args: fmt::Arguments<'_>) {
    let Some(sink) = SINK.get() else { return };
    let Ok(mut s) = sink.lock() else { return };
    let line = format!("[{:9.3}] {args}\n", s.start.elapsed().as_secs_f64());
    if let Some(f) = s.file.as_mut() {
        let _ = f.write_all(line.as_bytes());
        s.bytes += line.len() as u64;
        if s.bytes > MAX_LOG_BYTES {
            s.rotate();
        }
    }
    if s.echo || s.file.is_none() {
        eprint!("{line}");
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(format_args!($($t)*)) };
}
