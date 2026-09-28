@echo off
REM SignDock - Build release installer (MSI/NSIS/exe)
REM Output: src-tauri\tarball\bundle\

setlocal
cd /d "%~dp0"

echo [SignDock] Building release installer...
echo [SignDock] This takes a few minutes on first run.

if not exist node_modules (
    echo [SignDock] node_modules missing, running npm install...
    call npm install
)

call npm run tauri build

echo.
echo [SignDock] Build done. Installer is in:
echo           %~dp0src-tauri\target\release\bundle\
endlocal
