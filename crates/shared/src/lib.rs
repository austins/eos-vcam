//! Types shared by the feeder process and the Frame Server media source.

pub mod log;
pub mod shm;
pub mod windows_bindings;

/// CLSID of the media source COM class.
pub const SOURCE_CLSID: &str = "{6F0B9D3A-4C1E-4E53-9A57-3D1C2B7E8A41}";
/// Name shown in camera pickers.
pub const CAMERA_NAME: &str = "EosVCam";
pub const SERVICE_NAME: &str = "EosVCamFeeder";
pub const DATA_DIR: &str = r"C:\ProgramData\EosVCam";
