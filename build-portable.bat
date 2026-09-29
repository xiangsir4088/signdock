@echo off
REM SignDock - Build Windows portable (green) version
REM
REM Tauri 2 on Windows does NOT have a native portable target --
REM --bundles only accepts {msi, nsis}. So we do it manually:
REM   Step 1: compile release exe (--no-bundle, skips installer wrapping)
REM   Step 2: zip the release directory -- that IS the portable version
REM
REM Output: dist-portable\SignDock-portable-<version>.zip
REM
REM Runtime dependency of the packaged app:
REM   Windows 10 2004+ or Windows 11 (WebView2 Runtime built in).
REM   Older systems: Tauri triggers auto-install of WebView2 on first run.
REM   SQLite is statically bundled into the exe (rusqlite feature = bundled).

setlocal enabledelayedexpansion
cd /d "%~dp0"

echo [SignDock] Building Windows portable (green) version...
echo [SignDock] This is a two-step process; first run may take 5-10 minutes.
echo.

if not exist node_modules (
    echo [SignDock] node_modules missing, running npm install...
    call npm install
)

REM Step 1: compile release binary, skip installer bundling
echo [Step 1/2] Compiling release binary (--no-bundle)...
call npm run tauri build -- --no-bundle
if !errorlevel! neq 0 (
    echo [SignDock] Build failed.
    exit /b 1
)

REM Step 2: assemble the portable zip
echo.
echo [Step 2/2] Assembling portable zip...

set PORTABLE_DIR=dist-portable\SignDock
set ZIP_NAME=dist-portable\SignDock-portable-0.1.1-windows-x64.zip

if exist "%PORTABLE_DIR%" rmdir /s /q "%PORTABLE_DIR%"
if not exist "%PORTABLE_DIR%" mkdir "%PORTABLE_DIR%"

REM Copy the release executable
copy /y src-tauri\target\release\signdock.exe "%PORTABLE_DIR%\SignDock.exe"
if !errorlevel! neq 0 (
    echo [SignDock] Failed to copy signdock.exe
    exit /b 1
)

REM Add a README so anyone who opens the zip knows what to do
(
echo SignDock - Portable (Green) Version
echo ====================================
echo.
echo To run: double-click SignDock.exe in this folder.
echo.
echo The app lives in the system tray. Right-click the tray icon for:
echo   Open settings / Claim now (all) / Quit
echo.
echo Requirements:
echo   - Windows 10 2004+ or Windows 11
echo   - WebView2 Runtime (bundled into OS; older systems auto-install on first run)
echo.
echo No installation required. Safe to delete this folder to uninstall.
echo.
echo License: MIT (see LICENSE in source repo).
) > "%PORTABLE_DIR%\README.txt"

REM Zip the directory (use built-in PowerShell Compress-Archive on Windows 10+)
if exist "%ZIP_NAME%" del /f /q "%ZIP_NAME%"
powershell -NoProfile -NonInteractive -Command ^
    "Compress-Archive -Path '%PORTABLE_DIR%\*' -DestinationPath '%ZIP_NAME%' -CompressionLevel Optimal"
if !errorlevel! neq 0 (
    echo [SignDock] Failed to create zip.
    exit /b 1
)

echo.
echo [SignDock] DONE.
echo           Portable zip: %~dp0%ZIP_NAME%
echo.
echo To distribute: send that .zip to the recipient.
echo They extract it and double-click SignDock.exe. No installer, no admin rights.
endlocal
