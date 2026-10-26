@echo off
setlocal
cd /d "%~dp0"
for /f %%i in ('powershell -NoProfile -Command "Get-Date -Format yyyyMMdd-HHmmss"') do set TS=%%i
set OUT=%~dp0check-%TS%
mkdir "%OUT%" 2>nul

echo.
echo  eos-vcam check
echo  --------------
echo  Camera on, connected by USB. Close any app that uses the camera first.
echo.
pause

(
  eosvcam.exe version
  echo.
  echo ---- selftest ----
  eosvcam.exe selftest
  echo.
  echo ---- status ----
  eosvcam.exe status
) > "%OUT%\summary.txt" 2>&1

echo  Asking the feeder for frames for 10 seconds...
eosvcam.exe demand 10 >> "%OUT%\summary.txt" 2>&1

copy "C:\ProgramData\EosVCam\*.log" "%OUT%" >nul 2>&1
type "%OUT%\summary.txt"
powershell -NoProfile -Command "Compress-Archive -Force -Path ($env:OUT + '\*') -DestinationPath ($env:OUT + '.zip')"
echo.
echo  Done. Attach this file when reporting a problem:
echo    %OUT%.zip
echo.
pause
