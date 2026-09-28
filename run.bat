@echo off
REM SignDock - Quick launch (debug build)
REM Double-click to start; tray icon appears in ~2 seconds.

setlocal
cd /d "%~dp0"

set EXE=src-tauri\target\debug\signdock.exe

if not exist "%EXE%" (
    echo [SignDock] Debug binary not found.
    echo Run "dev.bat" first to compile, or run "build.bat" for release.
    pause
    exit /b 1
)

echo [SignDock] Starting %EXE%
echo [SignDock] Tray icon will appear in the system tray.
start "" "%EXE%"

REM This line keeps the console alive long enough to see the "Starting" message.
timeout /t 1 >nul
endlocal
