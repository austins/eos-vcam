#!/bin/sh
# Cross-builds eos-vcam and packages dist/EosVCam.zip for Windows (needs mingw-w64 and zip).
# Canon's SDK files are never bundled by default; set SDK_DIR to a folder holding
# EDSDK.dll and EdsImage.dll to bundle them into a private build.
set -e
cd "$(dirname "$0")/.."
D=dist/EosVCam
T=target/x86_64-pc-windows-gnu/release

cargo build --release
rm -rf "$D" && mkdir -p "$D"
cp "$T/eosvcam.exe" "$T/eosvcam_source.dll" "$D/"
for f in install.bat uninstall.bat check.bat README.txt; do sed 's/$/\r/' "package/$f" > "$D/$f"; done
if [ -n "$SDK_DIR" ]; then
  cp "$SDK_DIR/EDSDK.dll" "$SDK_DIR/EdsImage.dll" "$D/"
fi
(cd dist && rm -f EosVCam.zip && zip -qr EosVCam.zip EosVCam)
ls -la dist/EosVCam.zip
