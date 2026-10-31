//! Installs, removes and inspects the EosVCam virtual camera and feeder service.

use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::{fs, thread};

use anyhow::{Context, Result, bail};
use eosvcam_shared::shm::{Mapping, status};
use eosvcam_shared::windows_bindings::{
    COINIT_MULTITHREADED, CoInitializeEx, GetProcAddress, IMFVirtualCamera, LoadLibraryW,
    MF_VERSION, MFSTARTUP_FULL, MFStartup, MFVirtualCameraAccess, MFVirtualCameraAccess_AllUsers,
    MFVirtualCameraLifetime, MFVirtualCameraLifetime_System, MFVirtualCameraType,
    MFVirtualCameraType_SoftwareCameraSource,
};
use eosvcam_shared::{CAMERA_NAME, DATA_DIR, SERVICE_NAME, SOURCE_CLSID};
use windows_core::{GUID, HRESULT, HSTRING, Interface, PCWSTR, s, w};

const SOURCE_DLL: &str = "eosvcam_source.dll";
/// Canon's SDK files; copied when they sit next to the installer.
const SDK_FILES: [&str; 2] = crate::sdk::FILES;

fn install_dir() -> PathBuf {
    Path::new(&std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into()))
        .join("EosVCam")
}

/// Creates the data folder with a protected ACL. The feeder service (LocalSystem) writes and
/// trims logs here, so a folder a standard user pre-created or redirected must not be trusted.
fn secure_data_dir() -> Result<()> {
    const REPARSE_POINT: u32 = 0x400;
    if let Ok(meta) = fs::symlink_metadata(DATA_DIR)
        && meta.file_attributes() & REPARSE_POINT != 0
    {
        fs::remove_dir(DATA_DIR).context("removing a redirected data folder")?;
    }
    fs::create_dir_all(DATA_DIR)?;
    for log in ["feeder.log", "source.log"] {
        let _ = fs::remove_file(Path::new(DATA_DIR).join(log));
    }
    // SYSTEM and Administrators full, LocalService (Frame Server writes source.log) modify,
    // Users read and execute only.
    let secured = run(
        "icacls.exe",
        &[DATA_DIR, "/setowner", "*S-1-5-32-544", "/t", "/c", "/q"],
    ) && run(
        "icacls.exe",
        &[
            DATA_DIR,
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            "*S-1-5-19:(OI)(CI)M",
            "*S-1-5-32-545:(OI)(CI)RX",
            "/t",
            "/c",
            "/q",
        ],
    );
    if !secured {
        bail!("could not secure {DATA_DIR}");
    }
    Ok(())
}

/// Runs a command and returns whether it succeeded. Output is printed only on failure.
fn run(program: &str, args: &[&str]) -> bool {
    match Command::new(program).args(args).output() {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            // Tool output is in the OEM code page and unreadable as UTF-8, so report the exit code only.
            println!("  {program} {} exited with {}", args.join(" "), o.status);
            false
        }
        Err(e) => {
            println!("  could not run {program}: {e}");
            false
        }
    }
}

type CreateVirtualCamera = unsafe extern "system" fn(
    MFVirtualCameraType,
    MFVirtualCameraLifetime,
    MFVirtualCameraAccess,
    PCWSTR,
    PCWSTR,
    *const GUID,
    u32,
    *mut *mut c_void,
) -> HRESULT;

/// Binds MFCreateVirtualCamera at run time so the program still starts (and says why it can't
/// continue) on systems without it.
fn virtual_camera() -> Result<IMFVirtualCamera> {
    // SAFETY: plain COM/MF initialisation, then a call through a function pointer whose
    // signature matches mfvirtualcamera.h.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED as u32);
        MFStartup(MF_VERSION as u32, MFSTARTUP_FULL as u32).ok()?;
        let lib = LoadLibraryW(w!("mfsensorgroup.dll"))
            .ok()
            .context("this Windows has no virtual camera support (needs Windows 11)")?;
        let entry = GetProcAddress(lib, s!("MFCreateVirtualCamera"))
            .context("MFCreateVirtualCamera is missing")?;
        let create: CreateVirtualCamera = std::mem::transmute(entry);
        let (name, id) = (HSTRING::from(CAMERA_NAME), HSTRING::from(SOURCE_CLSID));
        let mut out = std::ptr::null_mut();
        create(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_System,
            MFVirtualCameraAccess_AllUsers,
            PCWSTR(name.as_ptr()),
            PCWSTR(id.as_ptr()),
            std::ptr::null(),
            0,
            &mut out,
        )
        .ok()?;
        Ok(IMFVirtualCamera::from_raw(out))
    }
}

fn copy_with_retry(from: &Path, to: &Path) -> Result<()> {
    for attempt in 0..20 {
        match fs::copy(from, to) {
            Ok(_) => return Ok(()),
            Err(e) if attempt == 19 => {
                return Err(e).with_context(|| format!("copying to {}", to.display()));
            }
            Err(_) => thread::sleep(Duration::from_millis(500)),
        }
    }
    Ok(())
}

pub fn install(sdk_arg: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let src_dir = exe.parent().context("no exe folder")?;
    if !src_dir.join(SOURCE_DLL).is_file() {
        bail!("{SOURCE_DLL} must sit next to eosvcam.exe");
    }
    let dir = install_dir();
    if fs::canonicalize(src_dir).ok() == fs::canonicalize(&dir).ok() {
        bail!(
            "run install from the downloaded package folder, not from {}",
            dir.display()
        );
    }
    // Resolve the SDK first so a bad path cannot leave the machine with the old install removed.
    let located = crate::sdk::locate(src_dir, sdk_arg)?;

    println!("Removing any previous install...");
    remove_all();
    // Frame Server keeps the old DLL loaded; it restarts on demand.
    run("sc.exe", &["stop", "FrameServer"]);

    println!("Copying files to {}", dir.display());
    fs::create_dir_all(&dir)?;
    copy_with_retry(&exe, &dir.join("eosvcam.exe"))?;
    copy_with_retry(&src_dir.join(SOURCE_DLL), &dir.join(SOURCE_DLL))?;
    let sdk_present = located.is_some();
    if let Some(l) = &located {
        for f in SDK_FILES {
            copy_with_retry(&l.dir.join(f), &dir.join(f))?;
        }
    }
    if !sdk_present {
        println!(
            "  note: no Canon SDK files found, so an installed Canon utility will be used if present. Put Canon's .zip next to eosvcam.exe or use: install --sdk PATH"
        );
    }

    println!("Registering the media source...");
    let key = format!(r"HKLM\SOFTWARE\Classes\CLSID\{SOURCE_CLSID}\InprocServer32");
    let dll = dir.join(SOURCE_DLL);
    let ok = run(
        "reg.exe",
        &[
            "add",
            &key,
            "/ve",
            "/d",
            &dll.to_string_lossy(),
            "/f",
            "/reg:64",
        ],
    ) && run(
        "reg.exe",
        &[
            "add",
            &key,
            "/v",
            "ThreadingModel",
            "/d",
            "Both",
            "/f",
            "/reg:64",
        ],
    );
    if !ok {
        bail!("could not write the COM registration (run as administrator)");
    }

    secure_data_dir()?;

    println!("Creating the feeder service...");
    let bin = format!("\"\\\"{}\\\" service\"", dir.join("eosvcam.exe").display());
    let created = Command::new("sc.exe")
        .args([
            "create",
            SERVICE_NAME,
            "start=",
            "auto",
            "DisplayName=",
            "EosVCam Feeder",
            "binPath=",
        ])
        .raw_arg(&bin)
        .output()?
        .status
        .success();
    if !created {
        bail!("could not create the service");
    }
    run(
        "sc.exe",
        &[
            "failure",
            SERVICE_NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/5000/restart/5000",
        ],
    );
    if !run("sc.exe", &["start", SERVICE_NAME]) {
        bail!("service did not start; see {DATA_DIR}\\feeder.log");
    }

    println!("Creating the virtual camera...");
    // SAFETY: Start takes no pointer arguments here.
    unsafe { virtual_camera()?.Start(None).ok()? };

    println!("\nDone. Pick \"{CAMERA_NAME}\" as the video device in your app.");
    println!("Check progress with: eosvcam.exe status   (logs are in {DATA_DIR})");
    Ok(())
}

/// Best-effort removal of everything install creates, except the files.
fn remove_all() {
    // SAFETY: Remove is called on a live virtual camera interface.
    match virtual_camera().and_then(|vc| unsafe { vc.Remove() }.ok().map_err(Into::into)) {
        Ok(()) => println!("  virtual camera removed"),
        Err(e) => println!("  no previous virtual camera to remove ({e})"),
    }
    run("sc.exe", &["stop", SERVICE_NAME]);
    // `sc stop` is asynchronous; give the feeder a moment to close the camera.
    thread::sleep(Duration::from_secs(2));
    run("sc.exe", &["delete", SERVICE_NAME]);
    let key = format!(r"HKLM\SOFTWARE\Classes\CLSID\{SOURCE_CLSID}");
    run("reg.exe", &["delete", &key, "/f", "/reg:64"]);
}

pub fn uninstall() -> Result<()> {
    remove_all();
    run("sc.exe", &["stop", "FrameServer"]);
    let dir = install_dir();
    match fs::remove_dir_all(&dir) {
        Ok(()) => println!("  removed {}", dir.display()),
        Err(e) => println!(
            "  could not remove {} ({e}); delete it by hand after a reboot",
            dir.display()
        ),
    }
    println!("Uninstalled. Logs remain in {DATA_DIR}.");
    Ok(())
}

pub fn status() -> Result<()> {
    let dir = install_dir();
    for f in ["eosvcam.exe", SOURCE_DLL, "EDSDK.dll"] {
        println!(
            "{:<20} {}",
            f,
            if dir.join(f).is_file() {
                "installed"
            } else {
                "missing"
            }
        );
    }
    let out = Command::new("sc.exe")
        .args(["query", SERVICE_NAME])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let state = text
        .lines()
        .find(|l| l.contains("STATE"))
        .map_or("not installed", str::trim);
    println!("{:<20} {state}", "service");

    match Mapping::open() {
        Ok(m) => {
            let label = match m.status() {
                status::IDLE => "idle (no app is using the camera)",
                status::NO_CAMERA => "no camera found",
                status::ERROR => "starting or failing, see feeder.log",
                status::LIVE => "live",
                _ => "feeder stopped",
            };
            println!(
                "{:<20} {label}{}",
                "feeder",
                if m.feeder_alive() {
                    ""
                } else {
                    " (no heartbeat)"
                }
            );
            println!("{:<20} {}", "frames published", m.write_seq());
        }
        Err(_) => println!("{:<20} not running", "feeder"),
    }
    println!("logs: {DATA_DIR}");
    Ok(())
}
