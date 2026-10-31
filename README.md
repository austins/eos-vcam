# eos-vcam

A virtual camera for Canon EOS cameras on Windows 11. It shows up in apps as a video/webcam device named **EosVCam** and streams the camera's USB live view at 1280x720, 30 fps.

Unofficial: not affiliated with or endorsed by Canon. "Canon" and "EOS" are Canon's trademarks.

## What it improves

Canon's utility stutters on a Canon 600D (T3i). Based on its observed behavior, eos-vcam replaces its video path and fixes three causes:

1. Polling stall. Canon's service waited 60 ms whenever a USB frame wasn't ready. The 600D sends a frame every ~35 ms, so a 1 ms miss cost 60 ms, giving about 23 fps with ~20% duplicate frames. eos-vcam polls every 1 ms and delivers every frame.
2. Repeated frames. Canon re-sent the old frame every 33 ms when no new one had arrived. eos-vcam waits for the next real frame and uses its true capture time.
3. Pipeline overhead. Canon used several background processes and a UI app, converted JPEG to RGB to YUY2/NV12, and reopened shared memory for every frame. eos-vcam is one service and one DLL, converts once to NV12, and uses a persistent shared-memory ring. The camera is off while no app uses it.

The camera's live-view rate is the limit (about 28.5 fps on a 600D).

## Install

1. Download Canon's EOS Webcam Utility Pro for Windows from Canon's support site and put the `.zip` next to `install.bat`. It isn't installed; only its camera SDK files are used.
2. Run `install.bat` and accept the administrator prompt.
3. Turn the camera on, connect it over USB, and choose EosVCam in your app.

You can also keep Canon's utility installed, or run `eosvcam.exe install --sdk PATH` from an administrator prompt (`PATH` is Canon's `.zip`, its `.cab`, or a folder with `EDSDK.dll` and `EdsImage.dll`).

`uninstall.bat` removes it. `check.bat` runs a self-test and collects logs (`C:\ProgramData\EosVCam\`) for bug reports. Only one program can hold the camera through Canon's SDK, so don't run Canon's utility at the same time.

## How it works

- `eosvcam_source.dll`: a Media Foundation virtual camera source loaded by Windows' Frame Server.
- `eosvcam.exe`: installer, plus a Windows service that reads live-view JPEGs through Canon's EDSDK, converts them to NV12 and writes them to a shared-memory ring. The camera starts only while an app uses it.

Code layout: [AGENTS.md](AGENTS.md).

## Build

Outputs are `eosvcam.exe` and `eosvcam_source.dll`. `.cargo/config.toml` defaults to the `x86_64-pc-windows-gnu` target (Linux cross builds), so on Windows always pass `--target x86_64-pc-windows-msvc`.

**Windows** (Rust MSVC toolchain with `rustfmt`, and Visual Studio Build Tools; with rustup, run `rustup target add x86_64-pc-windows-msvc` once):

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo test --workspace --target x86_64-pc-windows-msvc
scripts\pack.ps1            # dist\EosVCam.zip
```

**Linux** (cross-compile; needs the `x86_64-pc-windows-gnu` Rust standard library, mingw-w64, `zip` and Wine for tests; with rustup, `rustup target add x86_64-pc-windows-gnu`). Developed on Fedora with `rust-std-static-x86_64-pc-windows-gnu`, `rustfmt`, `mingw64-gcc`, `wine` and `zip`:

```sh
cargo build --release
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine cargo test --workspace
scripts/pack.sh             # dist/EosVCam.zip
```

Clippy is strict by default (the workspace `Cargo.toml` denies `clippy::all` and undocumented `unsafe`). CI also runs `cargo clippy --workspace --all-targets -- -D warnings`; add `--target x86_64-pc-windows-msvc` on Windows.

The build generates Win32 bindings with `windows-bindgen`, which needs `rustfmt` on the `PATH`.

This repository contains no Canon files. Set `SDK_DIR` for `scripts/pack.sh` only for private builds.

## Limits

* Windows 11 only. Video is fixed at 1280x720, 30 fps, and the camera's own live-view rate applies.
* Tested on a Canon 600D (T3i) on Windows 11 with the SDK files from EOS Webcam Utility Pro 2.3g (`EOSWebcamUtilityPro-WIN2.3g.zip`), and developed on Linux (cross-compiled, unit-tested under Wine). Other EDSDK cameras should work. To report results, install, turn the camera on, run `check.bat` and send the zip it creates.
* Other resolutions are not implemented. If your camera reports a live-view size other than the 600D's, please send that `check.bat` zip, whose log records the camera model and live-view size.

## License

[MIT](LICENSE). Canon's SDK is subject to Canon's own license terms.
