@echo off
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0codex-shake-update.ps1"
exit /b %ERRORLEVEL%
