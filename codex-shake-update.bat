@echo off
setlocal
rem Use Windows PowerShell's default modules when called from pwsh.
set "PSModulePath="
rem Parse the call and exit before extraction replaces this batch file.
(
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0codex-shake-update.ps1"
    call exit /b %%ERRORLEVEL%%
)
