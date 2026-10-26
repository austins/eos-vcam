@echo off
cd /d "%~dp0"
net session >nul 2>&1
if errorlevel 1 (
  echo Asking for administrator permission...
  set "SELF=%~f0"
  powershell -NoProfile -Command "Start-Process -FilePath $env:SELF -Verb RunAs"
  exit /b
)
echo.
echo  Removing the EosVCam virtual camera...
echo.
eosvcam.exe uninstall
echo.
pause
