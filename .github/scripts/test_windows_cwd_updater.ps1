param([string]$Archive)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repository = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$testDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "codex updater test $([guid]::NewGuid())"

try {
    $scripts = New-Item -ItemType Directory -Path (Join-Path $testDirectory 'scripts with spaces')
    $mockBin = New-Item -ItemType Directory -Path (Join-Path $testDirectory 'mock bin')
    Copy-Item (Join-Path $repository 'codex-shake-update.bat') $scripts.FullName
    Copy-Item (Join-Path $repository 'codex-shake-update.ps1') $scripts.FullName
    @'
@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0mock-gh.ps1" %*
exit /b %ERRORLEVEL%
'@ | Set-Content -LiteralPath (Join-Path $mockBin.FullName 'gh.cmd') -Encoding ASCII
    @'
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($env:UPDATER_TEST_MODE -eq 'list-failure') { exit 23 }
if ($args[0] -eq 'run' -and $args[1] -eq 'list') {
    $expected = 'run list -R epsalmond/codex --workflow fork-ci.yml --branch eric/local-features --event push --status success --limit 1 --json databaseId,headSha'
    if (($args -join ' ') -ne $expected) { throw "Unexpected gh arguments: $args" }
    if ($env:UPDATER_TEST_MODE -eq 'invalid-metadata') { '[{"databaseId":123}]'; exit 0 }
    if ($env:UPDATER_TEST_MODE -eq 'invalid-json') { 'not JSON'; exit 0 }
    if ($env:UPDATER_TEST_MODE -eq 'no-build') { '[]'; exit 0 }
    '[{"databaseId":123,"headSha":"1111111111111111111111111111111111111111"}]'
    exit 0
}
if ($args[0] -ne 'run' -or $args[1] -ne 'download') { throw 'Unexpected gh command' }
$expected = 'run download 123 -R epsalmond/codex --name codex-windows-x86_64-pc-windows-msvc-1111111111111111111111111111111111111111 --dir '
if (-not (($args -join ' ').StartsWith($expected))) { throw "Unexpected gh arguments: $args" }
if ($env:UPDATER_TEST_MODE -eq 'download-failure') { exit 24 }
$download = $args[-1]
$zip = Join-Path $download 'codex-test.zip'
if ($env:UPDATER_TEST_ARCHIVE -and $env:UPDATER_TEST_MODE -notin @('lf-to-crlf', 'crlf-to-lf')) {
    Copy-Item -LiteralPath $env:UPDATER_TEST_ARCHIVE -Destination $zip
} else {
    $payload = New-Item -ItemType Directory -Path (Join-Path $download 'payload')
    'updated' | Set-Content -LiteralPath (Join-Path $payload.FullName 'installed.txt')
    if ($env:UPDATER_TEST_MODE -in @('lf-to-crlf', 'crlf-to-lf')) {
        Copy-Item (Join-Path $env:UPDATER_TEST_SOURCE 'codex-shake-update.ps1') $payload.FullName
        $batch = [System.IO.File]::ReadAllText((Join-Path $env:UPDATER_TEST_SOURCE 'codex-shake-update.bat')).Replace("`r`n", "`n")
        if ($env:UPDATER_TEST_MODE -eq 'lf-to-crlf') { $batch = $batch.Replace("`n", "`r`n") }
        [System.IO.File]::WriteAllText((Join-Path $payload.FullName 'codex-shake-update.bat'), $batch, [System.Text.Encoding]::ASCII)
    }
    Compress-Archive -Path (Join-Path $payload.FullName '*') -DestinationPath $zip
    Remove-Item -LiteralPath $payload.FullName -Recurse -Force
}
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash
if ($env:UPDATER_TEST_MODE -eq 'bad-checksum') { $hash = '0' * 64 }
"$hash  codex-test.zip" | Set-Content -LiteralPath "$zip.sha256" -Encoding ASCII
exit 0
'@ | Set-Content -LiteralPath (Join-Path $mockBin.FullName 'mock-gh.ps1') -Encoding ASCII

    foreach ($mode in @('success', 'bad-checksum', 'list-failure', 'download-failure', 'invalid-metadata', 'invalid-json', 'no-build', 'lf-to-crlf', 'crlf-to-lf')) {
        $destination = New-Item -ItemType Directory -Path (Join-Path $testDirectory "destination with spaces $mode")
        $sentinel = Join-Path $destination.FullName 'installed.txt'
        'original' | Set-Content -LiteralPath $sentinel
        $start = New-Object System.Diagnostics.ProcessStartInfo
        $start.FileName = $env:ComSpec
        $start.Arguments = '/d /c ""' + (Join-Path $scripts.FullName 'codex-shake-update.bat') + '""'
        if ($mode -in @('lf-to-crlf', 'crlf-to-lf')) {
            Copy-Item (Join-Path $scripts.FullName 'codex-shake-update.*') $destination.FullName
            $batchPath = Join-Path $destination.FullName 'codex-shake-update.bat'
            $batch = [System.IO.File]::ReadAllText($batchPath).Replace("`r`n", "`n")
            if ($mode -eq 'crlf-to-lf') { $batch = $batch.Replace("`n", "`r`n") }
            [System.IO.File]::WriteAllText($batchPath, $batch, [System.Text.Encoding]::ASCII)
            $start.Arguments = '/d /c ""' + $batchPath + '""'
        }
        $start.WorkingDirectory = $destination.FullName
        $start.UseShellExecute = $false
        $start.RedirectStandardOutput = $true
        $start.RedirectStandardError = $true
        $start.EnvironmentVariables['PATH'] = $mockBin.FullName + ';' + $env:PATH
        $start.EnvironmentVariables['UPDATER_TEST_MODE'] = $mode
        $start.EnvironmentVariables['UPDATER_TEST_ARCHIVE'] = $Archive
        $start.EnvironmentVariables['UPDATER_TEST_SOURCE'] = $scripts.FullName
        $start.EnvironmentVariables['TEMP'] = $testDirectory
        $start.EnvironmentVariables['TMP'] = $testDirectory
        $process = [System.Diagnostics.Process]::Start($start)
        $stdout = $process.StandardOutput.ReadToEnd()
        $stderr = $process.StandardError.ReadToEnd()
        $process.WaitForExit()
        if ($mode -in @('success', 'lf-to-crlf', 'crlf-to-lf')) {
            if ($process.ExitCode -ne 0 -or $stderr) { throw "Success test failed: $stdout $stderr" }
            if ($Archive -and $mode -eq 'success') {
                & (Join-Path $destination.FullName 'bin/codex.exe') --version
                if ($LASTEXITCODE -ne 0) { throw 'Extracted Codex could not run' }
            } elseif ((Get-Content -LiteralPath $sentinel -Raw).Trim() -ne 'updated') {
                throw 'The updater did not extract into the caller directory'
            }
        } else {
            if ($process.ExitCode -eq 0) { throw "$mode did not fail through the batch wrapper" }
            $files = @(Get-ChildItem -LiteralPath $destination.FullName -Recurse -File)
            if ($files.Count -ne 1 -or (Get-Content -LiteralPath $sentinel -Raw).Trim() -ne 'original') {
                throw "$mode modified the destination"
            }
            $expectedError = switch ($mode) {
                'bad-checksum' { 'checksum does not match' }
                'list-failure' { 'gh auth login' }
                'download-failure' { 'expire after 14 days' }
                'invalid-metadata' { 'invalid build metadata' }
                'invalid-json' { 'invalid build metadata' }
                'no-build' { 'No successful fork-ci.yml push build' }
            }
            if (-not $stderr.Contains($expectedError)) { throw "$mode returned an unexpected error: $stderr" }
        }
        if (@(Get-ChildItem -LiteralPath $testDirectory -Directory -Filter 'codex-shake-update-*').Count -ne 0) {
            throw "$mode left download files behind"
        }
        Write-Host "PASS: $mode"
    }
    if (Test-Path -LiteralPath (Join-Path $scripts.FullName 'installed.txt')) {
        throw 'The updater extracted into its script directory'
    }
} finally {
    Remove-Item -LiteralPath $testDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
