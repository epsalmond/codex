Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$destination = (Get-Location).Path
$temporaryDirectory = $null

try {
    if (-not (Get-Command gh -CommandType Application -ErrorAction SilentlyContinue)) {
        throw 'Install GitHub CLI (gh) from https://cli.github.com, then run gh auth login.'
    }

    $runJson = & gh run list -R epsalmond/codex --workflow fork-ci.yml --branch eric/local-features --event push --status success --limit 1 --json databaseId,headSha
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not find the Windows build. Check your connection and run gh auth login.'
    }
    try {
        $runMetadata = ($runJson -join "`n") | ConvertFrom-Json
        $runs = @($runMetadata)
    } catch {
        throw 'GitHub CLI returned invalid build metadata.'
    }
    if ($runs.Count -eq 0) {
        throw 'No successful fork-ci.yml push build exists for eric/local-features.'
    }
    if ($runs.Count -ne 1 -or $null -eq $runs[0] -or
        $null -eq $runs[0].PSObject.Properties['headSha'] -or
        $null -eq $runs[0].PSObject.Properties['databaseId'] -or
        $runs[0].headSha -notmatch '^[0-9a-fA-F]{40}$' -or
        [string]$runs[0].databaseId -notmatch '^[1-9][0-9]*$') {
        throw 'GitHub CLI returned invalid build metadata.'
    }

    $run = $runs[0]
    $artifact = "codex-windows-x86_64-pc-windows-msvc-$($run.headSha)"
    $temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "codex-shake-update-$([guid]::NewGuid())"
    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
    & gh run download $run.databaseId -R epsalmond/codex --name $artifact --dir $temporaryDirectory
    if ($LASTEXITCODE -ne 0) {
        throw "Could not download $artifact. Artifacts expire after 14 days; a new successful push build is required if it expired."
    }

    $archives = @(Get-ChildItem -LiteralPath $temporaryDirectory -Filter '*.zip' -File)
    $checksums = @(Get-ChildItem -LiteralPath $temporaryDirectory -Filter '*.zip.sha256' -File)
    if ($archives.Count -ne 1 -or $checksums.Count -ne 1 -or
        $checksums[0].Name -ne "$($archives[0].Name).sha256") {
        throw 'The artifact must contain exactly one ZIP and its matching .zip.sha256 file.'
    }
    $archive = $archives[0]
    $checksum = (Get-Content -LiteralPath $checksums[0].FullName -Raw).Trim()
    $pattern = '^(?<hash>[0-9a-fA-F]{64})\s+\*?' + [regex]::Escape($archive.Name) + '$'
    if ($checksum -notmatch $pattern) {
        throw 'The artifact checksum file is invalid.'
    }
    $expectedHash = $Matches.hash
    if ((Get-FileHash -LiteralPath $archive.FullName -Algorithm SHA256).Hash -ne $expectedHash) {
        throw 'The ZIP checksum does not match. No files were extracted.'
    }

    Write-Host "Extracting build $($run.headSha) into $destination"
    Expand-Archive -LiteralPath $archive.FullName -DestinationPath $destination -Force
    Write-Host 'Updated. Run .\bin\codex.exe --version to check the build.'
} catch {
    [Console]::Error.WriteLine("codex-shake-update: $($_.Exception.Message)")
    exit 1
} finally {
    if ($null -ne $temporaryDirectory) {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}
