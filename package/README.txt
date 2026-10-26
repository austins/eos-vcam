eos-vcam - virtual camera for Canon EOS cameras
===============================================

Unofficial. Not affiliated with or endorsed by Canon.

What it is
  A virtual camera called "EosVCam" plus a small background service that
  reads the live view from your Canon EOS camera over USB. It shows up in apps as a
  video/webcam device. No other utility needs to run.

Install (Windows 11)
  1. Download Canon's "EOS Webcam Utility Pro" for Windows from Canon's support site and
     put the downloaded .zip in this folder, next to install.bat. Only the camera SDK
     files are taken from it; the utility itself is not installed.
  2. Double-click install.bat and accept the administrator prompt.
  3. Turn the camera on, connect USB, and pick "EosVCam" in your app.

  If Canon's utility is already installed, step 1 can be skipped.
  From an administrator command prompt you can also use: eosvcam.exe install --sdk PATH
  where PATH is Canon's .zip, its .cab, or a folder holding EDSDK.dll and EdsImage.dll.

Check it works
  Double-click check.bat. It runs a self-test, asks the service for frames and writes a
  check-....zip with the results and logs.

Remove
  Double-click uninstall.bat.

Notes
  * Only one program can use the camera through Canon's SDK at a time. Do not run Canon's
    own utility together with this one.
  * The camera only turns on while an app is using the virtual camera.
  * Video is 1280x720 at 30 fps. The live view is cropped to 16:9 and scaled.
  * Logs: C:\ProgramData\EosVCam\ (feeder.log, source.log).
  * "eosvcam.exe status" shows what is installed and what the service is doing.
