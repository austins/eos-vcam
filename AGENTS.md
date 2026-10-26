# AGENTS.md

Notes for AI agents and maintainers. User docs are in `README.md`.

## Layout

```
Cargo.toml, Cargo.lock      workspace manifest (shared lints, release profile, dependency versions)
.cargo/config.toml          default target and static CRT settings
crates/
  shared/                   library used by both binaries below
    src/lib.rs              constants: CLSID, camera/service names, data dir
    src/shm.rs              shared-memory ring between feeder and source (fixed 1280x720@30 NV12)
    src/log.rs              size-capped file logging to C:\ProgramData\EosVCam
  source/                   eosvcam_source.dll, loaded by Windows' Frame Server (reader side)
    src/lib.rs              DLL exports and COM class factory
    src/activate.rs         the IMFActivate that Windows uses to create the camera
    src/media.rs            media source, stream and worker thread that serve frames from the ring
  app/                      eosvcam.exe, installer, service and feeder (writer side)
    src/main.rs             command-line entry point and dispatch
    src/install.rs          install, uninstall and status (registry, service, virtual camera)
    src/sdk.rs              finds Canon's SDK DLLs (folder, .zip or .cab) for the installer
    src/service.rs          Windows service wrapper around the feeder
    src/feeder.rs           camera session and decode loop that publishes frames into the ring
    src/edsdk.rs            dynamic binding to Canon's EDSDK.dll
    src/decode.rs           JPEG decode, centre 16:9 crop, scale, NV12 output
    src/convert.rs          BGR to NV12 colour conversion
    src/selftest.rs         camera-free end-to-end check and the `demand` command
package/                    files shipped in the zip: install/uninstall/check .bat and a user README.txt
scripts/                    pack.sh (Linux) and pack.ps1 (Windows) build dist/EosVCam.zip
```

Data flow: Canon camera, USB, EDSDK, `feeder.rs` (decode, convert), shared-memory ring in `shared`, `media.rs` in Frame Server, the app that uses the camera. The two binaries only talk through the ring, and never share code except `shared`.

## Build and test

Develop on either OS; the exact commands (build, test, pack, strict clippy) are in the Build section of `README.md`.

- Wine (Linux) has no `tar.exe`/`expand.exe` or `mfsensorgroup.dll`, so SDK extraction and the sensor-profile calls are only verified on real Windows.
- The Windows MSVC path has not been verified yet.
- `eosvcam.exe selftest 3` (from `target/<target>/release/`) checks the media source end to end without a camera. Run `cargo fmt` and strict clippy before committing.

Lints live in the workspace `Cargo.toml` and are strict: `clippy::all`, `undocumented_unsafe_blocks` and `unsafe_op_in_unsafe_fn` are `deny`. Every `unsafe` block needs a `// SAFETY:` comment.

## CI

`.github/workflows`: `pr.yml` and `main.yml` call `test.yml` (fmt, clippy, tests on `windows-latest`, MSVC target). `deploy.yml` runs on a published release: tests, `scripts/pack.ps1`, then uploads `EosVCam-<version>.zip` (the tag without any leading `v`) plus a `.sha256` and attests build provenance. The tag is validated and passed to the build as `EOSVCAM_VERSION` (shown by `eosvcam.exe version`); local builds report `dev` (the crates have no `version` in `Cargo.toml`). The zip never contains Canon files. None of this has run on GitHub yet.

## Windows 11 virtual camera pitfalls

- Frame Server (LocalService) loads the DLL, so it needs read access to the install dir and write access to `C:\ProgramData\EosVCam`; `install` stops FrameServer so the old DLL unloads.
- The activator forwards `IMFAttributes`; `SetUnknown(null)` is legal and must not return `E_POINTER`.
- The source must publish persistent attributes and sensor profiles (`KSCAMERAPROFILE_Legacy`, `HighFrameRate`) or apps may not enumerate it.
- Names that must stay in sync: CLSID `{6F0B9D3A-4C1E-4E53-9A57-3D1C2B7E8A41}`, service `EosVCamFeeder`, shm objects `EosVCamFrames`/`EosVCamFrameReady`, env `EOSVCAM_NS` (test namespace) and `EOSVCAM_TEST_JPEG`.

## Canon SDK

No Canon files are committed. `EDSDK.dll` and `EdsImage.dll` come from the user's Canon download (see `sdk.rs`) or an installed Canon utility. EDSDK allows one session holder at a time. Never commit Canon binaries or code.

## Invariants

- `Mapping` is `Sync` but `publish` assumes a single writer thread; split a writer type if a second publisher is ever added.
- The feeder service runs as LocalSystem. It must only load `EDSDK.dll` from administrator-writable folders (its own folder or Program Files), and `install` locks `C:\ProgramData\EosVCam` to SYSTEM/Administrators/LocalService write. Do not add user-writable search paths.
