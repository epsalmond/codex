@echo off
setlocal
rem Parse the call and exit before extraction replaces this batch file.
(
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0codex-shake-update.ps1"
    call exit /b %%ERRORLEVEL%%
)
