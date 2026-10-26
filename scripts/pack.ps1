# Builds eos-vcam on Windows (Rust MSVC toolchain) and packages dist\EosVCam.zip.
# Set $env:SDK_DIR to a folder holding EDSDK.dll and EdsImage.dll to bundle Canon's files
# into a private build; they are not bundled by default.
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$target = 'x86_64-pc-windows-msvc'
$out = "target\$target\release"

cargo build --release --target $target
if ($LASTEXITCODE) { throw 'build failed' }

$d = 'dist\EosVCam'
Remove-Item -Recurse -Force $d -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $d | Out-Null
Copy-Item "$out\eosvcam.exe", "$out\eosvcam_source.dll" $d
# .NET resolves relative paths against the process directory, not Set-Location.
$abs = (Resolve-Path $d).Path
foreach ($f in Get-ChildItem package) {
  $text = (Get-Content -Raw $f.FullName) -replace "`r?`n", "`r`n"
  [IO.File]::WriteAllText((Join-Path $abs $f.Name), $text)
}
if ($env:SDK_DIR) {
  Copy-Item "$env:SDK_DIR\EDSDK.dll", "$env:SDK_DIR\EdsImage.dll" $d
}
Compress-Archive -Force -Path $d -DestinationPath dist\EosVCam.zip
Get-Item dist\EosVCam.zip
