@echo off
rem Start PencariMovie Server (tray + server) when this user logs in.
rem Registers a per-user HKCU Run entry - no administrator rights needed.
rem The entry points at the BINARY with --tray; the tray is part of the exe, so
rem there is no script to keep in place for the login start.
setlocal
cd /d "%~dp0"
if "%PORT%"=="" set PORT=8088
set "RUNKEY=HKCU\Software\Microsoft\Windows\CurrentVersion\Run"
set "NAME=PencariMovie Server"
set "CMD="%~dp0pencarimovie-server.exe" --tray --port %PORT%"
reg add "%RUNKEY%" /v "%NAME%" /t REG_SZ /d "%CMD%" /f >nul
if errorlevel 1 (
  echo Could not register the login entry.
  exit /b 1
)
echo Registered: "%NAME%" will start at login.
echo   command: %CMD%
