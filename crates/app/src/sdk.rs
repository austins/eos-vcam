//! Finds Canon's SDK files (`EDSDK.dll`, `EdsImage.dll`) so the installer can copy them.
//!
//! Where they may come from, first match wins: next to the installer, the path given with
//! `--sdk` (a folder, Canon's `.zip` download or its `.cab`), or Canon's download sitting next to
//! the installer. The cab is read with Windows' own `expand.exe` and the zip with `tar.exe`, so
//! nothing has to be installed first.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const FILES: [&str; 2] = ["EDSDK.dll", "EdsImage.dll"];

#[derive(Debug, PartialEq, Eq)]
pub enum Origin {
    Folder(PathBuf),
    Cab(PathBuf),
    Zip(PathBuf),
}

/// A folder holding the SDK files. Staged files are deleted when this is dropped.
pub struct Located {
    pub dir: PathBuf,
    _temp: Option<TempDir>,
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("eosvcam-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn has_files(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f).is_file())
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Decides what a user-supplied path is.
pub fn classify(path: &Path) -> Option<Origin> {
    if path.is_dir() {
        return Some(Origin::Folder(path.into()));
    }
    match extension(path).as_str() {
        "cab" if path.is_file() => Some(Origin::Cab(path.into())),
        "zip" if path.is_file() => Some(Origin::Zip(path.into())),
        _ => None,
    }
}

/// Canon's download (or its cab, named like the one inside the zip) lying in `dir`. Other zips, such as our own, are ignored.
pub fn find_nearby(dir: &Path) -> Option<Origin> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found.into_iter().find_map(|p| {
        let name = p.file_name()?.to_string_lossy().to_ascii_lowercase();
        match extension(&p).as_str() {
            "cab" if name.starts_with("eos webcam utility") => Some(Origin::Cab(p)),
            "zip" if name.starts_with("eoswebcamutility") => Some(Origin::Zip(p)),
            _ => None,
        }
    })
}

fn expand_args(cab: &Path, file: &str, dest: &Path) -> Vec<OsString> {
    vec![cab.into(), format!("-F:{file}").into(), dest.into()]
}

fn tar_args(zip: &Path, dest: &Path) -> Vec<OsString> {
    vec![
        "-xf".into(),
        zip.into(),
        "-C".into(),
        dest.into(),
        "*.cab".into(),
    ]
}

fn run(program: &str, args: &[OsString]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("could not run {program}"))?;
    if !status.success() {
        bail!("{program} failed ({status})");
    }
    Ok(())
}

fn expand_all(cab: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for f in FILES {
        run("expand.exe", &expand_args(cab, f, dest))
            .with_context(|| format!("extracting {f} from {}", cab.display()))?;
    }
    if !has_files(dest) {
        bail!("{} does not contain {}", cab.display(), FILES.join(" and "));
    }
    Ok(())
}

fn resolve(origin: Origin) -> Result<Located> {
    match origin {
        Origin::Folder(dir) if has_files(&dir) => Ok(Located { dir, _temp: None }),
        Origin::Folder(dir) => match find_nearby(&dir) {
            Some(inner) => resolve(inner),
            None => bail!(
                "{} has no {} and no Canon download",
                dir.display(),
                FILES.join(" or ")
            ),
        },
        Origin::Cab(cab) => {
            let temp = TempDir::new("cab")?;
            expand_all(&cab, &temp.0)?;
            Ok(Located {
                dir: temp.0.clone(),
                _temp: Some(temp),
            })
        }
        Origin::Zip(zip) => {
            let temp = TempDir::new("zip")?;
            run("tar.exe", &tar_args(&zip, &temp.0))
                .with_context(|| format!("reading {}", zip.display()))?;
            let cab = fs::read_dir(&temp.0)?
                .flatten()
                .map(|e| e.path())
                .find(|p| extension(p) == "cab")
                .with_context(|| format!("{} has no .cab inside", zip.display()))?;
            let dir = temp.0.join("sdk");
            expand_all(&cab, &dir)?;
            Ok(Located {
                dir,
                _temp: Some(temp),
            })
        }
    }
}

/// Finds the SDK files for the installer. `Ok(None)` means nothing was offered and the feeder
/// will look for an installed Canon utility instead.
pub fn locate(installer_dir: &Path, requested: Option<&Path>) -> Result<Option<Located>> {
    if has_files(installer_dir) {
        return Ok(Some(Located {
            dir: installer_dir.into(),
            _temp: None,
        }));
    }
    let origin = match requested {
        Some(p) => Some(
            classify(p)
                .with_context(|| format!("{} is not a folder, .zip or .cab", p.display()))?,
        ),
        None => find_nearby(installer_dir),
    };
    origin.map(resolve).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> TempDir {
        TempDir::new(&format!("test-{tag}")).unwrap()
    }

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"x").unwrap();
    }

    #[test]
    fn classify_recognises_folders_cabs_and_zips() {
        let t = scratch("classify");
        touch(&t.0, "a.CAB");
        touch(&t.0, "b.zip");
        touch(&t.0, "c.txt");
        assert_eq!(classify(&t.0), Some(Origin::Folder(t.0.clone())));
        assert_eq!(
            classify(&t.0.join("a.CAB")),
            Some(Origin::Cab(t.0.join("a.CAB")))
        );
        assert_eq!(
            classify(&t.0.join("b.zip")),
            Some(Origin::Zip(t.0.join("b.zip")))
        );
        assert_eq!(classify(&t.0.join("c.txt")), None);
        assert_eq!(classify(&t.0.join("missing.zip")), None);
    }

    #[test]
    fn find_nearby_takes_canons_download_but_not_other_zips() {
        let t = scratch("nearby");
        touch(&t.0, "EosVCam.zip");
        assert_eq!(find_nearby(&t.0), None);
        touch(&t.0, "EOSWebcamUtilityPro-WIN2.3g.zip");
        assert_eq!(
            find_nearby(&t.0),
            Some(Origin::Zip(t.0.join("EOSWebcamUtilityPro-WIN2.3g.zip")))
        );
        touch(&t.0, "unrelated.cab");
        assert_eq!(
            find_nearby(&t.0),
            Some(Origin::Zip(t.0.join("EOSWebcamUtilityPro-WIN2.3g.zip")))
        );
        touch(&t.0, "EOS Webcam Utility1.cab");
        assert_eq!(
            find_nearby(&t.0),
            Some(Origin::Cab(t.0.join("EOS Webcam Utility1.cab")))
        );
    }

    #[test]
    fn locate_prefers_files_next_to_the_installer() {
        let t = scratch("beside");
        for f in FILES {
            touch(&t.0, f);
        }
        touch(&t.0, "EOSWebcamUtilityPro-WIN2.3g.zip");
        assert_eq!(locate(&t.0, None).unwrap().unwrap().dir, t.0);
    }

    #[test]
    fn locate_follows_a_folder_to_the_download_inside_it() {
        let t = scratch("folder");
        let empty = t.0.join("empty");
        fs::create_dir(&empty).unwrap();
        assert!(locate(&t.0, Some(&empty)).is_err());

        let with_sdk = t.0.join("sdk");
        fs::create_dir(&with_sdk).unwrap();
        for f in FILES {
            touch(&with_sdk, f);
        }
        assert_eq!(
            locate(&t.0, Some(&with_sdk)).unwrap().unwrap().dir,
            with_sdk
        );
    }

    #[test]
    fn locate_reports_nothing_found_and_bad_paths() {
        let t = scratch("none");
        assert!(locate(&t.0, None).unwrap().is_none());
        touch(&t.0, "notes.txt");
        assert!(locate(&t.0, Some(&t.0.join("notes.txt"))).is_err());
    }

    #[test]
    fn tool_arguments_have_the_expected_shape() {
        let args = expand_args(Path::new("C:/d/u.cab"), "EDSDK.dll", Path::new("C:/tmp"));
        assert_eq!(
            args,
            ["C:/d/u.cab", "-F:EDSDK.dll", "C:/tmp"].map(OsString::from)
        );
        let args = tar_args(Path::new("C:/d/u.zip"), Path::new("C:/tmp"));
        assert_eq!(
            args,
            ["-xf", "C:/d/u.zip", "-C", "C:/tmp", "*.cab"].map(OsString::from)
        );
    }
}
