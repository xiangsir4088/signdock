@echo off
REM SignDock - Development mode (Vite HMR + Tauri)
REM Requires: Node.js 22+, Rust stable, Tauri 2 prerequisites.

setlocal
cd /d "%~dp0"

echo [SignDock] Starting dev mode (Vite + Tauri hot reload)
echo [SignDock] Press Ctrl+C in this window to stop.

if not exist node_modules (
    echo [SignDock] node_modules missing, running npm install...
    call npm install
)

call npm run tauri dev
endlocal
